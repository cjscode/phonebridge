//! Safe wrapper over one embedded overlay client: a NetBird client or a
//! Tailscale node (see `crates/overlay-sys/go`).
//!
//! All calls block (they cross into Go). Async callers use `spawn_blocking`.

use std::ffi::{CStr, CString};
use std::io::{BufRead, BufReader};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::raw::{c_char, c_int};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use overlay_sys as ffi;
use serde::{Deserialize, Serialize};

use super::status::{Network, RawStatus};
use crate::error::{AppError, ErrorCode};

/// How the node authenticates when it starts.
#[derive(Debug, Clone)]
pub enum Credential {
    /// A NetBird setup key or Tailscale auth key (registers a new device).
    SetupKey(String),
    /// NetBird: a token from the browser (SSO) login.
    Jwt(String),
    /// The identity saved by an earlier login. For Tailscale, also "start and
    /// ask for a browser login if there is none".
    SavedKey,
}

#[derive(Debug, Clone)]
pub struct NodeConfig {
    pub network: Network,
    pub device_name: String,
    /// NetBird management URL, or Tailscale/Headscale control URL.
    pub management_url: String,
    /// NetBird: `config.json` (the peer's identity) and `state.json`.
    /// Tailscale: `tailscaled.state` and tsnet's other files.
    pub state_dir: PathBuf,
    pub credential: Credential,
}

impl NodeConfig {
    /// NetBird's identity file.
    pub fn config_path(dir: &Path) -> PathBuf {
        dir.join("config.json")
    }

    /// True if `dir` holds a login for `network` from an earlier run.
    pub fn has_login(network: Network, dir: &Path) -> bool {
        match network {
            Network::Netbird => Self::config_path(dir).is_file(),
            // tsnet writes this file (with a machine key) before any login;
            // a finished login adds the current profile.
            Network::Tailscale => std::fs::read(dir.join("tailscaled.state"))
                .ok()
                .and_then(|d| {
                    serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&d).ok()
                })
                .is_some_and(|m| m.contains_key("_current-profile")),
        }
    }
}

/// Why a dial failed. Used to tell "phone rebooted" from "phone offline".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum DialError {
    /// The phone answered with a TCP reset: it is on the network, but nothing listens on the port.
    #[error("connection refused")]
    Refused,
    /// No answer in time.
    #[error("timed out")]
    Timeout,
    /// No route to the address (peer gone, blocked by policy, or not on the network).
    #[error("unreachable: {0}")]
    Unreachable(String),
    /// Our overlay client isn't running (logged out, starting, or stopped).
    #[error("network client not running")]
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
        } else if m.contains("timed out")
            || m.contains("timeout")
            || m.contains("deadline exceeded")
        {
            DialError::Timeout
        } else if m.contains("no route")
            || m.contains("unreachable")
            || m.contains("no such host")
            || m.contains("not found")
        {
            DialError::Unreachable(msg.to_string())
        } else if m.contains("not started")
            || m.contains("invalid netbird client handle")
            || m.contains("invalid tailscale node handle")
            || m.contains("backend in state")
        {
            DialError::NotRunning
        } else {
            DialError::Other(msg.to_string())
        }
    }
}

/// True when a start/login error means "log in again" rather than "try later".
pub fn is_login_error(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    [
        "permissiondenied",
        "unauthenticated",
        "expired",
        "setup key",
        "setupkey",
        "invalid token",
        "token is invalid",
        "unauthorized",
        "no saved netbird login",
        "no private key",
        "approval",
        "invalid key",
        "auth key",
        "authkey",
    ]
    .iter()
    .any(|k| m.contains(k))
}

/// Take ownership of a Go-allocated string.
fn take(p: *mut c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    unsafe { ffi::ov_free(p) };
    s
}

fn cstring(s: &str, what: &str) -> Result<CString, AppError> {
    CString::new(s)
        .map_err(|_| AppError::new(ErrorCode::BadInput, format!("{what} contains a NUL byte")))
}

#[derive(Serialize)]
struct NewOptions<'a> {
    device_name: &'a str,
    management_url: &'a str,
    config_path: String,
    state_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    setup_key: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    jwt_token: Option<&'a str>,
    use_saved_key: bool,
    log_fd: c_int,
    log_level: &'a str,
}

#[derive(Serialize)]
struct TsOptions<'a> {
    hostname: &'a str,
    state_dir: String,
    /// Empty = Tailscale's own control server.
    control_url: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    auth_key: Option<&'a str>,
    log_fd: c_int,
}

pub struct Node {
    network: Network,
    h: c_int,
    stopped: AtomicBool,
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Node")
            .field("network", &self.network)
            .field("h", &self.h)
            .finish()
    }
}

impl Node {
    /// Create (but don't start) a client.
    pub fn new(cfg: &NodeConfig) -> Result<Node, AppError> {
        match cfg.network {
            Network::Netbird => Self::new_netbird(cfg),
            Network::Tailscale => Self::new_tailscale(cfg),
        }
    }

    pub fn network(&self) -> Network {
        self.network
    }

    fn new_tailscale(cfg: &NodeConfig) -> Result<Node, AppError> {
        make_private_dir(&cfg.state_dir)?;
        let log_fd = pipe_logs("tailscale").unwrap_or(-1);
        let control_url = if cfg.management_url == Network::Tailscale.default_server_url() {
            ""
        } else {
            cfg.management_url.as_str()
        };
        let opts = TsOptions {
            hostname: &cfg.device_name,
            state_dir: cfg.state_dir.to_string_lossy().into_owned(),
            control_url,
            auth_key: match &cfg.credential {
                Credential::SetupKey(k) => Some(k.trim()),
                _ => None,
            },
            log_fd,
        };
        let json = cstring(&serde_json::to_string(&opts).unwrap_or_default(), "options")?;
        let mut err = std::ptr::null_mut();
        let h = unsafe { ffi::ts_new(json.as_ptr(), &mut err) };
        if h < 0 {
            if log_fd >= 0 {
                unsafe { libc::close(log_fd) };
            }
            return Err(login_or_net_error("Tailscale setup", take(err)));
        }
        Ok(Node {
            network: Network::Tailscale,
            h,
            stopped: AtomicBool::new(false),
        })
    }

    fn new_netbird(cfg: &NodeConfig) -> Result<Node, AppError> {
        make_private_dir(&cfg.state_dir)?;
        let log_fd = pipe_logs("netbird").unwrap_or(-1);
        let (setup_key, jwt_token, use_saved_key) = match &cfg.credential {
            Credential::SetupKey(k) => (Some(k.trim()), None, false),
            Credential::Jwt(t) => (None, Some(t.as_str()), false),
            Credential::SavedKey => (None, None, true),
        };
        let opts = NewOptions {
            device_name: &cfg.device_name,
            management_url: &cfg.management_url,
            config_path: NodeConfig::config_path(&cfg.state_dir)
                .to_string_lossy()
                .into_owned(),
            state_path: cfg
                .state_dir
                .join("state.json")
                .to_string_lossy()
                .into_owned(),
            setup_key,
            jwt_token,
            use_saved_key,
            log_fd,
            log_level: "info",
        };
        let json = cstring(&serde_json::to_string(&opts).unwrap_or_default(), "options")?;
        let mut err = std::ptr::null_mut();
        let h = unsafe { ffi::nb_new(json.as_ptr(), &mut err) };
        if h < 0 {
            if log_fd >= 0 {
                unsafe { libc::close(log_fd) };
            }
            return Err(login_or_net_error("NetBird setup", take(err)));
        }
        Ok(Node {
            network: Network::Netbird,
            h,
            stopped: AtomicBool::new(false),
        })
    }

    /// NetBird: log in and bring the engine up. Tailscale: start, and wait
    /// until it runs, needs a login (the URL is then in the status) or fails.
    pub fn start(&self, timeout: Duration) -> Result<(), AppError> {
        let mut err = std::ptr::null_mut();
        let ms = timeout.as_millis().min(i32::MAX as u128) as c_int;
        let rc = match self.network {
            Network::Netbird => unsafe { ffi::nb_start(self.h, ms, &mut err) },
            Network::Tailscale => unsafe { ffi::ts_start(self.h, ms, &mut err) },
        };
        if rc != 0 {
            return Err(login_or_net_error(self.network.label(), take(err)));
        }
        Ok(())
    }

    pub fn raw_status(&self) -> Result<RawStatus, AppError> {
        let mut out = std::ptr::null_mut();
        let mut err = std::ptr::null_mut();
        let rc = match self.network {
            Network::Netbird => unsafe { ffi::nb_status_json(self.h, &mut out, &mut err) },
            Network::Tailscale => unsafe { ffi::ts_status_json(self.h, &mut out, &mut err) },
        };
        let label = self.network.label();
        if rc != 0 {
            return Err(AppError::new(
                ErrorCode::Network,
                format!("{label} status: {}", take(err)),
            ));
        }
        let json = take(out);
        serde_json::from_str(&json)
            .map_err(|e| AppError::new(ErrorCode::Network, format!("parsing {label} status: {e}")))
    }

    /// Tailscale: ask for a fresh login URL (shows up in the status).
    pub fn request_login(&self) -> Result<(), AppError> {
        if self.network != Network::Tailscale {
            return Err(AppError::internal("request_login is Tailscale-only"));
        }
        let mut err = std::ptr::null_mut();
        if unsafe { ffi::ts_login(self.h, &mut err) } != 0 {
            return Err(AppError::new(
                ErrorCode::Network,
                format!("Tailscale login: {}", take(err)),
            ));
        }
        Ok(())
    }

    /// Tailscale: log this node out at the control server.
    pub fn logout_tailscale(&self) -> Result<(), AppError> {
        if self.network != Network::Tailscale {
            return Err(AppError::internal("logout_tailscale is Tailscale-only"));
        }
        let mut err = std::ptr::null_mut();
        if unsafe { ffi::ts_logout(self.h, &mut err) } != 0 {
            return Err(AppError::new(
                ErrorCode::Network,
                format!("Tailscale logout: {}", take(err)),
            ));
        }
        Ok(())
    }

    /// Open a TCP connection to `addr` ("ip:port") through the overlay.
    pub fn dial_tcp(&self, addr: &str, timeout: Duration) -> Result<OwnedFd, DialError> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(DialError::NotRunning);
        }
        let addr_c = CString::new(addr).map_err(|_| DialError::Other("bad address".into()))?;
        let ms = timeout.as_millis().min(i32::MAX as u128) as c_int;
        let mut fd: c_int = -1;
        let mut err = std::ptr::null_mut();
        let dial = match self.network {
            Network::Netbird => ffi::nb_dial,
            Network::Tailscale => ffi::ts_dial,
        };
        let rc = unsafe {
            dial(
                self.h,
                c"tcp".as_ptr(),
                addr_c.as_ptr(),
                ms,
                &mut fd,
                &mut err,
            )
        };
        if rc != 0 || fd < 0 {
            return Err(DialError::classify(&take(err)));
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// Stop the engine. Idempotent.
    pub fn stop(&self) {
        if !self.stopped.swap(true, Ordering::AcqRel) {
            let mut err = std::ptr::null_mut();
            let rc = match self.network {
                Network::Netbird => unsafe { ffi::nb_stop(self.h, &mut err) },
                Network::Tailscale => unsafe { ffi::ts_stop(self.h, &mut err) },
            };
            if rc != 0 {
                tracing::warn!("stopping {}: {}", self.network.label(), take(err));
            }
        }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.stop();
    }
}

fn login_or_net_error(what: &str, msg: String) -> AppError {
    if is_login_error(&msg) {
        AppError::new(ErrorCode::NotLoggedIn, format!("{what}: {msg}"))
    } else {
        AppError::new(ErrorCode::Network, format!("{what}: {msg}"))
    }
}

fn make_private_dir(dir: &Path) -> Result<(), AppError> {
    std::fs::create_dir_all(dir)
        .map_err(|e| AppError::new(ErrorCode::Io, format!("creating {}: {e}", dir.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// A pipe whose read end feeds `tracing` (target `netbird` or `tailscale`).
/// Returns the write fd, which Go owns from then on.
fn pipe_logs(target: &'static str) -> Option<c_int> {
    let mut fds = [0 as c_int; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return None;
    }
    for fd in fds {
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    let reader = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    std::thread::Builder::new()
        .name(format!("{target}-log"))
        .spawn(move || {
            for line in BufReader::new(reader).lines() {
                match line {
                    Ok(l) if l.trim().is_empty() => {}
                    Ok(l) if target == "tailscale" => tracing::debug!(target: "tailscale", "{l}"),
                    Ok(l) => tracing::debug!(target: "netbird", "{l}"),
                    Err(_) => break,
                }
            }
        })
        .ok()?;
    Some(fds[1])
}

// ---- Browser (SSO) login ----------------------------------------------------

/// A browser login in progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SsoFlow {
    pub id: i32,
    /// "pkce" (browser + localhost callback) or "device" (enter a code).
    pub kind: String,
    pub url: String,
    #[serde(default)]
    pub user_code: Option<String>,
    pub expires_in: i64,
}

pub fn sso_begin(management_url: &str) -> Result<SsoFlow, AppError> {
    let url = cstring(management_url, "management URL")?;
    let mut out = std::ptr::null_mut();
    let mut err = std::ptr::null_mut();
    if unsafe { ffi::nb_sso_begin(url.as_ptr(), &mut out, &mut err) } != 0 {
        return Err(AppError::new(ErrorCode::Network, take(err)));
    }
    serde_json::from_str(&take(out))
        .map_err(|e| AppError::new(ErrorCode::Network, format!("parsing login info: {e}")))
}

/// Blocks until the browser login completes; returns the token.
pub fn sso_wait(id: i32) -> Result<String, AppError> {
    let mut out = std::ptr::null_mut();
    let mut err = std::ptr::null_mut();
    if unsafe { ffi::nb_sso_wait(id, &mut out, &mut err) } != 0 {
        return Err(AppError::new(ErrorCode::NotLoggedIn, take(err)));
    }
    Ok(take(out))
}

pub fn sso_cancel(id: i32) {
    unsafe { ffi::nb_sso_cancel(id) };
}

/// Tell the management server we logged out (uses the saved key).
pub fn logout(management_url: &str, state_dir: &Path) -> Result<(), AppError> {
    let url = cstring(management_url, "management URL")?;
    let path = cstring(
        &NodeConfig::config_path(state_dir).to_string_lossy(),
        "config path",
    )?;
    let mut err = std::ptr::null_mut();
    if unsafe { ffi::nb_logout(url.as_ptr(), path.as_ptr(), &mut err) } != 0 {
        return Err(AppError::new(ErrorCode::Network, take(err)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_errors() {
        assert_eq!(
            DialError::classify("dial tcp 100.1.2.3:5555: connect: connection refused"),
            DialError::Refused
        );
        assert_eq!(
            DialError::classify("connection was refused"),
            DialError::Refused
        );
        assert_eq!(
            DialError::classify("dial: context deadline exceeded"),
            DialError::Timeout
        );
        assert!(matches!(
            DialError::classify("no route to host"),
            DialError::Unreachable(_)
        ));
        assert_eq!(
            DialError::classify("client not started"),
            DialError::NotRunning
        );
        assert!(matches!(DialError::classify("weird"), DialError::Other(_)));
    }

    #[test]
    fn login_errors() {
        assert!(is_login_error(
            "login: rpc error: code = PermissionDenied desc = peer login has expired, please log in once more"
        ));
        assert!(is_login_error(
            "login: rpc error: code = Unauthenticated desc = invalid setup key"
        ));
        assert!(is_login_error(
            "no saved NetBird login (open config.json: no such file or directory)"
        ));
        assert!(!is_login_error(
            "create auth client: dial tcp: lookup api.netbird.io: no such host"
        ));
        assert!(is_login_error("invalid key: unable to validate API key"));
    }

    #[test]
    fn tailscale_login_needs_a_profile() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("tailscaled.state");
        assert!(!NodeConfig::has_login(Network::Tailscale, dir.path()));
        std::fs::write(&state, r#"{"_machinekey":"bWs="}"#).unwrap();
        assert!(!NodeConfig::has_login(Network::Tailscale, dir.path()));
        std::fs::write(
            &state,
            r#"{"_machinekey":"bWs=","_current-profile":"MWIy","_profiles":"e30="}"#,
        )
        .unwrap();
        assert!(NodeConfig::has_login(Network::Tailscale, dir.path()));
        assert!(!NodeConfig::has_login(Network::Netbird, dir.path()));
    }

    #[test]
    fn sso_flow_parses() {
        let f: SsoFlow = serde_json::from_str(
            r#"{"id":3,"kind":"pkce","url":"https://idp/authorize?x","expires_in":299}"#,
        )
        .unwrap();
        assert_eq!(f.kind, "pkce");
        assert_eq!(f.user_code, None);
    }
}
