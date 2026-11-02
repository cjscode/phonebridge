//! Safe wrapper over one libtailscale (tsnet) server.
//!
//! All calls here are blocking (they cross into Go). Async callers use
//! `spawn_blocking`; see `TailnetManager` and `TailnetDialer`.

use std::ffi::{CStr, CString};
use std::io::{BufRead, BufReader};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::raw::{c_char, c_int};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use libtailscale_sys as ffi;

use super::status::{RawStatus, TailnetSnapshot};
use crate::error::{AppError, ErrorCode};

/// Settings for a new node.
#[derive(Debug, Clone)]
pub struct NodeConfig {
    /// tsnet state directory (keys, prefs). Keeps us logged in across launches.
    pub state_dir: PathBuf,
    pub hostname: String,
    pub auth_key: Option<String>,
}

/// Why a dial failed. Used to tell "phone rebooted" from "phone offline".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, serde::Serialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum DialError {
    /// The phone answered with a TCP reset: it is on the tailnet, but nothing listens on the port.
    #[error("connection refused")]
    Refused,
    /// No answer in time.
    #[error("timed out")]
    Timeout,
    /// No route to the address (peer gone, ACL, or not on the tailnet).
    #[error("unreachable: {0}")]
    Unreachable(String),
    /// The tailnet node isn't running (logged out, starting, or closed).
    #[error("tailnet not running")]
    NotRunning,
    #[error("{0}")]
    Other(String),
}

impl DialError {
    /// Classify a Go/netstack error string.
    pub fn classify(msg: &str) -> DialError {
        let m = msg.to_ascii_lowercase();
        if m.contains("refused") {
            DialError::Refused
        } else if m.contains("timed out") || m.contains("timeout") || m.contains("deadline exceeded") {
            DialError::Timeout
        } else if m.contains("no route")
            || m.contains("unreachable")
            || m.contains("no such host")
            || m.contains("not found")
            || m.contains("no matching peer")
        {
            DialError::Unreachable(msg.to_string())
        } else if m.contains("not running") || m.contains("needslogin") || m.contains("server closed") {
            DialError::NotRunning
        } else {
            DialError::Other(msg.to_string())
        }
    }
}

/// LocalAPI / SOCKS5 loopback endpoint of a node.
#[derive(Debug, Clone)]
pub struct Loopback {
    pub addr: String,
    pub proxy_cred: String,
    pub local_api_cred: String,
}

pub struct Node {
    sd: ffi::tailscale,
    closed: AtomicBool,
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Node").field("sd", &self.sd).finish()
    }
}

fn cstring(s: &str, what: &str) -> Result<CString, AppError> {
    CString::new(s).map_err(|_| AppError::new(ErrorCode::BadInput, format!("{what} contains a NUL byte")))
}

impl Node {
    /// Create (but don't start) a node.
    pub fn new(cfg: &NodeConfig) -> Result<Node, AppError> {
        std::fs::create_dir_all(&cfg.state_dir).map_err(|e| {
            AppError::new(ErrorCode::Io, format!("creating {}: {e}", cfg.state_dir.display()))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&cfg.state_dir, std::fs::Permissions::from_mode(0o700));
        }

        let sd = unsafe { ffi::tailscale_new() };
        if sd < 0 {
            return Err(AppError::new(ErrorCode::Tailnet, "tailscale_new failed"));
        }
        let node = Node { sd, closed: AtomicBool::new(false) };

        let dir = cstring(&cfg.state_dir.to_string_lossy(), "state dir")?;
        node.check(unsafe { ffi::tailscale_set_dir(sd, dir.as_ptr()) }, "set_dir")?;
        let host = cstring(&cfg.hostname, "hostname")?;
        node.check(unsafe { ffi::tailscale_set_hostname(sd, host.as_ptr()) }, "set_hostname")?;
        if let Some(key) = cfg.auth_key.as_deref().filter(|k| !k.trim().is_empty()) {
            let key = cstring(key.trim(), "auth key")?;
            node.check(unsafe { ffi::tailscale_set_authkey(sd, key.as_ptr()) }, "set_authkey")?;
        }
        node.pipe_logs()?;
        Ok(node)
    }

    /// Route tsnet's logs into `tracing` (target `tsnet`).
    fn pipe_logs(&self) -> Result<(), AppError> {
        let mut fds = [0 as c_int; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        for fd in fds {
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        }
        let (read_fd, write_fd) = (fds[0], fds[1]);
        // Go takes ownership of write_fd (os.NewFile).
        let rc = unsafe { ffi::tailscale_set_logfd(self.sd, write_fd) };
        if rc != 0 {
            unsafe {
                libc::close(read_fd);
                libc::close(write_fd);
            }
            return Err(self.error("set_logfd"));
        }
        let reader = unsafe { std::fs::File::from_raw_fd(read_fd) };
        let sd = self.sd;
        std::thread::Builder::new()
            .name(format!("tsnet-log-{sd}"))
            .spawn(move || {
                for line in BufReader::new(reader).lines() {
                    match line {
                        Ok(l) if !l.trim().is_empty() => tracing::debug!(target: "tsnet", "{l}"),
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
            })
            .map_err(AppError::from)?;
        Ok(())
    }

    fn errmsg(&self) -> String {
        let mut buf = vec![0 as c_char; 2048];
        unsafe { ffi::tailscale_errmsg(self.sd, buf.as_mut_ptr(), buf.len()) };
        unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned()
    }

    fn error(&self, op: &str) -> AppError {
        let msg = self.errmsg();
        AppError::new(ErrorCode::Tailnet, format!("tailscale {op}: {msg}"))
    }

    fn check(&self, rc: c_int, op: &str) -> Result<(), AppError> {
        if rc == 0 { Ok(()) } else { Err(self.error(op)) }
    }

    fn ensure_open(&self) -> Result<(), AppError> {
        if self.closed.load(Ordering::Acquire) {
            Err(AppError::new(ErrorCode::Tailnet, "tailnet node is closed"))
        } else {
            Ok(())
        }
    }

    /// Connect to the control plane (non-blocking login: if login is needed the
    /// status reports `NeedsLogin` with an `AuthURL`).
    pub fn start(&self) -> Result<(), AppError> {
        self.ensure_open()?;
        self.check(unsafe { ffi::tailscale_start(self.sd) }, "start")
    }

    /// Current status (ipnstate.Status) via tsnet's in-memory LocalAPI. Blocks up to ~10s.
    pub fn status(&self) -> Result<TailnetSnapshot, AppError> {
        Ok(self.raw_status()?.into_snapshot())
    }

    pub fn raw_status(&self) -> Result<RawStatus, AppError> {
        self.ensure_open()?;
        let mut ptr: *mut c_char = std::ptr::null_mut();
        let rc = unsafe { ffi::tailscale_status_json(self.sd, &mut ptr) };
        if rc != 0 || ptr.is_null() {
            return Err(self.error("status"));
        }
        let json = unsafe { CStr::from_ptr(ptr) }.to_bytes().to_vec();
        unsafe { libc::free(ptr.cast()) };
        serde_json::from_slice(&json)
            .map_err(|e| AppError::new(ErrorCode::Tailnet, format!("parsing tailnet status: {e}")))
    }

    /// Open a TCP connection to `addr` ("ip:port") through the tailnet.
    /// Blocks with no internal timeout; callers must bound it.
    pub fn dial_tcp(&self, addr: &str) -> Result<OwnedFd, DialError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(DialError::NotRunning);
        }
        let network = c"tcp";
        let addr_c = CString::new(addr).map_err(|_| DialError::Other("bad address".into()))?;
        let mut fd: RawFd = -1;
        let rc = unsafe { ffi::tailscale_dial(self.sd, network.as_ptr(), addr_c.as_ptr(), &mut fd) };
        if rc != 0 || fd < 0 {
            // lastErr is per-node; with concurrent dials it may belong to another
            // call. That only affects the classification, never correctness.
            return Err(DialError::classify(&self.errmsg()));
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// Start (once) and return the LocalAPI loopback endpoint.
    pub fn loopback(&self) -> Result<Loopback, AppError> {
        self.ensure_open()?;
        let mut addr = vec![0 as c_char; 128];
        let mut proxy = vec![0 as c_char; 33];
        let mut local = vec![0 as c_char; 33];
        let rc = unsafe {
            ffi::tailscale_loopback(self.sd, addr.as_mut_ptr(), addr.len(), proxy.as_mut_ptr(), local.as_mut_ptr())
        };
        if rc != 0 {
            return Err(self.error("loopback"));
        }
        let s = |b: &[c_char]| unsafe { CStr::from_ptr(b.as_ptr()) }.to_string_lossy().into_owned();
        Ok(Loopback { addr: s(&addr), proxy_cred: s(&proxy), local_api_cred: s(&local) })
    }

    /// Shut the node down. Idempotent.
    pub fn close(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            let rc = unsafe { ffi::tailscale_close(self.sd) };
            if rc != 0 {
                tracing::warn!("tailscale_close returned {rc}");
            }
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_errors() {
        assert_eq!(DialError::classify("dial tcp 100.1.2.3:5555: connect: connection refused"), DialError::Refused);
        assert_eq!(DialError::classify("connection was refused"), DialError::Refused);
        assert_eq!(DialError::classify("dial tcp: i/o timeout"), DialError::Timeout);
        assert!(matches!(DialError::classify("no route to host"), DialError::Unreachable(_)));
        assert!(matches!(DialError::classify("weird"), DialError::Other(_)));
    }
}
