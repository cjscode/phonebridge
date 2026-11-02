//! The app's own tailnet node (userspace, in-process) and a dialer over it.

pub mod localapi;
pub mod node;
pub mod status;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{Notify, watch};

pub use node::{DialError, Loopback, Node, NodeConfig};
pub use status::{Peer, PeerLink, TailnetSnapshot, TailnetState};

use crate::error::{AppError, ErrorCode};
use crate::forwarder::{BoxFuture, BoxStream, Dialer};

/// Owns the current node, polls its status and publishes snapshots.
pub struct TailnetManager {
    state_dir: PathBuf,
    hostname: String,
    node: Mutex<Option<Arc<Node>>>,
    tx: watch::Sender<TailnetSnapshot>,
    /// Wakes the poll loop early (after start/logout).
    poke: Arc<Notify>,
    poller_started: AtomicBool,
}

impl TailnetManager {
    pub fn new(state_dir: impl Into<PathBuf>, hostname: impl Into<String>) -> Arc<Self> {
        let (tx, _rx) = watch::channel(TailnetSnapshot::starting());
        Arc::new(Self {
            state_dir: state_dir.into(),
            hostname: hostname.into(),
            node: Mutex::new(None),
            tx,
            poke: Arc::new(Notify::new()),
            poller_started: AtomicBool::new(false),
        })
    }

    pub fn subscribe(&self) -> watch::Receiver<TailnetSnapshot> {
        self.tx.subscribe()
    }

    pub fn snapshot(&self) -> TailnetSnapshot {
        self.tx.borrow().clone()
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    pub fn node(&self) -> Option<Arc<Node>> {
        self.node.lock().unwrap().clone()
    }

    /// Create and start the node (reusing saved state). Spawns the status poller
    /// the first time. Must be called inside a Tokio runtime.
    pub async fn start(self: &Arc<Self>) -> Result<(), AppError> {
        self.start_with(None).await
    }

    async fn start_with(self: &Arc<Self>, auth_key: Option<String>) -> Result<(), AppError> {
        self.close_node().await;
        self.tx.send_replace(TailnetSnapshot::starting());
        let cfg = NodeConfig { state_dir: self.state_dir.clone(), hostname: self.hostname.clone(), auth_key };
        let node = tokio::task::spawn_blocking(move || -> Result<Arc<Node>, AppError> {
            let node = Node::new(&cfg)?;
            node.start()?;
            Ok(Arc::new(node))
        })
        .await
        .map_err(|e| AppError::internal(e.to_string()))?;
        let node = match node {
            Ok(n) => n,
            Err(e) => {
                self.tx.send_replace(TailnetSnapshot::with_state(TailnetState::Error { message: e.message.clone() }));
                return Err(e);
            }
        };
        *self.node.lock().unwrap() = Some(node);
        if !self.poller_started.swap(true, Ordering::AcqRel) {
            self.spawn_poller();
        }
        self.poke.notify_one();
        Ok(())
    }

    fn spawn_poller(self: &Arc<Self>) {
        let me = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let Some(me) = me.upgrade() else { return };
                let snap = me.poll_once().await;
                let delay = match &snap {
                    Some(s) if s.is_running() => Duration::from_secs(3),
                    _ => Duration::from_secs(1),
                };
                if let Some(s) = snap {
                    me.tx.send_if_modified(|cur| {
                        if *cur != s {
                            *cur = s;
                            true
                        } else {
                            false
                        }
                    });
                }
                let poke = me.poke.clone();
                drop(me);
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = poke.notified() => {}
                }
            }
        });
    }

    /// One status read. `None` means "no node right now" (keep current snapshot).
    async fn poll_once(&self) -> Option<TailnetSnapshot> {
        let node = self.node()?;
        let res = tokio::task::spawn_blocking(move || node.status()).await;
        Some(match res {
            Ok(Ok(s)) => s,
            Ok(Err(e)) if self.node().is_none() => {
                tracing::debug!("status after close: {e}");
                return None;
            }
            Ok(Err(e)) => TailnetSnapshot::with_state(TailnetState::Error { message: e.message }),
            Err(e) => TailnetSnapshot::with_state(TailnetState::Error { message: e.to_string() }),
        })
    }

    /// Force a status refresh now and return it.
    pub async fn refresh(&self) -> TailnetSnapshot {
        if let Some(s) = self.poll_once().await {
            self.tx.send_replace(s);
        }
        self.snapshot()
    }

    /// Replace the node with one that joins using `auth_key`. Clears saved state
    /// first, since tsnet ignores auth keys once it has state.
    pub async fn login_with_auth_key(self: &Arc<Self>, auth_key: String) -> Result<(), AppError> {
        let key = auth_key.trim().to_string();
        if !key.starts_with("tskey-") {
            return Err(AppError::new(ErrorCode::BadInput, "That doesn't look like a Tailscale auth key")
                .hint("Auth keys start with \"tskey-\". Create one at login.tailscale.com/admin/settings/keys."));
        }
        self.close_node().await;
        self.wipe_state()?;
        self.start_with(Some(key)).await
    }

    /// Log out of the tailnet, forget the saved node, and start fresh (which
    /// yields a new login URL).
    pub async fn logout(self: &Arc<Self>) -> Result<(), AppError> {
        if let Some(node) = self.node() {
            let res = tokio::task::spawn_blocking(move || {
                let lb = node.loopback()?;
                localapi::logout(&lb)
            })
            .await
            .map_err(|e| AppError::internal(e.to_string()))?;
            if let Err(e) = res {
                // Still wipe local state below so the user ends up logged out locally.
                tracing::warn!("LocalAPI logout failed: {e}");
            }
        }
        self.close_node().await;
        self.wipe_state()?;
        self.start_with(None).await
    }

    fn wipe_state(&self) -> Result<(), AppError> {
        match std::fs::remove_dir_all(&self.state_dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(AppError::new(ErrorCode::Io, format!("removing {}: {e}", self.state_dir.display()))),
        }
    }

    async fn close_node(&self) {
        let old = self.node.lock().unwrap().take();
        if let Some(node) = old {
            let _ = tokio::task::spawn_blocking(move || node.close()).await;
        }
    }

    /// Stop the node (on quit).
    pub async fn shutdown(&self) {
        self.close_node().await;
        self.tx.send_replace(TailnetSnapshot::with_state(TailnetState::Stopped));
    }

    pub fn dialer(self: &Arc<Self>) -> Arc<TailnetDialer> {
        Arc::new(TailnetDialer { manager: self.clone(), timeout: Duration::from_secs(10) })
    }
}

/// Dials TCP through the manager's current node.
pub struct TailnetDialer {
    manager: Arc<TailnetManager>,
    timeout: Duration,
}

impl Dialer for TailnetDialer {
    fn dial(&self, addr: SocketAddr) -> BoxFuture<Result<BoxStream, DialError>> {
        let node = self.manager.node();
        let timeout = self.timeout;
        Box::pin(async move {
            let node = node.ok_or(DialError::NotRunning)?;
            let target = addr.to_string();
            // If we time out, the blocking call keeps running; its fd is dropped
            // (closed) whenever it finishes.
            let task = tokio::task::spawn_blocking(move || node.dial_tcp(&target));
            let fd = match tokio::time::timeout(timeout, task).await {
                Err(_) => return Err(DialError::Timeout),
                Ok(Err(join)) => return Err(DialError::Other(join.to_string())),
                Ok(Ok(res)) => res?,
            };
            let std_stream = std::os::unix::net::UnixStream::from(fd);
            std_stream.set_nonblocking(true).map_err(|e| DialError::Other(e.to_string()))?;
            let stream =
                tokio::net::UnixStream::from_std(std_stream).map_err(|e| DialError::Other(e.to_string()))?;
            Ok(Box::new(stream) as BoxStream)
        })
    }
}
