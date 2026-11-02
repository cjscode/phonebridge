//! Menu bar icon: connection status, favourites, full mirror, show, quit.

use pb_core::presets::QualityPreset;
use pb_core::scrcpy::LaunchMode;
use tauri::image::Image;
use tauri::menu::{Menu, MenuBuilder, MenuItem, PredefinedMenuItem, Submenu, SubmenuBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, Wry};

use crate::state::SharedState;

const TRAY_ID: &str = "main";

pub fn create(app: &AppHandle) -> tauri::Result<()> {
    let menu = build_menu(app)?;
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(Image::from_bytes(include_bytes!("../icons/tray.png"))?)
        .icon_as_template(true)
        .tooltip("PhoneBridge")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| on_menu(app, event.id().as_ref()))
        .build(app)?;
    Ok(())
}

/// Rebuild the menu (after status / favourites changes).
pub fn refresh(app: &AppHandle) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else { return };
    match build_menu(app) {
        Ok(menu) => {
            let _ = tray.set_menu(Some(menu));
        }
        Err(e) => tracing::warn!("tray menu: {e}"),
    }
}

fn build_menu(app: &AppHandle) -> tauri::Result<Menu<Wry>> {
    let state = app.state::<SharedState>();
    let status = state.conn.status();
    let connected = status.is_connected();
    let settings = state.settings.get();

    let status_item = MenuItem::with_id(app, "status", status.summary(), false, None::<&str>)?;
    let mirror = MenuItem::with_id(app, "mirror", "Full Mirror", connected, None::<&str>)?;

    let mut favs = SubmenuBuilder::new(app, "Favorites");
    if settings.favorites.is_empty() {
        favs = favs.item(&MenuItem::with_id(app, "nofav", "Star apps in the main window", false, None::<&str>)?);
    }
    for pkg in &settings.favorites {
        let name = settings.apps.iter().find(|a| &a.package == pkg).map(|a| a.name.clone()).unwrap_or_else(|| pkg.clone());
        favs = favs.item(&MenuItem::with_id(app, format!("fav:{pkg}"), name, connected, None::<&str>)?);
    }
    let favs: Submenu<Wry> = favs.build()?;

    let preset_label = match settings.preset {
        QualityPreset::Wifi => "Quality: Wi-Fi (switch to Cellular)",
        QualityPreset::Cellular => "Quality: Cellular (switch to Wi-Fi)",
    };
    let preset = MenuItem::with_id(app, "preset", preset_label, true, None::<&str>)?;
    let reconnect = MenuItem::with_id(app, "reconnect", "Reconnect", state.conn.phone().is_some() && !connected, None::<&str>)?;
    let show = MenuItem::with_id(app, "show", "Show PhoneBridge", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit PhoneBridge", true, Some("CmdOrCtrl+Q"))?;

    MenuBuilder::new(app)
        .item(&status_item)
        .item(&PredefinedMenuItem::separator(app)?)
        .item(&mirror)
        .item(&favs)
        .item(&PredefinedMenuItem::separator(app)?)
        .item(&preset)
        .item(&reconnect)
        .item(&PredefinedMenuItem::separator(app)?)
        .item(&show)
        .item(&quit)
        .build()
}

pub fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

fn on_menu(app: &AppHandle, id: &str) {
    let state = app.state::<SharedState>().inner().clone();
    let result = match id {
        "mirror" => crate::commands::launch(&state, LaunchMode::Mirror).map(|_| ()),
        "show" => {
            show_main(app);
            Ok(())
        }
        "quit" => {
            app.exit(0);
            Ok(())
        }
        "reconnect" => {
            let s = state.clone();
            tauri::async_runtime::spawn(async move { s.conn.connect_now().await });
            Ok(())
        }
        "preset" => {
            let next = match state.settings.get().preset {
                QualityPreset::Wifi => QualityPreset::Cellular,
                QualityPreset::Cellular => QualityPreset::Wifi,
            };
            match state.settings.update(|s| s.preset = next) {
                Ok(s) => {
                    let _ = app.emit("settings", &s);
                    refresh(app);
                    Ok(())
                }
                Err(e) => Err(pb_core::AppError::new(pb_core::ErrorCode::Io, e)),
            }
        }
        other => match other.strip_prefix("fav:") {
            Some(pkg) => {
                let name = state
                    .settings
                    .get()
                    .apps
                    .iter()
                    .find(|a| a.package == pkg)
                    .map(|a| a.name.clone())
                    .unwrap_or_else(|| pkg.to_string());
                crate::commands::launch(&state, LaunchMode::App { package: pkg.to_string(), name }).map(|_| ())
            }
            None => Ok(()),
        },
    };
    if let Err(e) = result {
        crate::report_error(app, e);
    }
}
