use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use pb_core::adb::Adb;
use pb_core::connection::ConnectionHandle;
use pb_core::overlay::NetManager;
use pb_core::procs::ProcessRegistry;
use pb_core::scrcpy::Scrcpy;
use pb_core::tools::BundledTools;
use tauri::Wry;

use crate::settings::SettingsStore;

pub struct AppState {
    pub tools: BundledTools,
    /// Set if the bundle is incomplete; shown in the UI.
    pub tools_error: Option<pb_core::AppError>,
    pub adb: Adb,
    pub scrcpy: Scrcpy,
    pub tailnet: Arc<NetManager>,
    pub conn: ConnectionHandle,
    pub procs: Arc<ProcessRegistry>,
    pub settings: SettingsStore<Wry>,
    pub log_dir: PathBuf,
    pub data_dir: PathBuf,
    pub shutting_down: AtomicBool,
    /// Login URL we already opened in the browser (don't reopen on every poll).
    pub opened_auth_url: Mutex<Option<String>>,
}

pub type SharedState = Arc<AppState>;
