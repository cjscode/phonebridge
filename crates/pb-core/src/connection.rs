//! Keeps the saved phone connected: NetBird → forwarder → `adb connect`, with
//! liveness checks, reconnect-with-backoff, and a status the UI can explain.
//!
//! Everything runs in one task (an actor), driven by: user commands, NetBird
//! snapshot changes, forwarder events, a keepalive tick, and a retry timer.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::time::Instant;

use crate::adb::{Adb, DeviceState};
use crate::forwarder::{Dialer, Forwarder, ForwarderEvent, ForwarderStats, bind_loopback};
use crate::overlay::{DialError, NetSnapshot, NetState, PeerLink};

/// The phone we remember between launches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhoneConfig {
    /// Stable NetBird peer ID (its WireGuard public key).
    pub node_id: String,
    pub name: String,
    /// Last known NetBird IP; refreshed from NetBird when it changes.
    pub ip: IpAddr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConnectionStatus {
    /// Nothing set up yet; run "Set up via USB".
    NoPhone,
    /// Our NetBird client isn't running (logged out, starting, Mac offline).
    NetOffline {
        detail: String,
    },
    Connecting {
        attempt: u32,
    },
    Connected {
        serial: String,
        link: Option<PeerLink>,
    },
    /// Was connected (or a step failed); trying again.
    Reconnecting {
        attempt: u32,
        retry_in_secs: u64,
        reason: String,
    },
    /// The phone is reachable but nothing listens on 5555: it rebooted, so
    /// `adb tcpip` must be re-enabled over USB.
    PhoneNeedsUsb {
        retry_in_secs: u64,
    },
    /// The phone can't be reached over NetBird (offline, asleep, NetBird off).
    PhoneUnreachable {
        detail: String,
        retry_in_secs: u64,
    },
    /// adb is waiting for "Allow USB debugging?" on the phone.
    Unauthorized,
    /// The user disconnected.
    Disconnected,
}

impl ConnectionStatus {
    pub fn is_connected(&self) -> bool {
        matches!(self, ConnectionStatus::Connected { .. })
    }

    /// One short line for the tray menu.
    pub fn summary(&self) -> String {
        match self {
            Self::NoPhone => "No phone set up".into(),
            Self::NetOffline { .. } => "NetBird offline".into(),
            Self::Connecting { .. } => "Connecting…".into(),
            Self::Connected {
                link: Some(PeerLink::Direct { .. }),
                ..
            } => "Connected (direct)".into(),
            Self::Connected {
                link: Some(PeerLink::Relayed { .. }),
                ..
            } => "Connected (relayed)".into(),
            Self::Connected { .. } => "Connected".into(),
            Self::Reconnecting { .. } => "Reconnecting…".into(),
            Self::PhoneNeedsUsb { .. } => "Phone rebooted: plug in USB once".into(),
            Self::PhoneUnreachable { .. } => "Phone not reachable over NetBird".into(),
            Self::Unauthorized => "Allow USB debugging on the phone".into(),
            Self::Disconnected => "Disconnected".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConnectionConfig {
    /// Forwarder port to reuse (keeps the adb serial stable across launches).
    pub forwarder_port: Option<u16>,
    /// adbd TCP port on the phone.
    pub phone_adb_port: u16,
    /// How often to check the adb device state while connected.
    pub keepalive: Duration,
    /// Every Nth keepalive also runs a shell round-trip through the phone.
    pub liveness_every: u32,
    pub probe_timeout: Duration,
    pub backoff_base: Duration,
    pub backoff_max: Duration,
    /// How long to wait for the device to show up after `adb connect`.
    pub connect_settle: Duration,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            forwarder_port: None,
            phone_adb_port: 5555,
            keepalive: Duration::from_secs(3),
            liveness_every: 5,
            probe_timeout: Duration::from_secs(8),
            backoff_base: Duration::from_secs(1),
            backoff_max: Duration::from_secs(30),
            connect_settle: Duration::from_secs(6),
        }
    }
}

/// 1, 2, 4, … capped.
pub fn backoff(base: Duration, max: Duration, attempt: u32) -> Duration {
    let factor = 1u32.checked_shl(attempt.min(16)).unwrap_or(u32::MAX);
    base.saturating_mul(factor).min(max)
}

enum Cmd {
    SetPhone(Option<PhoneConfig>),
    ConnectNow,
    Disconnect,
    Shutdown(oneshot::Sender<()>),
}

/// Handle used by the app to drive the connection actor.
#[derive(Clone)]
pub struct ConnectionHandle {
    cmd: mpsc::Sender<Cmd>,
    status: watch::Receiver<ConnectionStatus>,
    shared: Arc<Shared>,
}

struct Shared {
    forwarder: Mutex<Option<Arc<Forwarder>>>,
    phone: Mutex<Option<PhoneConfig>>,
}

impl ConnectionHandle {
    pub fn status(&self) -> ConnectionStatus {
        self.status.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<ConnectionStatus> {
        self.status.clone()
    }

    /// adb serial while connected.
    pub fn serial(&self) -> Option<String> {
        match &*self.status.borrow() {
            ConnectionStatus::Connected { serial, .. } => Some(serial.clone()),
            _ => None,
        }
    }

    pub fn phone(&self) -> Option<PhoneConfig> {
        self.shared.phone.lock().unwrap().clone()
    }

    pub fn forwarder_port(&self) -> Option<u16> {
        self.shared
            .forwarder
            .lock()
            .unwrap()
            .as_ref()
            .map(|f| f.local_addr().port())
    }

    pub fn forwarder_stats(&self) -> Option<ForwarderStats> {
        self.shared
            .forwarder
            .lock()
            .unwrap()
            .as_ref()
            .map(|f| f.stats())
    }

    pub async fn set_phone(&self, phone: Option<PhoneConfig>) {
        let _ = self.cmd.send(Cmd::SetPhone(phone)).await;
    }

    pub async fn connect_now(&self) {
        let _ = self.cmd.send(Cmd::ConnectNow).await;
    }

    pub async fn disconnect(&self) {
        let _ = self.cmd.send(Cmd::Disconnect).await;
    }

    /// Disconnect, stop the forwarder and our adb server. Waits for completion.
    pub async fn shutdown(&self) {
        let (tx, rx) = oneshot::channel();
        if self.cmd.send(Cmd::Shutdown(tx)).await.is_ok() {
            let _ = tokio::time::timeout(Duration::from_secs(10), rx).await;
        }
    }
}

struct Actor {
    adb: Adb,
    dialer: Arc<dyn Dialer>,
    net: watch::Receiver<NetSnapshot>,
    cfg: ConnectionConfig,
    status: watch::Sender<ConnectionStatus>,
    shared: Arc<Shared>,
    fwd_events: Option<broadcast::Receiver<ForwarderEvent>>,

    phone: Option<PhoneConfig>,
    want_connected: bool,
    attempt: u32,
    next_try: Option<Instant>,
    serial: Option<String>,
    ticks: u32,
}

/// Start the connection actor.
pub fn spawn(
    adb: Adb,
    dialer: Arc<dyn Dialer>,
    net: watch::Receiver<NetSnapshot>,
    cfg: ConnectionConfig,
    phone: Option<PhoneConfig>,
) -> ConnectionHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel(16);
    let initial = if phone.is_some() {
        ConnectionStatus::Connecting { attempt: 0 }
    } else {
        ConnectionStatus::NoPhone
    };
    let (status_tx, status_rx) = watch::channel(initial);
    let shared = Arc::new(Shared {
        forwarder: Mutex::new(None),
        phone: Mutex::new(phone.clone()),
    });
    let actor = Actor {
        adb,
        dialer,
        net,
        cfg,
        status: status_tx,
        shared: shared.clone(),
        fwd_events: None,
        want_connected: phone.is_some(),
        next_try: phone.as_ref().map(|_| Instant::now()),
        phone,
        attempt: 0,
        serial: None,
        ticks: 0,
    };
    tokio::spawn(actor.run(cmd_rx));
    ConnectionHandle {
        cmd: cmd_tx,
        status: status_rx,
        shared,
    }
}

impl Actor {
    async fn run(mut self, mut cmds: mpsc::Receiver<Cmd>) {
        let mut keepalive = tokio::time::interval(self.cfg.keepalive);
        keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let retry_at = self.next_try;
            tokio::select! {
                cmd = cmds.recv() => match cmd {
                    None => { self.teardown().await; return; }
                    Some(Cmd::Shutdown(done)) => { self.teardown().await; let _ = done.send(()); return; }
                    Some(c) => self.handle_cmd(c).await,
                },
                changed = self.net.changed() => {
                    if changed.is_err() { self.teardown().await; return; }
                    self.on_net_change().await;
                }
                _ = sleep_until_opt(retry_at) => {
                    self.next_try = None;
                    self.try_connect().await;
                }
                ev = recv_fwd(&mut self.fwd_events) => {
                    if let Some(ForwarderEvent::Closed { error, .. }) = ev
                        && self.serial.is_some() {
                            tracing::info!("adb transport connection closed ({error:?}); checking device");
                            self.check_alive(true).await;
                        }
                }
                _ = keepalive.tick() => {
                    if self.serial.is_some() {
                        self.ticks = self.ticks.wrapping_add(1);
                        let deep = self.cfg.liveness_every > 0 && self.ticks.is_multiple_of(self.cfg.liveness_every);
                        self.check_alive(deep).await;
                    }
                }
            }
        }
    }

    fn set(&self, s: ConnectionStatus) {
        self.status.send_if_modified(|cur| {
            if *cur != s {
                tracing::info!("connection: {}", s.summary());
                *cur = s;
                true
            } else {
                false
            }
        });
    }

    fn schedule(&mut self, after: Duration) {
        self.next_try = Some(Instant::now() + after);
    }

    fn schedule_backoff(&mut self) -> u64 {
        let d = backoff(self.cfg.backoff_base, self.cfg.backoff_max, self.attempt);
        self.attempt = self.attempt.saturating_add(1);
        self.schedule(d);
        d.as_secs().max(1)
    }

    async fn handle_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::SetPhone(phone) => {
                self.drop_device().await;
                *self.shared.phone.lock().unwrap() = phone.clone();
                self.phone = phone;
                self.attempt = 0;
                if self.phone.is_some() {
                    self.want_connected = true;
                    self.schedule(Duration::ZERO);
                } else {
                    self.want_connected = false;
                    self.next_try = None;
                    self.set(ConnectionStatus::NoPhone);
                }
            }
            Cmd::ConnectNow => {
                self.want_connected = true;
                self.attempt = 0;
                if self.serial.is_none() {
                    self.schedule(Duration::ZERO);
                }
            }
            Cmd::Disconnect => {
                self.want_connected = false;
                self.next_try = None;
                self.drop_device().await;
                self.set(if self.phone.is_some() {
                    ConnectionStatus::Disconnected
                } else {
                    ConnectionStatus::NoPhone
                });
            }
            Cmd::Shutdown(_) => unreachable!(),
        }
    }

    async fn on_net_change(&mut self) {
        let snap = self.net.borrow_and_update().clone();
        if !snap.is_running() {
            if self.serial.is_some() {
                self.drop_device().await;
            }
            if self.want_connected && self.phone.is_some() {
                self.next_try = None; // wait for the NetBird to come back
                self.set(ConnectionStatus::NetOffline {
                    detail: net_detail(&snap.state),
                });
            }
            return;
        }
        // NetBird is up: if we were waiting on it, try now.
        if self.want_connected
            && self.phone.is_some()
            && self.serial.is_none()
            && (matches!(
                *self.status.borrow(),
                ConnectionStatus::NetOffline { .. } | ConnectionStatus::Connecting { .. }
            ) || self.next_try.is_none())
        {
            self.attempt = 0;
            self.schedule(Duration::ZERO);
        }
        // Keep the link indicator fresh while connected.
        if let (Some(serial), Some(phone)) = (&self.serial, &self.phone) {
            let link = snap.peer_by_id(&phone.node_id).map(|p| p.link.clone());
            self.set(ConnectionStatus::Connected {
                serial: serial.clone(),
                link,
            });
        }
    }

    async fn ensure_forwarder(&mut self, target: SocketAddr) -> Result<Arc<Forwarder>, String> {
        if let Some(f) = self.shared.forwarder.lock().unwrap().clone() {
            if f.target() != target {
                f.set_target(target);
            }
            return Ok(f);
        }
        let listener = bind_loopback(self.cfg.forwarder_port)
            .await
            .map_err(|e| format!("forwarder bind: {e}"))?;
        let f = Forwarder::start(listener, target, self.dialer.clone())
            .map_err(|e| format!("forwarder: {e}"))?;
        self.cfg.forwarder_port = Some(f.local_addr().port());
        self.fwd_events = Some(f.subscribe());
        *self.shared.forwarder.lock().unwrap() = Some(f.clone());
        Ok(f)
    }

    async fn try_connect(&mut self) {
        if !self.want_connected || self.serial.is_some() {
            return;
        }
        let Some(mut phone) = self.phone.clone() else {
            self.set(ConnectionStatus::NoPhone);
            return;
        };
        let snap = self.net.borrow().clone();
        if !snap.is_running() {
            self.set(ConnectionStatus::NetOffline {
                detail: net_detail(&snap.state),
            });
            return; // on_net_change will retry
        }

        match snap.peer_by_id(&phone.node_id) {
            None if !snap.synced => {
                // NetBird hasn't received its peer list yet; check again shortly.
                self.set(ConnectionStatus::Connecting {
                    attempt: self.attempt,
                });
                self.schedule(Duration::from_secs(1).min(self.cfg.backoff_max));
                return;
            }
            None => {
                let retry = self.schedule_backoff();
                self.set(ConnectionStatus::PhoneUnreachable {
                    detail: format!("{} isn't in your NetBird network any more (removed, or blocked by an access policy)", phone.name),
                    retry_in_secs: retry,
                });
                return;
            }
            Some(peer) => {
                if let Some(ip) = peer
                    .ipv4()
                    .map(IpAddr::V4)
                    .or_else(|| peer.ips.first().copied())
                    && ip != phone.ip
                {
                    tracing::info!("phone NetBird IP changed {} -> {ip}", phone.ip);
                    phone.ip = ip;
                    self.phone = Some(phone.clone());
                    *self.shared.phone.lock().unwrap() = Some(phone.clone());
                }
            }
        }
        // NetBird has no "peer online" flag, so always try; the peer's
        // connection state only helps explain a failure.
        let peer_connected = snap.peer_by_id(&phone.node_id).is_some_and(|p| p.online);
        let unreachable_detail = |e: &str| {
            if peer_connected {
                format!("{}: {e}", phone.name)
            } else {
                format!(
                    "{} isn't connected to NetBird ({e}). Is NetBird on the phone switched on and does the phone have signal?",
                    phone.name
                )
            }
        };

        self.set(ConnectionStatus::Connecting {
            attempt: self.attempt,
        });
        let target = SocketAddr::new(phone.ip, self.cfg.phone_adb_port);

        // Probe first: tells "rebooted" (refused) apart from "unreachable".
        match tokio::time::timeout(self.cfg.probe_timeout, self.dialer.dial(target)).await {
            Ok(Ok(stream)) => drop(stream),
            Ok(Err(DialError::Refused)) => {
                let retry = self.schedule_backoff().max(5);
                self.next_try = Some(Instant::now() + Duration::from_secs(retry));
                self.set(ConnectionStatus::PhoneNeedsUsb {
                    retry_in_secs: retry,
                });
                return;
            }
            Ok(Err(e)) => {
                let retry = self.schedule_backoff();
                self.set(ConnectionStatus::PhoneUnreachable {
                    detail: unreachable_detail(&e.to_string()),
                    retry_in_secs: retry,
                });
                return;
            }
            Err(_) => {
                let retry = self.schedule_backoff();
                self.set(ConnectionStatus::PhoneUnreachable {
                    detail: unreachable_detail("timed out"),
                    retry_in_secs: retry,
                });
                return;
            }
        }

        let fwd = match self.ensure_forwarder(target).await {
            Ok(f) => f,
            Err(e) => return self.fail_retry(e),
        };
        let serial = fwd.adb_serial();

        if let Err(e) = self.adb.start_server().await {
            return self.fail_retry(e.message);
        }
        if let Err(e) = self.adb.connect(&serial).await {
            return self.fail_retry(e.message);
        }

        let deadline = Instant::now() + self.cfg.connect_settle;
        loop {
            match self.adb.get_state(&serial).await {
                Ok(Some(DeviceState::Device)) => {
                    self.attempt = 0;
                    self.serial = Some(serial.clone());
                    self.ticks = 0;
                    let link = snap.peer_by_id(&phone.node_id).map(|p| p.link.clone());
                    self.set(ConnectionStatus::Connected { serial, link });
                    return;
                }
                Ok(Some(DeviceState::Unauthorized)) | Ok(Some(DeviceState::Authorizing)) => {
                    self.set(ConnectionStatus::Unauthorized);
                    self.schedule(Duration::from_secs(2).min(self.cfg.backoff_max));
                    let _ = self.adb.disconnect(&serial).await;
                    return;
                }
                Ok(_) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(300).min(self.cfg.keepalive)).await;
                }
                Ok(state) => {
                    let _ = self.adb.disconnect(&serial).await;
                    return self.fail_retry(format!("device stayed {state:?} after adb connect"));
                }
                Err(e) => return self.fail_retry(e.message),
            }
        }
    }

    fn fail_retry(&mut self, reason: String) {
        let retry = self.schedule_backoff();
        self.set(ConnectionStatus::Reconnecting {
            attempt: self.attempt,
            retry_in_secs: retry,
            reason,
        });
    }

    /// While connected: is the device still there (and, if `deep`, responsive)?
    async fn check_alive(&mut self, deep: bool) {
        let Some(serial) = self.serial.clone() else {
            return;
        };
        let state = self.adb.get_state(&serial).await;
        let ok = match state {
            Ok(Some(DeviceState::Device)) => {
                if deep {
                    match self
                        .adb
                        .shell(&serial, "echo pb-alive", self.cfg.probe_timeout)
                        .await
                    {
                        Ok(out) if out.contains("pb-alive") => true,
                        Ok(out) => {
                            tracing::warn!("liveness check got {out:?}");
                            false
                        }
                        Err(e) => {
                            tracing::warn!("liveness check failed: {e}");
                            false
                        }
                    }
                } else {
                    true
                }
            }
            Ok(other) => {
                tracing::warn!("device {serial} is now {other:?}");
                false
            }
            Err(e) => {
                // Our adb server died (or was killed). It restarts on the next command.
                tracing::warn!("adb unavailable: {e}");
                false
            }
        };
        if !ok {
            self.lost("lost connection to phone".into()).await;
        }
    }

    async fn lost(&mut self, reason: String) {
        if let Some(serial) = self.serial.take() {
            let _ = self.adb.disconnect(&serial).await;
        }
        self.attempt = 0;
        self.schedule(Duration::ZERO);
        self.set(ConnectionStatus::Reconnecting {
            attempt: 0,
            retry_in_secs: 0,
            reason,
        });
    }

    async fn drop_device(&mut self) {
        if let Some(serial) = self.serial.take() {
            let _ = self.adb.disconnect(&serial).await;
        }
    }

    async fn teardown(&mut self) {
        self.want_connected = false;
        self.drop_device().await;
        let fwd = self.shared.forwarder.lock().unwrap().take();
        if let Some(f) = fwd {
            f.stop().await;
        }
        let _ = self.adb.kill_server().await;
        self.set(ConnectionStatus::Disconnected);
    }
}

fn net_detail(state: &NetState) -> String {
    match state {
        NetState::Starting => "NetBird starting…".into(),
        NetState::NeedsLogin { .. } => "Log in to NetBird".into(),
        NetState::Running => "Online".into(),
        NetState::Stopped => "NetBird stopped".into(),
        NetState::Error { message, .. } => format!("NetBird error: {message}"),
    }
}

async fn sleep_until_opt(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

async fn recv_fwd(rx: &mut Option<broadcast::Receiver<ForwarderEvent>>) -> Option<ForwarderEvent> {
    match rx {
        Some(r) => match r.recv().await {
            Ok(ev) => Some(ev),
            Err(broadcast::error::RecvError::Lagged(_)) => None,
            Err(broadcast::error::RecvError::Closed) => {
                *rx = None;
                None
            }
        },
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        let b = Duration::from_secs(1);
        let m = Duration::from_secs(30);
        assert_eq!(backoff(b, m, 0), Duration::from_secs(1));
        assert_eq!(backoff(b, m, 1), Duration::from_secs(2));
        assert_eq!(backoff(b, m, 4), Duration::from_secs(16));
        assert_eq!(backoff(b, m, 5), Duration::from_secs(30));
        assert_eq!(backoff(b, m, 200), Duration::from_secs(30));
    }
}
