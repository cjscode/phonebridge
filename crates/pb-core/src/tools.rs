//! Locations of the bundled adb / scrcpy / scrcpy-server, and the environment
//! every child process gets, so nothing from the system PATH is ever used.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{AppError, ErrorCode};

#[derive(Debug, Clone, Serialize)]
pub struct BundledTools {
    pub adb: PathBuf,
    pub scrcpy: PathBuf,
    pub scrcpy_server: PathBuf,
    /// Directory holding scrcpy.png and disconnected.png (`SCRCPY_ICON_DIR`).
    pub icon_dir: PathBuf,
    /// Port of our private adb server (never the default 5037).
    pub adb_server_port: u16,
}

impl BundledTools {
    /// `bin_dir`: where adb/scrcpy live (`Contents/MacOS` in the .app).
    /// `resource_dir`: where `scrcpy-server` and `scrcpy-icons/` live (`Contents/Resources`).
    pub fn new(bin_dir: &Path, resource_dir: &Path, adb_server_port: u16) -> Self {
        Self {
            adb: bin_dir.join("adb"),
            scrcpy: bin_dir.join("scrcpy"),
            scrcpy_server: resource_dir.join("scrcpy-server"),
            icon_dir: resource_dir.join("scrcpy-icons"),
            adb_server_port,
        }
    }

    /// Paths as laid out in the source tree after `cargo xtask fetch`
    /// (used by the probe example and `cargo tauri dev`).
    pub fn from_source_tree(workspace: &Path, target_triple: &str, adb_server_port: u16) -> Self {
        let tauri = workspace.join("src-tauri");
        Self {
            adb: tauri.join("binaries").join(format!("adb-{target_triple}")),
            scrcpy: tauri.join("binaries").join(format!("scrcpy-{target_triple}")),
            scrcpy_server: tauri.join("resources").join("scrcpy-server"),
            icon_dir: tauri.join("resources").join("scrcpy-icons"),
            adb_server_port,
        }
    }

    /// Fail clearly if anything is missing or not executable.
    pub fn check(&self) -> Result<(), AppError> {
        for (path, exec) in [(&self.adb, true), (&self.scrcpy, true), (&self.scrcpy_server, false)] {
            let meta = std::fs::metadata(path).map_err(|_| {
                AppError::new(ErrorCode::MissingBinary, format!("Bundled file missing: {}", path.display()))
                    .hint("The app bundle looks incomplete. Rebuild it with `cargo xtask dist`.")
            })?;
            #[cfg(unix)]
            if exec {
                use std::os::unix::fs::PermissionsExt;
                if meta.permissions().mode() & 0o111 == 0 {
                    return Err(AppError::new(
                        ErrorCode::MissingBinary,
                        format!("Bundled binary is not executable: {}", path.display()),
                    ));
                }
            }
            let _ = (meta, exec);
        }
        Ok(())
    }

    /// Environment for adb and scrcpy children. Applied after `env_clear()`.
    pub fn child_env(&self) -> Vec<(String, String)> {
        let mut env = vec![
            ("PATH".into(), "/usr/bin:/bin:/usr/sbin:/sbin".into()),
            ("ADB".into(), self.adb.to_string_lossy().into_owned()),
            ("SCRCPY_SERVER_PATH".into(), self.scrcpy_server.to_string_lossy().into_owned()),
            ("SCRCPY_ICON_DIR".into(), self.icon_dir.to_string_lossy().into_owned()),
            ("ANDROID_ADB_SERVER_PORT".into(), self.adb_server_port.to_string()),
        ];
        // Keep what adb needs to find ~/.android/adbkey and a temp dir, and the
        // locale; drop everything else (notably any ADB_* / ANDROID_* overrides).
        for key in ["HOME", "USER", "LOGNAME", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE"] {
            if let Ok(v) = std::env::var(key) {
                env.push((key.into(), v));
            }
        }
        env
    }

    pub fn apply_env(&self, cmd: &mut tokio::process::Command) {
        cmd.env_clear();
        for (k, v) in self.child_env() {
            cmd.env(k, v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_points_at_bundle_only() {
        let t = BundledTools::new(Path::new("/App/Contents/MacOS"), Path::new("/App/Contents/Resources"), 41001);
        let env: std::collections::HashMap<_, _> = t.child_env().into_iter().collect();
        assert_eq!(env["ADB"], "/App/Contents/MacOS/adb");
        assert_eq!(env["SCRCPY_SERVER_PATH"], "/App/Contents/Resources/scrcpy-server");
        assert_eq!(env["SCRCPY_ICON_DIR"], "/App/Contents/Resources/scrcpy-icons");
        assert_eq!(env["ANDROID_ADB_SERVER_PORT"], "41001");
        assert!(!env["PATH"].contains("homebrew") && !env["PATH"].contains("/usr/local"));
    }

    #[test]
    fn check_reports_missing() {
        let t = BundledTools::new(Path::new("/nonexistent"), Path::new("/nonexistent"), 1);
        assert_eq!(t.check().unwrap_err().code, ErrorCode::MissingBinary);
    }
}
