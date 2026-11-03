//! Persistent settings via tauri-plugin-store (`settings.json` in the app data folder).

use std::collections::BTreeMap;
use std::sync::Mutex;

use pb_core::connection::PhoneConfig;
use pb_core::overlay::Network;
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
    /// Which network PhoneBridge joins.
    pub network: Network,
    /// NetBird management server (NetBird Cloud unless self-hosted).
    pub management_url: String,
    /// Tailscale control server (Tailscale's own, or a Headscale server).
    pub tailscale_control_url: String,
    /// The phone, as reached over `network`.
    pub phone: Option<PhoneConfig>,
    /// The phone as set up on the other network (the phone's IP and peer ID
    /// differ per network), restored when switching back.
    pub other_phones: BTreeMap<Network, PhoneConfig>,
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
            network: Network::Netbird,
            management_url: pb_core::overlay::DEFAULT_MANAGEMENT_URL.to_string(),
            tailscale_control_url: pb_core::overlay::DEFAULT_CONTROL_URL.to_string(),
            phone: None,
            other_phones: BTreeMap::new(),
            forwarder_port: None,
            adb_server_port: 0,
            favorites: vec![],
            preset: QualityPreset::Wifi,
            window: WindowOptions::default(),
            apps: vec![],
        }
    }
}

impl Settings {
    /// The server URL saved for `network`.
    pub fn server_url(&self, network: Network) -> String {
        match network {
            Network::Netbird => self.management_url.clone(),
            Network::Tailscale => self.tailscale_control_url.clone(),
        }
    }

    pub fn set_server_url(&mut self, network: Network, url: String) {
        match network {
            Network::Netbird => self.management_url = url,
            Network::Tailscale => self.tailscale_control_url = url,
        }
    }

    /// Switch networks: park the current phone, bring back the other one.
    pub fn switch_network(&mut self, to: Network) {
        if to == self.network {
            return;
        }
        let from = self.network;
        match self.phone.take() {
            Some(p) => {
                self.other_phones.insert(from, p);
            }
            None => {
                self.other_phones.remove(&from);
            }
        }
        self.phone = self.other_phones.remove(&to);
        self.network = to;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn phone(id: &str) -> PhoneConfig {
        PhoneConfig {
            node_id: id.into(),
            name: "Pixel".into(),
            ip: "100.64.0.1".parse().unwrap(),
        }
    }

    #[test]
    fn each_network_keeps_its_phone() {
        let mut s = Settings {
            phone: Some(phone("nb")),
            ..Default::default()
        };
        s.switch_network(Network::Tailscale);
        assert_eq!(s.network, Network::Tailscale);
        assert_eq!(s.phone, None);
        s.phone = Some(phone("ts"));
        s.switch_network(Network::Netbird);
        assert_eq!(s.phone.as_ref().unwrap().node_id, "nb");
        assert_eq!(s.other_phones[&Network::Tailscale].node_id, "ts");
        s.switch_network(Network::Netbird);
        assert_eq!(s.phone.as_ref().unwrap().node_id, "nb");
    }

    #[test]
    fn old_settings_load_as_netbird() {
        let s: Settings = serde_json::from_str(
            r#"{"management_url":"https://nb.example.com","favorites":["a"]}"#,
        )
        .unwrap();
        assert_eq!(s.network, Network::Netbird);
        assert_eq!(s.server_url(Network::Netbird), "https://nb.example.com");
        assert_eq!(
            s.server_url(Network::Tailscale),
            pb_core::overlay::DEFAULT_CONTROL_URL
        );
    }
}
