//! Commands invoked from the UI (`window.__TAURI__.core.invoke`). Every one
//! returns `Result<_, AppError>` so the UI can show what went wrong.

use std::time::Duration;

use pb_core::connection::{ConnectionStatus, PhoneConfig};
use pb_core::forwarder::ForwarderStats;
use pb_core::overlay::{NetSnapshot, NetState, Peer, SsoFlow};
use pb_core::presets::QualityPreset;
use pb_core::procs::WindowInfo;
use pb_core::scrcpy::{AndroidApp, LaunchMode, WindowOptions, window_args};
use pb_core::setup::{self, UsbPhone};
use pb_core::{AppError, ErrorCode};
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_opener::OpenerExt;

use crate::settings::Settings;
use crate::state::SharedState;

type Res<T> = Result<T, AppError>;

fn settings_err(e: String) -> AppError {
    AppError::new(ErrorCode::Io, e)
}

#[derive(Serialize)]
pub struct Overview {
    pub net: NetSnapshot,
    pub connection: ConnectionStatus,
    pub phone: Option<PhoneConfig>,
    pub settings: Settings,
    pub windows: Vec<WindowInfo>,
    pub device_name: String,
    pub management_url: String,
    pub tools_error: Option<AppError>,
}

#[tauri::command]
pub fn get_overview(state: State<'_, SharedState>) -> Overview {
    Overview {
        net: state.net.snapshot(),
        connection: state.conn.status(),
        phone: state.conn.phone(),
        settings: state.settings.get(),
        windows: state.procs.list(),
        device_name: state.net.device_name().to_string(),
        management_url: state.net.management_url(),
        tools_error: state.tools_error.clone(),
    }
}

// ---- NetBird ---------------------------------------------------------------

pub fn open_url(app: &AppHandle, state: &SharedState, url: &str) -> Res<()> {
    *state.opened_auth_url.lock().unwrap() = Some(url.to_string());
    app.opener().open_url(url, None::<&str>).map_err(|e| {
        AppError::new(ErrorCode::Io, format!("Couldn't open the browser: {e}"))
            .hint(url.to_string())
    })
}

/// Start a browser (SSO) login. The status pump opens the URL in the browser.
#[tauri::command]
pub async fn net_login_browser(state: State<'_, SharedState>) -> Res<SsoFlow> {
    state.net.login_browser().await
}

/// Open the pending login page again.
#[tauri::command]
pub fn net_open_login(app: AppHandle, state: State<'_, SharedState>) -> Res<()> {
    match state.net.snapshot().state {
        NetState::NeedsLogin {
            auth_url: Some(url),
            ..
        } => open_url(&app, &state, &url),
        _ => Err(AppError::new(ErrorCode::Network, "No login is waiting")
            .hint("Click \"Sign in with browser\".")),
    }
}

#[tauri::command]
pub async fn net_login_setup_key(state: State<'_, SharedState>, key: String) -> Res<()> {
    state.net.login_with_setup_key(key).await
}

#[tauri::command]
pub async fn net_retry(state: State<'_, SharedState>) -> Res<()> {
    state.net.retry().await
}

#[tauri::command]
pub async fn net_logout(state: State<'_, SharedState>) -> Res<()> {
    state.procs.terminate_all(Duration::from_secs(2)).await;
    *state.opened_auth_url.lock().unwrap() = None;
    state.net.logout().await
}

/// Use a self-hosted NetBird server (logs out if the URL changes).
#[tauri::command]
pub async fn net_set_management_url(
    app: AppHandle,
    state: State<'_, SharedState>,
    url: String,
) -> Res<String> {
    state.procs.terminate_all(Duration::from_secs(2)).await;
    state.net.set_management_url(url).await?;
    let url = state.net.management_url();
    let s = state
        .settings
        .update(|s| s.management_url = url.clone())
        .map_err(settings_err)?;
    let _ = app.emit("settings", &s);
    Ok(url)
}

// ---- Phone setup -----------------------------------------------------------

#[derive(Serialize)]
pub struct UsbDetect {
    pub phone: UsbPhone,
    /// NetBird peer whose IP matches one of the phone's addresses.
    pub suggested_peer_id: Option<String>,
    /// Peers to choose from (Android first).
    pub peers: Vec<Peer>,
}

fn require_net(state: &SharedState) -> Res<NetSnapshot> {
    let snap = state.net.snapshot();
    if snap.is_running() {
        Ok(snap)
    } else {
        Err(AppError::new(
            ErrorCode::NotLoggedIn,
            "PhoneBridge isn't connected to NetBird yet",
        )
        .hint("Log in to NetBird first."))
    }
}

#[tauri::command]
pub async fn usb_detect(state: State<'_, SharedState>) -> Res<UsbDetect> {
    let mut snap = require_net(&state)?;
    let phone = setup::find_usb_phone(&state.adb).await?;
    // Right after NetBird starts, the peer list may not have arrived yet.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !snap.synced && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(500)).await;
        snap = state.net.refresh().await;
    }
    let suggested_peer_id = setup::match_peer(&snap, &phone.ips).map(|p| p.id);
    Ok(UsbDetect {
        phone,
        suggested_peer_id,
        peers: snap.peers,
    })
}

/// Enable adb over TCP on the USB phone and remember the chosen NetBird peer.
#[tauri::command]
pub async fn usb_setup(
    app: AppHandle,
    state: State<'_, SharedState>,
    serial: String,
    peer_id: String,
) -> Res<PhoneConfig> {
    let snap = require_net(&state)?;
    let peer = snap.peer_by_id(&peer_id).cloned().ok_or_else(|| {
        AppError::new(
            ErrorCode::BadInput,
            "That device is no longer in your NetBird network",
        )
    })?;
    let ip = peer
        .ipv4()
        .map(std::net::IpAddr::V4)
        .or_else(|| peer.ips.first().copied())
        .ok_or_else(|| AppError::new(ErrorCode::BadInput, "That device has no NetBird IP"))?;
    setup::enable_tcpip(&state.adb, &serial, 5555).await?;
    let phone = PhoneConfig {
        node_id: peer.id.clone(),
        name: peer.display_name(),
        ip,
    };
    let settings = state
        .settings
        .update(|s| {
            if s.phone.as_ref().map(|p| &p.node_id) != Some(&phone.node_id) {
                s.apps.clear(); // different phone, different apps
            }
            s.phone = Some(phone.clone());
        })
        .map_err(settings_err)?;
    let _ = app.emit("settings", &settings);
    state.conn.set_phone(Some(phone.clone())).await;
    Ok(phone)
}

#[derive(Serialize)]
pub struct Reenabled {
    /// Set when the phone on USB doesn't look like the saved phone.
    pub warning: Option<String>,
}

/// After a phone reboot: plug in, one click.
#[tauri::command]
pub async fn usb_reenable(state: State<'_, SharedState>) -> Res<Reenabled> {
    let saved = state
        .conn
        .phone()
        .ok_or_else(|| AppError::new(ErrorCode::PhoneNotSetUp, "No phone is set up yet"))?;
    let usb = setup::find_usb_phone(&state.adb).await?;
    let has_saved_ip = usb
        .ips
        .iter()
        .any(|ip| std::net::IpAddr::V4(*ip) == saved.ip);
    let warning = (!has_saved_ip).then(|| {
        format!(
            "The phone on USB doesn't have {}'s NetBird address ({}). Either NetBird isn't connected on the phone (open the NetBird app), or this is a different phone (use \"Set up via USB\").",
            saved.name, saved.ip
        )
    });
    setup::enable_tcpip(&state.adb, &usb.serial, 5555).await?;
    state.conn.connect_now().await;
    Ok(Reenabled { warning })
}

#[tauri::command]
pub async fn forget_phone(app: AppHandle, state: State<'_, SharedState>) -> Res<()> {
    state.procs.terminate_all(Duration::from_secs(2)).await;
    state.conn.set_phone(None).await;
    let settings = state
        .settings
        .update(|s| {
            s.phone = None;
            s.apps.clear();
        })
        .map_err(settings_err)?;
    let _ = app.emit("settings", &settings);
    Ok(())
}

#[tauri::command]
pub async fn connect_now(state: State<'_, SharedState>) -> Res<()> {
    if state.conn.phone().is_none() {
        return Err(
            AppError::new(ErrorCode::PhoneNotSetUp, "No phone is set up yet")
                .hint("Use \"Set up via USB\"."),
        );
    }
    state.conn.connect_now().await;
    Ok(())
}

#[tauri::command]
pub async fn disconnect(state: State<'_, SharedState>) -> Res<()> {
    state.procs.terminate_all(Duration::from_secs(2)).await;
    state.conn.disconnect().await;
    Ok(())
}

// ---- Apps & windows --------------------------------------------------------

fn connected_serial(state: &SharedState) -> Res<String> {
    state.conn.serial().ok_or_else(|| {
        let why = state.conn.status().summary();
        AppError::new(
            ErrorCode::PhoneUnreachable,
            format!("The phone isn't connected ({why})"),
        )
    })
}

#[tauri::command]
pub async fn list_apps(
    app: AppHandle,
    state: State<'_, SharedState>,
    refresh: bool,
) -> Res<Vec<AndroidApp>> {
    let cached = state.settings.get().apps;
    if !refresh && !cached.is_empty() {
        return Ok(cached);
    }
    let serial = connected_serial(&state)?;
    let mut apps = state.scrcpy.list_apps(&serial).await?;
    apps.sort_by_key(|a| a.name.to_lowercase());
    let settings = state
        .settings
        .update(|s| s.apps = apps.clone())
        .map_err(settings_err)?;
    let _ = app.emit("settings", &settings);
    Ok(apps)
}

/// Start a scrcpy window (shared by the UI and the tray).
pub fn launch(state: &SharedState, mode: LaunchMode) -> Res<WindowInfo> {
    if let Some(e) = &state.tools_error {
        return Err(e.clone());
    }
    let serial = connected_serial(state)?;
    let settings = state.settings.get();
    let (label, package) = match &mode {
        LaunchMode::Mirror => {
            if state.procs.list().iter().any(|w| w.package.is_none()) {
                return Err(AppError::new(
                    ErrorCode::BadInput,
                    "The full mirror is already open",
                ));
            }
            ("Full mirror".to_string(), None)
        }
        LaunchMode::App { package, name } => {
            if state.procs.find_package(package).is_some() {
                return Err(AppError::new(
                    ErrorCode::BadInput,
                    format!("{name} is already open in a window"),
                ));
            }
            (name.clone(), Some(package.clone()))
        }
    };
    let args = window_args(&serial, settings.preset, &mode, &settings.window);
    tracing::info!("launching scrcpy {}", args.join(" "));
    state
        .procs
        .spawn(state.scrcpy.command(&args), label, package)
}

#[tauri::command]
pub fn open_app(state: State<'_, SharedState>, package: String, name: String) -> Res<WindowInfo> {
    launch(&state, LaunchMode::App { package, name })
}

#[tauri::command]
pub fn open_mirror(state: State<'_, SharedState>) -> Res<WindowInfo> {
    launch(&state, LaunchMode::Mirror)
}

#[tauri::command]
pub async fn close_window(state: State<'_, SharedState>, id: u64) -> Res<()> {
    state.procs.terminate(id, Duration::from_secs(2)).await;
    Ok(())
}

// ---- Preferences -----------------------------------------------------------

#[tauri::command]
pub fn set_favorite(
    app: AppHandle,
    state: State<'_, SharedState>,
    package: String,
    favorite: bool,
) -> Res<Settings> {
    let s = state
        .settings
        .update(|s| {
            s.favorites.retain(|p| p != &package);
            if favorite {
                s.favorites.push(package);
            }
        })
        .map_err(settings_err)?;
    let _ = app.emit("settings", &s);
    crate::tray::refresh(&app);
    Ok(s)
}

#[tauri::command]
pub fn set_preset(
    app: AppHandle,
    state: State<'_, SharedState>,
    preset: QualityPreset,
) -> Res<Settings> {
    let s = state
        .settings
        .update(|s| s.preset = preset)
        .map_err(settings_err)?;
    let _ = app.emit("settings", &s);
    crate::tray::refresh(&app);
    Ok(s)
}

#[tauri::command]
pub fn set_window_options(
    app: AppHandle,
    state: State<'_, SharedState>,
    options: WindowOptions,
) -> Res<Settings> {
    let s = state
        .settings
        .update(|s| s.window = options)
        .map_err(settings_err)?;
    let _ = app.emit("settings", &s);
    Ok(s)
}

// ---- Diagnostics -----------------------------------------------------------

#[derive(Serialize)]
pub struct Diagnostics {
    pub adb_path: String,
    pub scrcpy_path: String,
    pub scrcpy_server_path: String,
    pub adb_version: String,
    pub scrcpy_version: String,
    pub adb_server_port: u16,
    pub forwarder_port: Option<u16>,
    pub forwarder_stats: Option<ForwarderStats>,
    pub netbird_device_name: String,
    pub netbird_management_url: String,
    pub netbird_state_dir: String,
    pub data_dir: String,
    pub log_dir: String,
    /// Other adb/scrcpy installs on this Mac (informational: we never use them).
    pub system_copies: Vec<String>,
}

#[tauri::command]
pub async fn diagnostics(state: State<'_, SharedState>) -> Res<Diagnostics> {
    let adb_version = state
        .adb
        .version()
        .await
        .unwrap_or_else(|e| format!("error: {e}"));
    let scrcpy_version = state
        .scrcpy
        .version()
        .await
        .unwrap_or_else(|e| format!("error: {e}"));
    let system_copies = ["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin"]
        .iter()
        .flat_map(|d| ["adb", "scrcpy"].map(|b| std::path::Path::new(d).join(b)))
        .filter(|p| p.exists())
        .map(|p| p.display().to_string())
        .collect();
    Ok(Diagnostics {
        adb_path: state.tools.adb.display().to_string(),
        scrcpy_path: state.tools.scrcpy.display().to_string(),
        scrcpy_server_path: state.tools.scrcpy_server.display().to_string(),
        adb_version,
        scrcpy_version,
        adb_server_port: state.tools.adb_server_port,
        forwarder_port: state.conn.forwarder_port(),
        forwarder_stats: state.conn.forwarder_stats(),
        netbird_device_name: state.net.device_name().to_string(),
        netbird_management_url: state.net.management_url(),
        netbird_state_dir: state.net.state_dir().display().to_string(),
        data_dir: state.data_dir.display().to_string(),
        log_dir: state.log_dir.display().to_string(),
        system_copies,
    })
}

#[tauri::command]
pub fn open_logs(app: AppHandle, state: State<'_, SharedState>) -> Res<()> {
    app.opener()
        .open_path(state.log_dir.to_string_lossy(), None::<&str>)
        .map_err(|e| AppError::new(ErrorCode::Io, format!("Couldn't open the logs folder: {e}")))
}
