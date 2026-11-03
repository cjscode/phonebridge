//! The app's own overlay-network device (NetBird peer or Tailscale node,
//! userspace, in-process) and a dialer over it.

pub mod node;
pub mod status;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{Notify, watch};

pub use node::{Credential, DialError, Node, NodeConfig, SsoFlow};
pub use status::{NetSnapshot, NetState, Network, Peer, PeerLink};

use crate::connection::backoff;
use crate::error::{AppError, ErrorCode};
use crate::forwarder::{BoxFuture, BoxStream, Dialer};

/// NetBird Cloud. Self-hosted users change this in Settings.
pub const DEFAULT_MANAGEMENT_URL: &str = "https://api.netbird.io:443";
/// Tailscale's control server. Headscale users change this in Settings.
pub const DEFAULT_CONTROL_URL: &str = "https://controlplane.tailscale.com";

const START_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a Tailscale browser login waits for its URL.
const LOGIN_URL_TIMEOUT: Duration = Duration::from_secs(30);

/// Owns the current overlay client (NetBird or Tailscale), logs in, polls
/// status and publishes snapshots.
pub struct NetManager {
    /// Each network keeps its login in `<base_dir>/<network>`.
    base_dir: PathBuf,
    device_name: String,
    network: Mutex<Network>,
    /// Management (NetBird) or control (Tailscale/Headscale) server URL.
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
pub(crate) fn friendly_login_reason(network: Network, msg: &str) -> String {
    let m = msg.to_ascii_lowercase();
    let label = network.label();
    if m.contains("expired") {
        format!("Your {label} login expired. Sign in again.")
    } else if m.contains("setup key")
        || m.contains("setupkey")
        || m.contains("invalid key")
        || m.contains("auth key")
        || m.contains("authkey")
    {
        format!(
            "{label} rejected the {} (wrong, used up, revoked or expired).",
            network.key_name()
        )
    } else if m.contains("approval") {
        format!("This Mac is waiting for approval in the {label} admin console.")
    } else if m.contains("no saved netbird login") || m.contains("no private key") {
        "Not logged in yet.".into()
    } else {
        msg.to_string()
    }
}

impl NetManager {
    /// `base_dir` holds one folder per network; `management_url` is the
    /// server for `network` (empty = its default).
    pub fn new(
        base_dir: impl Into<PathBuf>,
        device_name: impl Into<String>,
        network: Network,
        management_url: impl Into<String>,
    ) -> Arc<Self> {
        let (tx, _rx) = watch::channel(NetSnapshot::of(network, NetState::Starting));
        let url: String = management_url.into();
        let url = if url.trim().is_empty() {
            network.default_server_url().to_string()
        } else {
            url
        };
        Arc::new(Self {
            base_dir: base_dir.into(),
            device_name: device_name.into(),
            network: Mutex::new(network),
            management_url: Mutex::new(url),
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

    pub fn network(&self) -> Network {
        *self.network.lock().unwrap()
    }

    /// This network's login folder.
    pub fn state_dir(&self) -> PathBuf {
        self.base_dir.join(self.network().dir_name())
    }

    pub fn base_dir(&self) -> &Path {
        &self.base_dir
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

    /// True if an earlier login saved this device's identity.
    pub fn has_saved_login(&self) -> bool {
        NodeConfig::has_login(self.network(), &self.state_dir())
    }

    fn set_state(&self, state: NetState) {
        self.tx.send_replace(NetSnapshot::of(self.network(), state));
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
        let network = self.network();
        let cfg = NodeConfig {
            network,
            device_name: self.device_name.clone(),
            management_url: self.management_url(),
            state_dir: self.state_dir(),
            credential: cred.clone(),
        };
        let res = tokio::task::spawn_blocking(move || -> Result<Arc<Node>, AppError> {
            let node = Node::new(&cfg)?;
            node.start(START_TIMEOUT)?;
            // Tailscale doesn't fail on a bad auth key; it reports it and
            // waits for a login. Turn that into an error here.
            if network == Network::Tailscale && matches!(cfg.credential, Credential::SetupKey(_)) {
                let st = node.raw_status()?;
                if st.backend_state != "Running" {
                    node.stop();
                    let why = if st.management_error.is_empty() {
                        format!("didn't finish joining (state {})", st.backend_state)
                    } else {
                        st.management_error
                    };
                    return Err(AppError::new(
                        ErrorCode::NotLoggedIn,
                        format!("Tailscale auth key: {why}"),
                    ));
                }
            }
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
                tracing::info!("{network} client started");
                Ok(())
            }
            Err(e) if e.code == ErrorCode::NotLoggedIn => {
                tracing::warn!("{network} login needed: {e}");
                self.set_state(NetState::NeedsLogin {
                    auth_url: None,
                    user_code: None,
                    reason: Some(friendly_login_reason(network, &e.message)),
                });
                Err(e)
            }
            Err(e) => {
                tracing::warn!("{network} start failed: {e}");
                self.set_state(NetState::Error {
                    message: e.message.clone(),
                    retry_in_secs: None,
                });
                Err(e)
            }
        }
    }

    /// Start a browser login. Returns the URL to open (the status shows it too);
    /// finishing the login in the browser brings the network up automatically.
    pub async fn login_browser(self: &Arc<Self>) -> Result<SsoFlow, AppError> {
        if self.network() == Network::Tailscale {
            return self.login_browser_tailscale().await;
        }
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

    /// Tailscale's node does the login itself: start it (or ask the running
    /// one for a new URL), then wait for the URL to show up in its status.
    async fn login_browser_tailscale(self: &Arc<Self>) -> Result<SsoFlow, AppError> {
        if self.snapshot().is_running() {
            return Err(
                AppError::new(ErrorCode::BadInput, "Already signed in to Tailscale")
                    .hint("To use another account, log out in Settings first."),
            );
        }
        match self.node() {
            Some(node) => {
                tokio::task::spawn_blocking(move || node.request_login())
                    .await
                    .map_err(|e| AppError::internal(e.to_string()))??;
            }
            // A fresh node with no saved login asks for a browser login by itself.
            None => self.run(Credential::SavedKey).await?,
        }
        let deadline = tokio::time::Instant::now() + LOGIN_URL_TIMEOUT;
        loop {
            let snap = self.refresh().await;
            match snap.state {
                NetState::NeedsLogin {
                    auth_url: Some(url),
                    ..
                } => {
                    return Ok(SsoFlow {
                        id: 0,
                        kind: "tailscale".into(),
                        url,
                        user_code: None,
                        expires_in: 0,
                    });
                }
                NetState::Running => {
                    return Ok(SsoFlow {
                        id: 0,
                        kind: "tailscale".into(),
                        url: String::new(),
                        user_code: None,
                        expires_in: 0,
                    });
                }
                _ if tokio::time::Instant::now() >= deadline => {
                    return Err(AppError::new(
                        ErrorCode::Network,
                        "Tailscale didn't send a login link",
                    )
                    .hint("Check your internet connection (and the control server URL if you use Headscale), then try again."));
                }
                _ => tokio::time::sleep(Duration::from_millis(500)).await,
            }
        }
    }

    fn cancel_sso(&self) {
        if let Some(id) = self.sso.lock().unwrap().take() {
            node::sso_cancel(id);
        }
    }

    /// Join with a NetBird setup key or Tailscale auth key (replaces any saved
    /// identity).
    pub async fn login_with_setup_key(self: &Arc<Self>, key: String) -> Result<(), AppError> {
        let key = key.trim().to_string();
        let network = self.network();
        if key.len() < 16 || key.contains(char::is_whitespace) {
            let hint = match network {
                Network::Netbird => {
                    "Create one in the NetBird dashboard under Setup Keys, then paste it here."
                }
                Network::Tailscale => {
                    "Create one in the Tailscale admin console under Settings → Keys (it starts with tskey-auth-), then paste it here."
                }
            };
            return Err(AppError::new(
                ErrorCode::BadInput,
                format!("That doesn't look like a {network} {}", network.key_name()),
            )
            .hint(hint));
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
                format!("Not logged in to {} yet", self.network()),
            ))
        }
    }

    /// Log out, forget this device's identity, and wait for a new login.
    pub async fn logout(self: &Arc<Self>) -> Result<(), AppError> {
        self.cancel_sso();
        self.bump();
        let network = self.network();
        let res = match (network, self.node()) {
            (Network::Netbird, _) if self.has_saved_login() => {
                let (url, dir) = (self.management_url(), self.state_dir());
                tokio::task::spawn_blocking(move || node::logout(&url, &dir)).await
            }
            (Network::Tailscale, Some(node)) => {
                tokio::task::spawn_blocking(move || node.logout_tailscale()).await
            }
            _ => Ok(Ok(())),
        };
        match res {
            Ok(Err(e)) => tracing::warn!("{network} logout request failed (continuing): {e}"),
            Err(e) => tracing::warn!("{network} logout task failed: {e}"),
            Ok(Ok(())) => {}
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

    /// Point at another server (self-hosted NetBird, or Headscale). Logs out
    /// if it changes. An empty URL means the network's default.
    pub async fn set_management_url(self: &Arc<Self>, url: String) -> Result<(), AppError> {
        let network = self.network();
        let mut url = url.trim().trim_end_matches('/').to_string();
        if url.is_empty() {
            url = network.default_server_url().to_string();
        }
        if !(url.starts_with("https://") || url.starts_with("http://")) || url.len() < 10 {
            let default = match network {
                Network::Netbird => format!("NetBird Cloud is {DEFAULT_MANAGEMENT_URL}"),
                Network::Tailscale => format!("Tailscale's own server is {DEFAULT_CONTROL_URL}"),
            };
            return Err(AppError::new(
                ErrorCode::BadInput,
                "The server URL must start with https://",
            )
            .hint(default));
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

    /// Switch to another network. Its own saved login (if any) is used; the
    /// other network's login is kept for switching back.
    pub async fn set_network(
        self: &Arc<Self>,
        network: Network,
        management_url: String,
    ) -> Result<(), AppError> {
        self.cancel_sso();
        self.bump();
        self.close_node().await;
        let url = if management_url.trim().is_empty() {
            network.default_server_url().to_string()
        } else {
            management_url.trim().trim_end_matches('/').to_string()
        };
        *self.network.lock().unwrap() = network;
        *self.management_url.lock().unwrap() = url;
        self.set_state(NetState::Starting);
        self.start().await
    }

    fn wipe_state(&self) -> Result<(), AppError> {
        let dir = self.state_dir();
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(AppError::new(
                ErrorCode::Io,
                format!("removing {}: {e}", dir.display()),
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
        let network = node.network();
        let res = tokio::task::spawn_blocking(move || node.raw_status()).await;
        if self.node().is_none() || self.network() != network {
            return false; // closed meanwhile; don't overwrite the newer state
        }
        let snap = match res {
            Ok(Ok(raw)) => raw.into_snapshot(network),
            Ok(Err(e)) => NetSnapshot::of(
                network,
                NetState::Error {
                    message: e.message,
                    retry_in_secs: None,
                },
            ),
            Err(e) => NetSnapshot::of(
                network,
                NetState::Error {
                    message: e.to_string(),
                    retry_in_secs: None,
                },
            ),
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

/// Dials TCP through the manager's current overlay client.
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

    fn manager(dir: &Path, network: Network) -> Arc<NetManager> {
        NetManager::new(dir, "pb-test", network, "")
    }

    #[test]
    fn friendly_reasons() {
        let nb = Network::Netbird;
        assert!(
            friendly_login_reason(
                nb,
                "rpc error: code = PermissionDenied desc = peer login has expired"
            )
            .contains("NetBird login expired")
        );
        assert!(friendly_login_reason(nb, "invalid setup key").contains("setup key"));
        assert_eq!(
            friendly_login_reason(nb, "something else"),
            "something else"
        );
        assert_eq!(
            friendly_login_reason(
                Network::Tailscale,
                "invalid key: unable to validate API key"
            ),
            "Tailscale rejected the auth key (wrong, used up, revoked or expired)."
        );
    }

    #[tokio::test]
    async fn no_saved_login_waits_for_user() {
        let dir = tempfile::tempdir().unwrap();
        let m = manager(dir.path(), Network::Netbird);
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
        let m = manager(dir.path(), Network::Netbird);
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
        let m = manager(dir.path(), Network::Netbird);
        let err = m
            .dialer()
            .dial("100.64.0.1:5555".parse().unwrap())
            .await
            .err()
            .unwrap();
        assert_eq!(err, DialError::NotRunning);
    }

    #[tokio::test]
    async fn each_network_has_its_own_login_and_server() {
        let dir = tempfile::tempdir().unwrap();
        let m = manager(dir.path(), Network::Netbird);
        assert_eq!(m.management_url(), DEFAULT_MANAGEMENT_URL);
        assert_eq!(m.state_dir(), dir.path().join("netbird"));
        // A saved NetBird login doesn't count for Tailscale.
        std::fs::create_dir_all(dir.path().join("netbird")).unwrap();
        std::fs::write(dir.path().join("netbird/config.json"), "{}").unwrap();
        assert!(m.has_saved_login());

        // No Tailscale login yet: switching waits for one (doesn't start a node).
        m.set_network(Network::Tailscale, String::new())
            .await
            .unwrap();
        assert_eq!(m.network(), Network::Tailscale);
        assert_eq!(m.management_url(), DEFAULT_CONTROL_URL);
        assert_eq!(m.state_dir(), dir.path().join("tailscale"));
        assert!(!m.has_saved_login());
        let snap = m.snapshot();
        assert_eq!(snap.kind, Network::Tailscale);
        assert!(matches!(
            snap.state,
            NetState::NeedsLogin { auth_url: None, .. }
        ));
        assert!(m.node().is_none());

        // Headscale: any https URL; empty means Tailscale's own server.
        m.set_management_url("https://headscale.example.com/".into())
            .await
            .unwrap();
        assert_eq!(m.management_url(), "https://headscale.example.com");
        m.set_management_url(String::new()).await.unwrap();
        assert_eq!(m.management_url(), DEFAULT_CONTROL_URL);
        let err = m.login_with_setup_key("short".into()).await.unwrap_err();
        assert!(
            err.message.contains("Tailscale auth key"),
            "{}",
            err.message
        );

        // The NetBird login is still there when switching back.
        m.set_network(Network::Netbird, String::new()).await.ok();
        assert!(m.has_saved_login());
        m.shutdown().await;
    }
}
