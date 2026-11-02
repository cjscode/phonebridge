//! Persistent settings via tauri-plugin-store (`settings.json` in the app data folder).

use std::sync::Mutex;

use pb_core::connection::PhoneConfig;
use pb_core::presets::QualityPreset;
use pb_core::scrcpy::{AndroidApp, WindowOptions};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Runtime};
use tauri_plugin_store::StoreExt;

const STORE_FILE: &str = "settings.json";
const KEY: &str = "settings";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub phone: Option<PhoneConfig>,
    /// Loopback port of the forwarder (keeps the adb serial stable).
    pub forwarder_port: Option<u16>,
    /// Port of our private adb server.
    pub adb_server_port: u16,
    /// Favourite app packages, in display order.
    pub favorites: Vec<String>,
    pub preset: QualityPreset,
    pub window: WindowOptions,
    /// Last `--list-apps` result, so the grid shows instantly.
    pub apps: Vec<AndroidApp>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            phone: None,
            forwarder_port: None,
            adb_server_port: 0,
            favorites: vec![],
            preset: QualityPreset::Wifi,
            window: WindowOptions::default(),
            apps: vec![],
        }
    }
}

/// In-memory copy + write-through to the store.
pub struct SettingsStore<R: Runtime> {
    app: AppHandle<R>,
    data: Mutex<Settings>,
}

impl<R: Runtime> SettingsStore<R> {
    pub fn load(app: &AppHandle<R>) -> Self {
        let data = match app.store(STORE_FILE) {
            Ok(store) => store
                .get(KEY)
                .and_then(|v| match serde_json::from_value::<Settings>(v) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        tracing::error!("settings unreadable, using defaults: {e}");
                        None
                    }
                })
                .unwrap_or_default(),
            Err(e) => {
                tracing::error!("opening settings store: {e}");
                Settings::default()
            }
        };
        Self {
            app: app.clone(),
            data: Mutex::new(data),
        }
    }

    pub fn get(&self) -> Settings {
        self.data.lock().unwrap().clone()
    }

    /// Change settings and save. Returns the new settings.
    pub fn update(&self, f: impl FnOnce(&mut Settings)) -> Result<Settings, String> {
        let snapshot = {
            let mut d = self.data.lock().unwrap();
            f(&mut d);
            d.clone()
        };
        let store = self.app.store(STORE_FILE).map_err(|e| e.to_string())?;
        store.set(
            KEY,
            serde_json::to_value(&snapshot).map_err(|e| e.to_string())?,
        );
        store.save().map_err(|e| format!("saving settings: {e}"))?;
        Ok(snapshot)
    }
}
