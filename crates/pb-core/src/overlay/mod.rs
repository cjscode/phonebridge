//! The app's own NetBird peer (userspace, in-process) and a dialer over it.

pub mod node;
pub mod status;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{Notify, watch};

pub use node::{Credential, DialError, Node, NodeConfig, SsoFlow};
pub use status::{NetSnapshot, NetState, Peer, PeerLink};

use crate::connection::backoff;
use crate::error::{AppError, ErrorCode};
use crate::forwarder::{BoxFuture, BoxStream, Dialer};

/// NetBird Cloud. Self-hosted users change this in Settings.
pub const DEFAULT_MANAGEMENT_URL: &str = "https://api.netbird.io:443";

const START_TIMEOUT: Duration = Duration::from_secs(60);

/// Owns the current NetBird client, logs in, polls status and publishes snapshots.
pub struct NetManager {
    state_dir: PathBuf,
    device_name: String,
    management_url: Mutex<String>,
    node: Mutex<Option<Arc<Node>>>,
    tx: watch::Sender<NetSnapshot>,
    poke: Arc<Notify>,
    poller_started: AtomicBool,
    /// Bumped by every start/login/logout; older background tasks see the
    /// change and stop.
    generation: AtomicU64,
    sso: Mutex<Option<i32>>,
}

/// Turn a login error into something a person can act on.
fn friendly_login_reason(msg: &str) -> String {
    let m = msg.to_ascii_lowercase();
    if m.contains("expired") {
        "Your NetBird login expired. Sign in again.".into()
    } else if m.contains("setup key") || m.contains("setupkey") {
        "NetBird rejected the setup key (wrong, used up, revoked or expired).".into()
    } else if m.contains("approval") {
        "This Mac is waiting for approval in the NetBird dashboard.".into()
    } else if m.contains("no saved netbird login") || m.contains("no private key") {
        "Not logged in yet.".into()
    } else {
        msg.to_string()
    }
}

impl NetManager {
    pub fn new(
        state_dir: impl Into<PathBuf>,
        device_name: impl Into<String>,
        management_url: impl Into<String>,
    ) -> Arc<Self> {
        let (tx, _rx) = watch::channel(NetSnapshot::starting());
        Arc::new(Self {
            state_dir: state_dir.into(),
            device_name: device_name.into(),
            management_url: Mutex::new(management_url.into()),
            node: Mutex::new(None),
            tx,
            poke: Arc::new(Notify::new()),
            poller_started: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            sso: Mutex::new(None),
        })
    }

    pub fn subscribe(&self) -> watch::Receiver<NetSnapshot> {
        self.tx.subscribe()
    }

    pub fn snapshot(&self) -> NetSnapshot {
        self.tx.borrow().clone()
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    pub fn management_url(&self) -> String {
        self.management_url.lock().unwrap().clone()
    }

    pub fn node(&self) -> Option<Arc<Node>> {
        self.node.lock().unwrap().clone()
    }

    /// True if an earlier login saved this peer's identity.
    pub fn has_saved_login(&self) -> bool {
        NodeConfig::config_path(&self.state_dir).is_file()
    }

    fn set_state(&self, state: NetState) {
        self.tx.send_replace(NetSnapshot::with_state(state));
    }

    fn bump(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn current(&self, generation: u64) -> bool {
        self.generation.load(Ordering::Acquire) == generation
    }

    /// On launch: reuse the saved login, or wait for the user to log in.
    pub async fn start(self: &Arc<Self>) -> Result<(), AppError> {
        self.ensure_poller();
        if self.has_saved_login() {
            self.run(Credential::SavedKey).await
        } else {
            self.set_state(NetState::NeedsLogin {
                auth_url: None,
                user_code: None,
                reason: None,
            });
            Ok(())
        }
    }

    /// Start the client with `cred`. Returns the first attempt's result; on a
    /// network-type failure it keeps retrying in the background (with backoff)
    /// until it works or something newer (login/logout) supersedes it.
    async fn run(self: &Arc<Self>, cred: Credential) -> Result<(), AppError> {
        self.ensure_poller();
        let generation = self.bump();
        self.close_node().await;
        self.set_state(NetState::Starting);
        let first = self.attempt(generation, &cred).await;
        if let Err(e) = &first
            && e.code != ErrorCode::NotLoggedIn
            && self.current(generation)
        {
            let me = self.clone();
            tokio::spawn(async move {
                let mut attempt = 1u32;
                loop {
                    let delay = backoff(Duration::from_secs(5), Duration::from_secs(60), attempt);
                    if let NetState::Error { message, .. } = me.snapshot().state {
                        me.set_state(NetState::Error {
                            message,
                            retry_in_secs: Some(delay.as_secs()),
                        });
                    }
                    tokio::time::sleep(delay).await;
                    if !me.current(generation) {
                        return;
                    }
                    match me.attempt(generation, &cred).await {
                        Ok(()) => return,
                        Err(e) if e.code == ErrorCode::NotLoggedIn => return,
                        Err(_) => attempt += 1,
                    }
                }
            });
        }
        first
    }

    async fn attempt(self: &Arc<Self>, generation: u64, cred: &Credential) -> Result<(), AppError> {
        let cfg = NodeConfig {
            device_name: self.device_name.clone(),
            management_url: self.management_url(),
            state_dir: self.state_dir.clone(),
            credential: cred.clone(),
        };
        let res = tokio::task::spawn_blocking(move || -> Result<Arc<Node>, AppError> {
            let node = Node::new(&cfg)?;
            node.start(START_TIMEOUT)?;
            Ok(Arc::new(node))
        })
        .await
        .map_err(|e| AppError::internal(e.to_string()))
        .and_then(|r| r);

        if !self.current(generation) {
            if let Ok(node) = res {
                let _ = tokio::task::spawn_blocking(move || node.stop()).await;
            }
            return Err(AppError::new(
                ErrorCode::Network,
                "superseded by a newer login",
            ));
        }
        match res {
            Ok(node) => {
                *self.node.lock().unwrap() = Some(node);
                self.poke.notify_one();
                tracing::info!("NetBird is up");
                Ok(())
            }
            Err(e) if e.code == ErrorCode::NotLoggedIn => {
                tracing::warn!("NetBird login needed: {e}");
                self.set_state(NetState::NeedsLogin {
                    auth_url: None,
                    user_code: None,
                    reason: Some(friendly_login_reason(&e.message)),
                });
                Err(e)
            }
            Err(e) => {
                tracing::warn!("NetBird start failed: {e}");
                self.set_state(NetState::Error {
                    message: e.message.clone(),
                    retry_in_secs: None,
                });
                Err(e)
            }
        }
    }

    /// Start a browser (SSO) login. Returns the URL to open; finishing the login
    /// in the browser brings NetBird up automatically.
    pub async fn login_browser(self: &Arc<Self>) -> Result<SsoFlow, AppError> {
        self.cancel_sso();
        let url = self.management_url();
        let flow = tokio::task::spawn_blocking(move || node::sso_begin(&url))
            .await
            .map_err(|e| AppError::internal(e.to_string()))??;
        *self.sso.lock().unwrap() = Some(flow.id);
        let generation = self.bump();
        self.set_state(NetState::NeedsLogin {
            auth_url: Some(flow.url.clone()),
            user_code: flow.user_code.clone(),
            reason: None,
        });
        let me = self.clone();
        let id = flow.id;
        tokio::spawn(async move {
            let res = tokio::task::spawn_blocking(move || node::sso_wait(id)).await;
            {
                let mut sso = me.sso.lock().unwrap();
                if *sso == Some(id) {
                    *sso = None;
                }
            }
            if !me.current(generation) {
                return;
            }
            match res {
                Ok(Ok(token)) => {
                    let _ = me.run(Credential::Jwt(token)).await;
                }
                Ok(Err(e)) => me.set_state(NetState::NeedsLogin {
                    auth_url: None,
                    user_code: None,
                    reason: Some(e.message),
                }),
                Err(e) => me.set_state(NetState::NeedsLogin {
                    auth_url: None,
                    user_code: None,
                    reason: Some(e.to_string()),
                }),
            }
        });
        Ok(flow)
    }

    fn cancel_sso(&self) {
        if let Some(id) = self.sso.lock().unwrap().take() {
            node::sso_cancel(id);
        }
    }

    /// Join with a setup key (replaces any saved identity).
    pub async fn login_with_setup_key(self: &Arc<Self>, key: String) -> Result<(), AppError> {
        let key = key.trim().to_string();
        if key.len() < 16 || key.contains(char::is_whitespace) {
            return Err(AppError::new(
                ErrorCode::BadInput,
                "That doesn't look like a NetBird setup key",
            )
            .hint("Create one in the NetBird dashboard under Setup Keys, then paste it here."));
        }
        self.cancel_sso();
        self.bump();
        self.close_node().await;
        self.wipe_state()?;
        self.run(Credential::SetupKey(key)).await
    }

    /// Retry now (after an error), using the saved login.
    pub async fn retry(self: &Arc<Self>) -> Result<(), AppError> {
        if self.has_saved_login() {
            self.run(Credential::SavedKey).await
        } else {
            Err(AppError::new(
                ErrorCode::NotLoggedIn,
                "Not logged in to NetBird yet",
            ))
        }
    }

    /// Log out, forget this peer's identity, and wait for a new login.
    pub async fn logout(self: &Arc<Self>) -> Result<(), AppError> {
        self.cancel_sso();
        self.bump();
        if self.has_saved_login() {
            let (url, dir) = (self.management_url(), self.state_dir.clone());
            match tokio::task::spawn_blocking(move || node::logout(&url, &dir)).await {
                Ok(Err(e)) => tracing::warn!("NetBird logout request failed (continuing): {e}"),
                Err(e) => tracing::warn!("NetBird logout task failed: {e}"),
                Ok(Ok(())) => {}
            }
        }
        self.close_node().await;
        self.wipe_state()?;
        self.set_state(NetState::NeedsLogin {
            auth_url: None,
            user_code: None,
            reason: None,
        });
        Ok(())
    }

    /// Point at another management server (self-hosted). Logs out if it changes.
    pub async fn set_management_url(self: &Arc<Self>, url: String) -> Result<(), AppError> {
        let url = url.trim().trim_end_matches('/').to_string();
        if !(url.starts_with("https://") || url.starts_with("http://")) || url.len() < 10 {
            return Err(AppError::new(
                ErrorCode::BadInput,
                "The management URL must start with https://",
            )
            .hint(format!("NetBird Cloud is {DEFAULT_MANAGEMENT_URL}")));
        }
        if url == self.management_url() {
            return Ok(());
        }
        self.cancel_sso();
        self.bump();
        self.close_node().await;
        self.wipe_state()?;
        *self.management_url.lock().unwrap() = url;
        self.set_state(NetState::NeedsLogin {
            auth_url: None,
            user_code: None,
            reason: None,
        });
        Ok(())
    }

    fn wipe_state(&self) -> Result<(), AppError> {
        match std::fs::remove_dir_all(&self.state_dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(AppError::new(
                ErrorCode::Io,
                format!("removing {}: {e}", self.state_dir.display()),
            )),
        }
    }

    async fn close_node(&self) {
        let old = self.node.lock().unwrap().take();
        if let Some(node) = old {
            let _ = tokio::task::spawn_blocking(move || node.stop()).await;
        }
    }

    /// Stop everything (on quit).
    pub async fn shutdown(&self) {
        self.cancel_sso();
        self.bump();
        self.close_node().await;
        self.set_state(NetState::Stopped);
    }

    fn ensure_poller(self: &Arc<Self>) {
        if self.poller_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let me = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let Some(me) = me.upgrade() else { return };
                let running = me.poll_once().await;
                let delay = if running {
                    Duration::from_secs(3)
                } else {
                    Duration::from_secs(1)
                };
                let poke = me.poke.clone();
                drop(me);
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = poke.notified() => {}
                }
            }
        });
    }

    /// Publish the running node's status. Returns whether a node is running.
    async fn poll_once(&self) -> bool {
        let Some(node) = self.node() else {
            return false;
        };
        let res = tokio::task::spawn_blocking(move || node.raw_status()).await;
        if self.node().is_none() {
            return false; // closed meanwhile; don't overwrite the newer state
        }
        let snap = match res {
            Ok(Ok(raw)) => raw.into_snapshot(),
            Ok(Err(e)) => NetSnapshot::with_state(NetState::Error {
                message: e.message,
                retry_in_secs: None,
            }),
            Err(e) => NetSnapshot::with_state(NetState::Error {
                message: e.to_string(),
                retry_in_secs: None,
            }),
        };
        self.tx.send_if_modified(|cur| {
            if *cur != snap {
                *cur = snap;
                true
            } else {
                false
            }
        });
        true
    }

    /// Force a status refresh now and return it.
    pub async fn refresh(&self) -> NetSnapshot {
        self.poll_once().await;
        self.snapshot()
    }

    pub fn dialer(self: &Arc<Self>) -> Arc<NetDialer> {
        Arc::new(NetDialer {
            manager: self.clone(),
            timeout: Duration::from_secs(10),
        })
    }
}

/// Dials TCP through the manager's current NetBird client.
pub struct NetDialer {
    manager: Arc<NetManager>,
    timeout: Duration,
}

impl Dialer for NetDialer {
    fn dial(&self, addr: SocketAddr) -> BoxFuture<Result<BoxStream, DialError>> {
        let node = self.manager.node();
        let timeout = self.timeout;
        Box::pin(async move {
            let node = node.ok_or(DialError::NotRunning)?;
            let target = addr.to_string();
            // Go enforces `timeout`; the outer timeout is a safety net. A late
            // result's fd is dropped (closed) when the blocking task finishes.
            let task = tokio::task::spawn_blocking(move || node.dial_tcp(&target, timeout));
            let fd = match tokio::time::timeout(timeout + Duration::from_secs(2), task).await {
                Err(_) => return Err(DialError::Timeout),
                Ok(Err(join)) => return Err(DialError::Other(join.to_string())),
                Ok(Ok(res)) => res?,
            };
            let std_stream = std::os::unix::net::UnixStream::from(fd);
            std_stream
                .set_nonblocking(true)
                .map_err(|e| DialError::Other(e.to_string()))?;
            let stream = tokio::net::UnixStream::from_std(std_stream)
                .map_err(|e| DialError::Other(e.to_string()))?;
            Ok(Box::new(stream) as BoxStream)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn friendly_reasons() {
        assert!(
            friendly_login_reason(
                "rpc error: code = PermissionDenied desc = peer login has expired"
            )
            .contains("expired")
        );
        assert!(friendly_login_reason("invalid setup key").contains("setup key"));
        assert_eq!(friendly_login_reason("something else"), "something else");
    }

    #[tokio::test]
    async fn no_saved_login_waits_for_user() {
        let dir = tempfile::tempdir().unwrap();
        let m = NetManager::new(
            dir.path().join("netbird"),
            "pb-test",
            DEFAULT_MANAGEMENT_URL,
        );
        m.start().await.unwrap();
        assert_eq!(
            m.snapshot().state,
            NetState::NeedsLogin {
                auth_url: None,
                user_code: None,
                reason: None
            }
        );
        assert!(!m.has_saved_login());
    }

    #[tokio::test]
    async fn rejects_bad_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let m = NetManager::new(
            dir.path().join("netbird"),
            "pb-test",
            DEFAULT_MANAGEMENT_URL,
        );
        assert_eq!(
            m.login_with_setup_key("nope".into())
                .await
                .unwrap_err()
                .code,
            ErrorCode::BadInput
        );
        assert_eq!(
            m.set_management_url("ftp://x".into())
                .await
                .unwrap_err()
                .code,
            ErrorCode::BadInput
        );
        m.set_management_url("https://netbird.example.com:33073/".into())
            .await
            .unwrap();
        assert_eq!(m.management_url(), "https://netbird.example.com:33073");
    }

    #[tokio::test]
    async fn dial_without_node_is_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let m = NetManager::new(
            dir.path().join("netbird"),
            "pb-test",
            DEFAULT_MANAGEMENT_URL,
        );
        let err = m
            .dialer()
            .dial("100.64.0.1:5555".parse().unwrap())
            .await
            .err()
            .unwrap();
        assert_eq!(err, DialError::NotRunning);
    }
}
