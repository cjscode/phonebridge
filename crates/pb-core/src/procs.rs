//! Child process registry for scrcpy windows.
//!
//! - every child runs in its own process group, so SIGTERM reaches anything it spawns;
//! - the last stderr lines are kept so a failed window can say why;
//! - a PID file lets the next launch clean up after a crash (macOS has no
//!   "kill children when parent dies").

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::broadcast;

use crate::error::{AppError, ErrorCode};

const STDERR_LINES: usize = 40;

#[derive(Debug, Clone, Serialize)]
pub struct WindowInfo {
    pub id: u64,
    pub label: String,
    /// Package name for app windows, None for full mirror.
    pub package: Option<String>,
    pub pid: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ProcEvent {
    Started {
        window: WindowInfo,
    },
    Exited {
        window: WindowInfo,
        code: Option<i32>,
        success: bool,
        stderr_tail: Vec<String>,
        requested: bool,
    },
}

struct Entry {
    info: WindowInfo,
    stop_requested: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Serialize, Deserialize)]
struct PidRecord {
    pid: u32,
    exe: String,
}

pub struct ProcessRegistry {
    entries: Mutex<HashMap<u64, Entry>>,
    next_id: AtomicU64,
    events: broadcast::Sender<ProcEvent>,
    pid_file: Option<PathBuf>,
    exe_label: String,
}

impl ProcessRegistry {
    /// `pid_file`: where to record live children (for crash cleanup). `exe` is
    /// recorded alongside each pid so cleanup never kills an unrelated process.
    pub fn new(pid_file: Option<PathBuf>, exe: &Path) -> Arc<Self> {
        let (events, _) = broadcast::channel(64);
        Arc::new(Self {
            entries: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            events,
            pid_file,
            exe_label: exe.to_string_lossy().into_owned(),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ProcEvent> {
        self.events.subscribe()
    }

    pub fn list(&self) -> Vec<WindowInfo> {
        let mut v: Vec<_> = self
            .entries
            .lock()
            .unwrap()
            .values()
            .map(|e| e.info.clone())
            .collect();
        v.sort_by_key(|w| w.id);
        v
    }

    pub fn find_package(&self, package: &str) -> Option<WindowInfo> {
        self.entries
            .lock()
            .unwrap()
            .values()
            .find(|e| e.info.package.as_deref() == Some(package))
            .map(|e| e.info.clone())
    }

    /// Spawn `cmd` as a tracked window. Must run inside a Tokio runtime
    /// (on a worker, or with `Handle::enter()` from another thread).
    pub fn spawn(
        self: &Arc<Self>,
        mut cmd: Command,
        label: String,
        package: Option<String>,
    ) -> Result<WindowInfo, AppError> {
        cmd.stdout(Stdio::null())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn().map_err(|e| {
            AppError::new(ErrorCode::Scrcpy, format!("could not start scrcpy: {e}"))
        })?;
        let pid = child.id().unwrap_or(0);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let info = WindowInfo {
            id,
            label,
            package,
            pid,
        };
        let stop_requested = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.entries.lock().unwrap().insert(
            id,
            Entry {
                info: info.clone(),
                stop_requested: stop_requested.clone(),
            },
        );
        self.write_pid_file();
        let _ = self.events.send(ProcEvent::Started {
            window: info.clone(),
        });

        let tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_LINES)));
        let reader = child.stderr.take().map(|stderr| {
            let tail = tail.clone();
            let label = info.label.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "scrcpy", "[{label}] {line}");
                    let mut t = tail.lock().unwrap();
                    if t.len() == STDERR_LINES {
                        t.pop_front();
                    }
                    t.push_back(line);
                }
            })
        });

        let me = Arc::downgrade(self);
        tokio::spawn(async move {
            let status = child.wait().await;
            if let Some(r) = reader {
                let _ = tokio::time::timeout(Duration::from_secs(1), r).await;
            }
            let (code, success) = match &status {
                Ok(s) => (s.code(), s.success()),
                Err(_) => (None, false),
            };
            let stderr_tail: Vec<String> = tail.lock().unwrap().iter().cloned().collect();
            if let Some(me) = me.upgrade() {
                let removed = me.entries.lock().unwrap().remove(&id);
                me.write_pid_file();
                let requested = stop_requested.load(Ordering::Acquire);
                if let Some(e) = removed {
                    let _ = me.events.send(ProcEvent::Exited {
                        window: e.info,
                        code,
                        success,
                        stderr_tail,
                        requested,
                    });
                }
            }
        });
        Ok(info)
    }

    /// SIGTERM the window's process group; SIGKILL after `grace`.
    pub async fn terminate(&self, id: u64, grace: Duration) {
        let pid = {
            let entries = self.entries.lock().unwrap();
            match entries.get(&id) {
                Some(e) => {
                    e.stop_requested.store(true, Ordering::Release);
                    e.info.pid
                }
                None => return,
            }
        };
        terminate_group(pid, grace, || {
            self.entries.lock().unwrap().contains_key(&id)
        })
        .await;
    }

    /// Terminate every window (on quit or disconnect).
    pub async fn terminate_all(&self, grace: Duration) {
        let ids: Vec<u64> = self.entries.lock().unwrap().keys().copied().collect();
        let futs = ids.into_iter().map(|id| self.terminate(id, grace));
        for f in futs {
            f.await;
        }
    }

    fn write_pid_file(&self) {
        let Some(path) = &self.pid_file else { return };
        let records: Vec<PidRecord> = self
            .entries
            .lock()
            .unwrap()
            .values()
            .map(|e| PidRecord {
                pid: e.info.pid,
                exe: self.exe_label.clone(),
            })
            .collect();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(path, serde_json::to_vec(&records).unwrap_or_default()) {
            tracing::warn!("writing {}: {e}", path.display());
        }
    }
}

#[cfg(unix)]
async fn terminate_group(pid: u32, grace: Duration, still_running: impl Fn() -> bool) {
    if pid == 0 {
        return;
    }
    let pgid = -(pid as i32);
    unsafe { libc::kill(pgid, libc::SIGTERM) };
    let deadline = tokio::time::Instant::now() + grace;
    while tokio::time::Instant::now() < deadline {
        if !still_running() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    unsafe { libc::kill(pgid, libc::SIGKILL) };
}

#[cfg(not(unix))]
async fn terminate_group(_pid: u32, _grace: Duration, _still_running: impl Fn() -> bool) {}

/// Kill children left over from a crashed previous run. Only kills a pid whose
/// executable path still matches what we recorded.
pub fn cleanup_stale(pid_file: &Path) -> usize {
    let Ok(bytes) = std::fs::read(pid_file) else {
        return 0;
    };
    let records: Vec<PidRecord> = serde_json::from_slice(&bytes).unwrap_or_default();
    let mut killed = 0;
    for r in records {
        if r.pid == 0 {
            continue;
        }
        if exe_of(r.pid).as_deref() == Some(r.exe.as_str()) {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(r.pid as i32), libc::SIGKILL);
                libc::kill(r.pid as i32, libc::SIGKILL);
            }
            killed += 1;
        }
    }
    let _ = std::fs::remove_file(pid_file);
    killed
}

/// Executable path of a running process, if it exists.
fn exe_of(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    }
    #[cfg(not(target_os = "linux"))]
    {
        // On macOS `ps -o comm=` prints the full executable path.
        let out = std::process::Command::new("/bin/ps")
            .args(["-o", "comm=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if s.is_empty() { None } else { Some(s) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The app starts windows from the macOS main thread, which only has the
    /// runtime entered (not a worker). That must work.
    #[test]
    fn spawn_from_a_thread_that_entered_the_runtime() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.handle().clone();
        let reg = ProcessRegistry::new(None, Path::new("/bin/sh"));
        let mut events = reg.subscribe();
        let reg2 = reg.clone();
        std::thread::spawn(move || {
            let _in_runtime = handle.enter();
            let mut cmd = Command::new("/bin/sh");
            cmd.args(["-c", "exit 0"]);
            reg2.spawn(cmd, "Mirror".into(), None).unwrap();
        })
        .join()
        .expect("spawning from a non-worker thread must not panic");
        // The exit is only seen if the watcher task runs on the runtime.
        rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !matches!(events.recv().await, Ok(ProcEvent::Exited { .. })) {}
            })
            .await
            .expect("window exit reported");
        });
    }

    #[tokio::test]
    async fn spawn_exit_and_stderr_tail() {
        let reg = ProcessRegistry::new(None, Path::new("/bin/sh"));
        let mut events = reg.subscribe();
        let mut cmd = Command::new("/bin/sh");
        cmd.args([
            "-c",
            "echo 'ERROR: Could not open video stream' >&2; exit 3",
        ]);
        let w = reg
            .spawn(cmd, "Messages".into(), Some("com.x".into()))
            .unwrap();
        assert!(matches!(
            events.recv().await.unwrap(),
            ProcEvent::Started { .. }
        ));
        match events.recv().await.unwrap() {
            ProcEvent::Exited {
                window,
                code,
                success,
                stderr_tail,
                requested,
            } => {
                assert_eq!(window.id, w.id);
                assert_eq!(code, Some(3));
                assert!(!success && !requested);
                assert_eq!(
                    stderr_tail,
                    vec!["ERROR: Could not open video stream".to_string()]
                );
            }
            e => panic!("{e:?}"),
        }
        assert!(reg.list().is_empty());
    }

    #[tokio::test]
    async fn terminate_all_kills_group() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("children.json");
        let reg = ProcessRegistry::new(Some(pid_file.clone()), Path::new("/bin/sh"));
        let mut events = reg.subscribe();
        for i in 0..3 {
            let mut cmd = Command::new("/bin/sh");
            // A child that ignores nothing, plus a grandchild in the same group.
            cmd.args(["-c", "sleep 30 & wait"]);
            reg.spawn(cmd, format!("w{i}"), None).unwrap();
        }
        assert_eq!(reg.list().len(), 3);
        let recorded: Vec<PidRecord> =
            serde_json::from_slice(&std::fs::read(&pid_file).unwrap()).unwrap();
        assert_eq!(recorded.len(), 3);
        reg.terminate_all(Duration::from_secs(2)).await;
        let mut exited = 0;
        while exited < 3 {
            if let ProcEvent::Exited { requested, .. } =
                tokio::time::timeout(Duration::from_secs(5), events.recv())
                    .await
                    .unwrap()
                    .unwrap()
            {
                assert!(requested);
                exited += 1;
            }
        }
        assert!(reg.list().is_empty());
        let recorded: Vec<PidRecord> =
            serde_json::from_slice(&std::fs::read(&pid_file).unwrap()).unwrap();
        assert!(recorded.is_empty());
    }

    #[test]
    fn cleanup_stale_only_kills_matching_exe() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("children.json");
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let real_exe = exe_of(child.id()).unwrap();
        // Wrong exe recorded: must not be killed.
        std::fs::write(
            &pid_file,
            format!(r#"[{{"pid":{},"exe":"/not/it"}}]"#, child.id()),
        )
        .unwrap();
        assert_eq!(cleanup_stale(&pid_file), 0);
        assert!(child.try_wait().unwrap().is_none());
        // Matching exe: killed.
        std::fs::write(
            &pid_file,
            format!(r#"[{{"pid":{},"exe":{:?}}}]"#, child.id(), real_exe),
        )
        .unwrap();
        assert_eq!(cleanup_stale(&pid_file), 1);
        let status = child.wait().unwrap();
        assert!(!status.success());
        assert!(!pid_file.exists());
    }
}
