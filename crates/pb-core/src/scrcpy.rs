//! Building scrcpy command lines, listing apps, and spawning windows.

use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::error::{AppError, ErrorCode};
use crate::presets::QualityPreset;
use crate::tools::BundledTools;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AndroidApp {
    pub name: String,
    pub package: String,
    pub system: bool,
}

/// What a window shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchMode {
    /// The phone's real screen.
    Mirror,
    /// One app on its own virtual display.
    App { package: String, name: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowOptions {
    /// Forward audio in full-mirror windows.
    pub mirror_audio: bool,
    /// Forward audio in app windows (only one capture can work at a time).
    pub app_audio: bool,
    /// Resize the virtual display with the window (`--flex-display`).
    pub flex_display: bool,
    /// Use H.265 (better quality per bit; needs a phone HEVC encoder).
    pub h265: bool,
    /// Use `adb forward` instead of `adb reverse` for scrcpy's sockets.
    pub force_adb_forward: bool,
    /// Keep app windows above others.
    pub always_on_top: bool,
}

impl Default for WindowOptions {
    fn default() -> Self {
        Self {
            mirror_audio: true,
            app_audio: false,
            flex_display: false,
            h265: false,
            force_adb_forward: false,
            always_on_top: false,
        }
    }
}

/// Arguments for one scrcpy window (excluding the binary).
pub fn window_args(
    serial: &str,
    preset: QualityPreset,
    mode: &LaunchMode,
    opts: &WindowOptions,
) -> Vec<String> {
    let p = preset.params();
    let mut a = vec![format!("--serial={serial}")];
    a.push(format!("--video-bit-rate={}", p.video_bit_rate));
    a.push(format!("--max-fps={}", p.max_fps));
    if opts.h265 {
        a.push("--video-codec=h265".into());
    }
    if opts.force_adb_forward {
        a.push("--force-adb-forward".into());
    }
    if opts.always_on_top {
        a.push("--always-on-top".into());
    }
    match mode {
        LaunchMode::Mirror => {
            a.push(format!("--max-size={}", p.max_size));
            a.push("--window-title=Phone — PhoneBridge".into());
            if opts.mirror_audio {
                a.push(format!("--audio-bit-rate={}", p.audio_bit_rate));
            } else {
                a.push("--no-audio".into());
            }
        }
        LaunchMode::App { package, name } => {
            a.push(format!("--new-display={}", p.app_display));
            a.push(format!("--start-app={package}"));
            a.push(format!("--window-title={name} — PhoneBridge"));
            a.push("--keep-active".into());
            if opts.flex_display {
                a.push("--flex-display".into());
            } else {
                a.push(format!("--max-size={}", p.max_size));
            }
            if opts.app_audio {
                a.push(format!("--audio-bit-rate={}", p.audio_bit_rate));
            } else {
                a.push("--no-audio".into());
            }
        }
    }
    a
}

/// Parse the output of `scrcpy --list-apps`.
///
/// Server format (LogUtils.buildAppListMessage): `" * "` (system) or `" - "`,
/// the name padded to 30 columns, a space, the package. Names of 30+ chars are
/// followed by a line of spaces and the package.
pub fn parse_list_apps(text: &str) -> Vec<AndroidApp> {
    let mut apps = Vec::new();
    let mut pending: Option<(String, bool)> = None;
    for raw in text.lines() {
        let line = raw.trim_end();
        let trimmed = line.trim_start();
        let marker = trimmed.get(..2);
        let is_entry = matches!(marker, Some("* ") | Some("- ")) && line.starts_with(' ');
        if is_entry {
            pending = None;
            let system = marker == Some("* ");
            let rest = &trimmed[2..];
            match rest.rfind("  ") {
                Some(idx) if is_package(rest[idx..].trim()) => {
                    let name = rest[..idx].trim().to_string();
                    apps.push(AndroidApp {
                        name,
                        package: rest[idx..].trim().to_string(),
                        system,
                    });
                }
                _ => pending = Some((rest.trim().to_string(), system)),
            }
        } else if let Some((name, system)) = pending.take()
            && line.starts_with("    ")
            && is_package(trimmed)
        {
            apps.push(AndroidApp {
                name,
                package: trimmed.to_string(),
                system,
            });
        }
    }
    apps
}

fn is_package(s: &str) -> bool {
    !s.is_empty()
        && s.contains('.')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
        && s.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
}

#[derive(Debug, Clone)]
pub struct Scrcpy {
    tools: BundledTools,
}

impl Scrcpy {
    pub fn new(tools: BundledTools) -> Self {
        Self { tools }
    }

    pub fn command(&self, args: &[String]) -> Command {
        let mut cmd = Command::new(&self.tools.scrcpy);
        self.tools.apply_env(&mut cmd);
        cmd.args(args).stdin(Stdio::null());
        cmd
    }

    pub async fn version(&self) -> Result<String, AppError> {
        let out = self
            .command(&["--version".into()])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| {
                AppError::new(
                    ErrorCode::MissingBinary,
                    format!("could not run bundled scrcpy: {e}"),
                )
            })?;
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .to_string())
    }

    /// Launchable apps on the phone (pushes scrcpy-server, ~0.7 MB, each call).
    pub async fn list_apps(&self, serial: &str) -> Result<Vec<AndroidApp>, AppError> {
        let args = [format!("--serial={serial}"), "--list-apps".into()];
        let child = self
            .command(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                AppError::new(
                    ErrorCode::MissingBinary,
                    format!("could not run bundled scrcpy: {e}"),
                )
            })?;
        let out = tokio::time::timeout(Duration::from_secs(90), child.wait_with_output())
            .await
            .map_err(|_| AppError::new(ErrorCode::Scrcpy, "Listing apps timed out"))?
            .map_err(AppError::from)?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let apps = parse_list_apps(&format!("{stdout}\n{stderr}"));
        if !out.status.success() || apps.is_empty() {
            let tail: Vec<&str> = stderr
                .lines()
                .rev()
                .take(6)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            return Err(AppError::new(
                ErrorCode::Scrcpy,
                format!("Couldn't list apps: {}", tail.join(" ").trim()),
            )
            .hint("Check that the phone is connected and unlocked."));
        }
        Ok(apps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = include_str!("../tests/fixtures/list_apps.txt");

    #[test]
    fn parses_list_apps_fixture() {
        let apps = parse_list_apps(LIST);
        let find = |p: &str| apps.iter().find(|a| a.package == p).cloned();
        assert_eq!(apps.len(), 7, "{apps:#?}");
        let msgs = find("com.google.android.apps.messaging").unwrap();
        assert_eq!(msgs.name, "Messages");
        assert!(msgs.system);
        let long = find("com.example.verylongname").unwrap();
        assert_eq!(long.name, "A Very Long Application Name That Wraps");
        assert!(!long.system);
        assert_eq!(find("org.mozilla.firefox").unwrap().name, "Firefox");
        assert_eq!(find("com.whatsapp").unwrap().name, "WhatsApp  Beta");
        // 29-char name: padding is exactly one space plus the separator.
        assert_eq!(
            find("com.example.edge").unwrap().name,
            "ABCDEFGHIJKLMNOPQRSTUVWXYZabc"
        );
    }

    #[test]
    fn app_window_args() {
        let mode = LaunchMode::App {
            package: "com.google.android.apps.messaging".into(),
            name: "Messages".into(),
        };
        let a = window_args(
            "127.0.0.1:41234",
            QualityPreset::Cellular,
            &mode,
            &WindowOptions::default(),
        );
        assert!(a.contains(&"--serial=127.0.0.1:41234".to_string()));
        assert!(a.contains(&"--new-display=720x1280/320".to_string()));
        assert!(a.contains(&"--start-app=com.google.android.apps.messaging".to_string()));
        assert!(a.contains(&"--video-bit-rate=1500K".to_string()));
        assert!(a.contains(&"--max-fps=30".to_string()));
        assert!(a.contains(&"--max-size=1024".to_string()));
        assert!(a.contains(&"--no-audio".to_string()));
        assert!(a.contains(&"--keep-active".to_string()));
        assert!(
            !a.iter().any(|x| x.starts_with("--tcpip")),
            "never let scrcpy dial the phone itself"
        );
    }

    #[test]
    fn flex_display_drops_max_size() {
        let mode = LaunchMode::App {
            package: "a.b".into(),
            name: "B".into(),
        };
        let opts = WindowOptions {
            flex_display: true,
            ..Default::default()
        };
        let a = window_args("s", QualityPreset::Wifi, &mode, &opts);
        assert!(a.contains(&"--flex-display".to_string()));
        assert!(!a.iter().any(|x| x.starts_with("--max-size")));
    }

    #[test]
    fn mirror_args() {
        let a = window_args(
            "s",
            QualityPreset::Wifi,
            &LaunchMode::Mirror,
            &WindowOptions::default(),
        );
        assert!(a.contains(&"--max-size=1920".to_string()));
        assert!(a.contains(&"--video-bit-rate=8M".to_string()));
        assert!(a.contains(&"--max-fps=60".to_string()));
        assert!(a.contains(&"--audio-bit-rate=128K".to_string()));
        assert!(!a.iter().any(|x| x.starts_with("--new-display")));
    }
}
