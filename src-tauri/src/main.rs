// No console window on Windows release builds (harmless elsewhere).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod settings;
mod state;
mod tray;

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pb_core::adb::Adb;
use pb_core::connection::{self, ConnectionConfig};
use pb_core::overlay::{NetManager, NetState};
use pb_core::procs::{self, ProcEvent, ProcessRegistry};
use pb_core::scrcpy::Scrcpy;
use pb_core::tools::BundledTools;
use pb_core::{AppError, ErrorCode};
use tauri::{AppHandle, Emitter, Manager, RunEvent, WindowEvent};
use tauri_plugin_opener::OpenerExt;

use crate::settings::SettingsStore;
use crate::state::{AppState, SharedState};

/// Send an error to the UI (toast) and the log. Nothing fails silently.
pub fn report_error(app: &AppHandle, e: AppError) {
    tracing::error!("{e} {}", e.hint.as_deref().unwrap_or(""));
    let _ = app.emit("app-error", &e);
}

fn init_logging(log_dir: &Path) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::prelude::*;
    let filter =
        tracing_subscriber::EnvFilter::try_from_env("PHONEBRIDGE_LOG").unwrap_or_else(|_| {
            "info,netbird=info,tailscale=info,scrcpy=info,tao=warn,wry=warn".into()
        });
    let stderr = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_writer(std::io::stderr);
    match std::fs::create_dir_all(log_dir) {
        Ok(()) => {
            let file = tracing_appender::rolling::daily(log_dir, "phonebridge.log");
            let (writer, guard) = tracing_appender::non_blocking(file);
            let file_layer = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(writer);
            tracing_subscriber::registry()
                .with(filter)
                .with(stderr)
                .with(file_layer)
                .init();
            Some(guard)
        }
        Err(_) => {
            tracing_subscriber::registry()
                .with(filter)
                .with(stderr)
                .init();
            None
        }
    }
}

/// Free loopback port for our private adb server (chosen once, then saved).
fn pick_adb_port() -> u16 {
    for _ in 0..50 {
        if let Ok(l) = std::net::TcpListener::bind(("127.0.0.1", 0))
            && let Ok(a) = l.local_addr()
        {
            let p = a.port();
            if (20000..60000).contains(&p) {
                return p;
            }
        }
    }
    25037
}

fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let handle = app.handle().clone();
    let paths = app.path();
    let data_dir = paths.app_data_dir()?;
    let log_dir = paths.app_log_dir()?;
    let resource_dir = paths.resource_dir()?;
    let guard = init_logging(&log_dir);
    // Keep the log writer alive for the whole process.
    app.manage(Mutex::new(guard));
    tracing::info!(
        "PhoneBridge {} starting; data {}",
        env!("CARGO_PKG_VERSION"),
        data_dir.display()
    );

    let settings = SettingsStore::load(&handle);
    if settings.get().adb_server_port == 0 {
        let port = pick_adb_port();
        if let Err(e) = settings.update(|s| s.adb_server_port = port) {
            tracing::error!("saving adb port: {e}");
        }
    }
    let s = settings.get();

    // Bundled binaries live next to our executable (Contents/MacOS); data files in Resources.
    let exe_dir: PathBuf = std::env::current_exe()?
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let tools = BundledTools::new(&exe_dir, &resource_dir, s.adb_server_port);
    let tools_error = tools.check().err();
    if let Some(e) = &tools_error {
        tracing::error!("{e}");
    }
    tracing::info!(
        "adb: {} | scrcpy: {} | server: {}",
        tools.adb.display(),
        tools.scrcpy.display(),
        tools.scrcpy_server.display()
    );

    let pid_file = data_dir.join("children.json");
    let killed = procs::cleanup_stale(&pid_file);
    if killed > 0 {
        tracing::warn!("cleaned up {killed} scrcpy process(es) left from a previous run");
    }

    let adb = Adb::new(tools.clone());
    let scrcpy = Scrcpy::new(tools.clone());
    // Each network keeps its login in its own folder: <data>/netbird, <data>/tailscale.
    let net = NetManager::new(
        &data_dir,
        pb_core::DEFAULT_HOSTNAME,
        s.network,
        s.server_url(s.network),
    );
    let procs = ProcessRegistry::new(Some(pid_file), &tools.scrcpy);

    // Tauri's async runtime is Tokio; spawn the connection actor on it.
    let conn = tauri::async_runtime::block_on(async {
        connection::spawn(
            adb.clone(),
            net.dialer(),
            net.subscribe(),
            ConnectionConfig {
                forwarder_port: s.forwarder_port,
                ..Default::default()
            },
            s.phone.clone(),
        )
    });

    let state: SharedState = Arc::new(AppState {
        tools,
        tools_error: tools_error.clone(),
        adb,
        scrcpy,
        net: net.clone(),
        conn,
        procs,
        settings,
        log_dir,
        data_dir,
        shutting_down: Default::default(),
        opened_auth_url: Mutex::new(None),
    });
    app.manage(state.clone());

    tray::create(&handle)?;
    spawn_pumps(&handle, &state);

    // Start our adb server fresh (a crashed run may have left one), then the network.
    // First run (no saved login): start a browser login right away; the pump
    // below opens it.
    let st = state.clone();
    let h = handle.clone();
    tauri::async_runtime::spawn(async move {
        let _ = st.adb.kill_server().await;
        if let Err(e) = st.net.start().await {
            report_error(&h, e);
        }
        if !st.net.has_saved_login()
            && let Err(e) = st.net.login_browser().await
        {
            report_error(&h, e);
        }
    });

    if let Some(e) = tools_error {
        report_error(&handle, e);
    }

    // Closing the window keeps PhoneBridge running in the menu bar.
    if let Some(w) = app.get_webview_window("main") {
        let w2 = w.clone();
        w.on_window_event(move |ev| {
            if let WindowEvent::CloseRequested { api, .. } = ev {
                api.prevent_close();
                let _ = w2.hide();
            }
        });
    }
    Ok(())
}

/// Forward core state changes to the UI and the tray.
fn spawn_pumps(app: &AppHandle, state: &SharedState) {
    // Network status.
    let (h, st) = (app.clone(), state.clone());
    tauri::async_runtime::spawn(async move {
        let mut rx = st.net.subscribe();
        loop {
            let snap = rx.borrow_and_update().clone();
            // A browser login started: open it (once per URL).
            if let NetState::NeedsLogin {
                auth_url: Some(url),
                ..
            } = &snap.state
            {
                let mut opened = st.opened_auth_url.lock().unwrap();
                if opened.as_deref() != Some(url.as_str()) {
                    *opened = Some(url.clone());
                    tracing::info!("opening {} login page", snap.kind);
                    if let Err(e) = h.opener().open_url(url, None::<&str>) {
                        report_error(
                            &h,
                            AppError::new(ErrorCode::Io, format!("Couldn't open the browser: {e}"))
                                .hint(url.clone()),
                        );
                    }
                }
            }
            let _ = h.emit("net", &snap);
            if rx.changed().await.is_err() {
                return;
            }
        }
    });

    // Connection status → UI, tray, and persist forwarder port / phone IP changes.
    let (h, st) = (app.clone(), state.clone());
    tauri::async_runtime::spawn(async move {
        let mut rx = st.conn.subscribe();
        loop {
            let status = rx.borrow_and_update().clone();
            let _ = h.emit("connection", &status);
            tray::refresh(&h);
            let port = st.conn.forwarder_port();
            let phone = st.conn.phone();
            let saved = st.settings.get();
            if (port.is_some() && port != saved.forwarder_port)
                || (phone.is_some() && phone != saved.phone)
            {
                let res = st.settings.update(|s| {
                    if port.is_some() {
                        s.forwarder_port = port;
                    }
                    if phone.is_some() {
                        s.phone = phone.clone();
                    }
                });
                match res {
                    Ok(s) => {
                        let _ = h.emit("settings", &s);
                    }
                    Err(e) => report_error(&h, AppError::new(ErrorCode::Io, e)),
                }
            }
            if !status.is_connected()
                && !st.procs.list().is_empty()
                && !st.shutting_down.load(Ordering::Acquire)
            {
                // scrcpy windows die with the adb transport anyway; their exit is reported below.
                tracing::info!("connection lost with windows open");
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    });

    // Window processes → UI; failed windows become visible errors.
    let (h, st) = (app.clone(), state.clone());
    tauri::async_runtime::spawn(async move {
        let mut rx = st.procs.subscribe();
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    if let ProcEvent::Exited {
                        window,
                        success,
                        requested,
                        stderr_tail,
                        code,
                    } = &ev
                        && !success
                        && !requested
                        && !st.shutting_down.load(Ordering::Acquire)
                    {
                        tracing::error!(
                            "{} window exited ({code:?}); scrcpy said:\n{}",
                            window.label,
                            stderr_tail.join("\n")
                        );
                        let (reason, details) = pb_core::scrcpy::explain_exit(stderr_tail, *code);
                        let mut err = AppError::new(
                            ErrorCode::Scrcpy,
                            format!("{} window closed: {reason}", window.label),
                        );
                        if !details.is_empty() {
                            err = err.hint(details.join(" · "));
                        }
                        report_error(&h, err);
                    }
                    let _ = h.emit("windows", st.procs.list());
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    let _ = h.emit("windows", st.procs.list());
                }
                Err(_) => return,
            }
        }
    });
}

/// Clean up, then exit. Used by the tray's Quit so cleanup never depends on
/// whether a programmatic exit can be intercepted in `RunEvent::ExitRequested`.
pub fn quit(app: &AppHandle) {
    let Some(state) = app.try_state::<SharedState>().map(|s| s.inner().clone()) else {
        app.exit(0);
        return;
    };
    if state.shutting_down.swap(true, Ordering::AcqRel) {
        return; // already shutting down
    }
    let h = app.clone();
    tauri::async_runtime::spawn(async move {
        let _ = tokio::time::timeout(Duration::from_secs(12), shutdown(state)).await;
        h.exit(0);
    });
}

/// Close windows, disconnect adb, stop our adb server and forwarder, then the network.
async fn shutdown(state: SharedState) {
    tracing::info!("shutting down");
    state.procs.terminate_all(Duration::from_secs(2)).await;
    state.conn.shutdown().await;
    state.net.shutdown().await;
    tracing::info!("shutdown complete");
}

fn main() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            tray::show_main(app)
        }))
        .plugin(tauri_plugin_store::Builder::new().build())
        .plugin(tauri_plugin_opener::init())
        .setup(setup)
        .invoke_handler(tauri::generate_handler![
            commands::get_overview,
            commands::net_login_browser,
            commands::net_open_login,
            commands::net_login_setup_key,
            commands::net_retry,
            commands::net_logout,
            commands::net_set_management_url,
            commands::net_set_network,
            commands::usb_detect,
            commands::usb_setup,
            commands::usb_reenable,
            commands::forget_phone,
            commands::connect_now,
            commands::disconnect,
            commands::list_apps,
            commands::open_app,
            commands::open_mirror,
            commands::close_window,
            commands::set_favorite,
            commands::set_preset,
            commands::set_window_options,
            commands::diagnostics,
            commands::open_logs,
        ])
        .build(tauri::generate_context!())
        .expect("error while building PhoneBridge");

    app.run(|app, event| match event {
        RunEvent::ExitRequested { api, .. } => {
            let Some(state) = app.try_state::<SharedState>().map(|s| s.inner().clone()) else {
                return;
            };
            if state.shutting_down.swap(true, Ordering::AcqRel) {
                return; // second pass: cleanup done, let it exit
            }
            api.prevent_exit();
            let h = app.clone();
            tauri::async_runtime::spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(12), shutdown(state)).await;
                h.exit(0);
            });
        }
        #[cfg(target_os = "macos")]
        RunEvent::Reopen { .. } => tray::show_main(app),
        _ => {}
    });
}
