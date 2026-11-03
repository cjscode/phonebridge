//! Connection manager state machine against a fake `adb` (a shell script whose
//! behaviour is controlled by files) and a scripted dialer.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pb_core::adb::Adb;
use pb_core::connection::{
    self, ConnectionConfig, ConnectionHandle, ConnectionStatus, PhoneConfig,
};
use pb_core::forwarder::{BoxFuture, BoxStream, Dialer};
use pb_core::overlay::{DialError, NetSnapshot, NetState, Network, Peer, PeerLink};
use pb_core::tools::BundledTools;
use tokio::sync::watch;

const PHONE_ID: &str = "nPHONE";

/// A fake adb: `connect` records the serial; `devices -l` reports it with the
/// state in `state`; `shell` fails if `shell_fail` exists. Every call is logged.
fn fake_adb(dir: &Path) -> PathBuf {
    let script = dir.join("adb");
    let d = dir.display();
    std::fs::write(
        &script,
        format!(
            r#"#!/bin/sh
D="{d}"
echo "$@" >> "$D/calls"
case "$1" in
  start-server|kill-server|version) exit 0 ;;
  connect) echo "$2" > "$D/connected"; echo "connected to $2"; exit 0 ;;
  disconnect) rm -f "$D/connected"; echo "disconnected $2"; exit 0 ;;
  devices)
    echo "List of devices attached"
    if [ -f "$D/connected" ]; then printf '%s\t%s product:p model:Pixel_9 transport_id:7\n' "$(cat "$D/connected")" "$(cat "$D/state")"; fi
    exit 0 ;;
  -s)
    if [ -f "$D/shell_fail" ]; then echo "error: closed" >&2; exit 1; fi
    shift 2; shift; echo "$1" | sed 's/^echo //'; exit 0 ;;
esac
exit 0
"#
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(dir.join("state"), "device").unwrap();
    script
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum DialMode {
    Ok,
    Refused,
    Timeout,
}

struct ScriptedDialer {
    mode: Mutex<DialMode>,
    sink: SocketAddr,
}

impl Dialer for ScriptedDialer {
    fn dial(&self, _addr: SocketAddr) -> BoxFuture<Result<BoxStream, DialError>> {
        let mode = *self.mode.lock().unwrap();
        let sink = self.sink;
        Box::pin(async move {
            match mode {
                DialMode::Refused => Err(DialError::Refused),
                DialMode::Timeout => Err(DialError::Timeout),
                DialMode::Ok => {
                    let s = tokio::net::TcpStream::connect(sink)
                        .await
                        .map_err(|e| DialError::Other(e.to_string()))?;
                    Ok(Box::new(s) as BoxStream)
                }
            }
        })
    }
}

async fn sink_server() -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            if let Ok((s, _)) = l.accept().await {
                tokio::spawn(async move {
                    let mut s = s;
                    let _ = tokio::io::copy(&mut tokio::io::empty(), &mut s).await;
                    tokio::time::sleep(Duration::from_secs(60)).await;
                });
            }
        }
    });
    a
}

fn snapshot(running: bool, phone_online: bool) -> NetSnapshot {
    let mut s = NetSnapshot::with_state(if running {
        NetState::Running
    } else {
        NetState::NeedsLogin {
            auth_url: None,
            user_code: None,
            reason: None,
        }
    });
    s.synced = running;
    if running {
        s.peers.push(Peer {
            id: PHONE_ID.into(),
            hostname: "Pixel-9".into(),
            dns_name: "pixel-9.netbird.cloud".into(),
            os: String::new(),
            ips: vec!["100.101.102.103".parse().unwrap()],
            online: phone_online,
            link: if phone_online {
                PeerLink::Direct {
                    addr: "203.0.113.7:51820".into(),
                }
            } else {
                PeerLink::Idle
            },
            latency_ms: None,
        });
    }
    s
}

struct Rig {
    _dir: tempfile::TempDir,
    dir: PathBuf,
    dialer: Arc<ScriptedDialer>,
    net: watch::Sender<NetSnapshot>,
    handle: ConnectionHandle,
}

impl Rig {
    async fn new(running: bool, online: bool, mode: DialMode, with_phone: bool) -> Rig {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let adb_path = fake_adb(&dir);
        let mut tools = BundledTools::new(&dir, &dir, 39999);
        tools.adb = adb_path;
        let dialer = Arc::new(ScriptedDialer {
            mode: Mutex::new(mode),
            sink: sink_server().await,
        });
        let (tx, rx) = watch::channel(snapshot(running, online));
        let cfg = ConnectionConfig {
            keepalive: Duration::from_millis(100),
            liveness_every: 2,
            probe_timeout: Duration::from_secs(2),
            backoff_base: Duration::from_millis(50),
            backoff_max: Duration::from_millis(200),
            connect_settle: Duration::from_millis(600),
            ..Default::default()
        };
        let phone = with_phone.then(|| PhoneConfig {
            node_id: PHONE_ID.into(),
            name: "Pixel 9".into(),
            ip: "100.101.102.103".parse::<IpAddr>().unwrap(),
        });
        let handle = connection::spawn(Adb::new(tools), dialer.clone(), rx, cfg, phone);
        Rig {
            _dir: tmp,
            dir,
            dialer,
            net: tx,
            handle,
        }
    }

    fn set_dial(&self, m: DialMode) {
        *self.dialer.mode.lock().unwrap() = m;
    }

    fn set_adb_state(&self, s: &str) {
        std::fs::write(self.dir.join("state"), s).unwrap();
    }

    async fn wait_for(
        &self,
        what: &str,
        pred: impl Fn(&ConnectionStatus) -> bool,
    ) -> ConnectionStatus {
        let mut rx = self.handle.subscribe();
        let r = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let s = rx.borrow_and_update().clone();
                if pred(&s) {
                    return s;
                }
                rx.changed().await.unwrap();
            }
        })
        .await;
        match r {
            Ok(s) => s,
            Err(_) => panic!(
                "timed out waiting for {what}; status is {:?}",
                self.handle.status()
            ),
        }
    }
}

#[tokio::test]
async fn connects_when_everything_is_up() {
    let rig = Rig::new(true, true, DialMode::Ok, true).await;
    let s = rig
        .wait_for("connected", ConnectionStatus::is_connected)
        .await;
    let ConnectionStatus::Connected { serial, link } = s else {
        unreachable!()
    };
    assert!(serial.starts_with("127.0.0.1:"));
    assert_eq!(Some(serial.clone()), rig.handle.serial());
    assert!(matches!(link, Some(PeerLink::Direct { .. })));
    assert_eq!(
        rig.handle.forwarder_port(),
        Some(serial.rsplit(':').next().unwrap().parse().unwrap())
    );
    rig.handle.shutdown().await;
    let calls = std::fs::read_to_string(rig.dir.join("calls")).unwrap();
    assert!(calls.contains(&format!("connect {serial}")));
    assert!(
        calls.contains("kill-server"),
        "shutdown stops our adb server: {calls}"
    );
}

#[tokio::test]
async fn no_phone_configured() {
    let rig = Rig::new(true, true, DialMode::Ok, false).await;
    assert_eq!(rig.handle.status(), ConnectionStatus::NoPhone);
    rig.handle.shutdown().await;
}

#[tokio::test]
async fn refused_means_phone_needs_usb() {
    let rig = Rig::new(true, true, DialMode::Refused, true).await;
    rig.wait_for("needs usb", |s| {
        matches!(s, ConnectionStatus::PhoneNeedsUsb { .. })
    })
    .await;
    // After re-enabling adb over USB the next retry connects.
    rig.set_dial(DialMode::Ok);
    rig.handle.connect_now().await;
    rig.wait_for("connected", ConnectionStatus::is_connected)
        .await;
    rig.handle.shutdown().await;
}

#[tokio::test]
async fn offline_peer_is_unreachable_then_recovers() {
    // Not connected in NetBird and the dial times out: explain it's the phone's NetBird.
    let rig = Rig::new(true, false, DialMode::Timeout, true).await;
    let s = rig
        .wait_for("unreachable", |s| {
            matches!(s, ConnectionStatus::PhoneUnreachable { .. })
        })
        .await;
    if let ConnectionStatus::PhoneUnreachable { detail, .. } = s {
        assert!(detail.contains("isn't connected to NetBird"), "{detail}");
    }
    rig.set_dial(DialMode::Ok);
    rig.net.send_replace(snapshot(true, true));
    rig.wait_for("connected", ConnectionStatus::is_connected)
        .await;
    rig.handle.shutdown().await;
}

#[tokio::test]
async fn messages_name_the_network_in_use() {
    let rig = Rig::new(true, false, DialMode::Timeout, true).await;
    let mut ts = snapshot(true, false);
    ts.kind = Network::Tailscale;
    rig.net.send_replace(ts);
    rig.handle.connect_now().await;
    let s = rig
        .wait_for("unreachable over Tailscale", |s| {
            matches!(s, ConnectionStatus::PhoneUnreachable { detail, .. } if detail.contains("Tailscale"))
        })
        .await;
    if let ConnectionStatus::PhoneUnreachable { detail, .. } = s {
        assert!(detail.contains("isn't connected to Tailscale"), "{detail}");
        assert!(!detail.contains("NetBird"), "{detail}");
    }
    let mut off = NetSnapshot::of(Network::Tailscale, NetState::Starting);
    off.synced = false;
    rig.net.send_replace(off);
    rig.wait_for(
        "Tailscale starting",
        |s| matches!(s, ConnectionStatus::NetOffline { detail } if detail == "Tailscale starting…"),
    )
    .await;
    rig.handle.shutdown().await;
}

#[tokio::test]
async fn idle_peer_is_still_tried() {
    // NetBird has no online flag: a peer that isn't connected yet may still answer.
    let rig = Rig::new(true, false, DialMode::Ok, true).await;
    rig.wait_for("connected", ConnectionStatus::is_connected)
        .await;
    rig.handle.shutdown().await;
}

#[tokio::test]
async fn waits_for_first_sync_before_saying_phone_is_gone() {
    // Running but the network map hasn't arrived: no peers yet.
    let rig = Rig::new(true, true, DialMode::Ok, true).await;
    rig.wait_for("connected", ConnectionStatus::is_connected)
        .await;
    rig.handle.disconnect().await;
    rig.wait_for("disconnected", |s| *s == ConnectionStatus::Disconnected)
        .await;
    let mut unsynced = snapshot(true, true);
    unsynced.synced = false;
    unsynced.peers.clear();
    rig.net.send_replace(unsynced);
    rig.handle.connect_now().await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !matches!(
            rig.handle.status(),
            ConnectionStatus::PhoneUnreachable { .. }
        ),
        "{:?}",
        rig.handle.status()
    );
    // The map arrives: connects.
    rig.net.send_replace(snapshot(true, true));
    rig.wait_for("connected", ConnectionStatus::is_connected)
        .await;
    rig.handle.shutdown().await;
}

#[tokio::test]
async fn dial_timeout_is_unreachable() {
    let rig = Rig::new(true, true, DialMode::Timeout, true).await;
    rig.wait_for("unreachable", |s| {
        matches!(s, ConnectionStatus::PhoneUnreachable { .. })
    })
    .await;
    rig.handle.shutdown().await;
}

#[tokio::test]
async fn waits_for_netbird_then_connects() {
    let rig = Rig::new(false, true, DialMode::Ok, true).await;
    rig.wait_for("NetBird offline", |s| {
        matches!(s, ConnectionStatus::NetOffline { .. })
    })
    .await;
    rig.net.send_replace(snapshot(true, true));
    rig.wait_for("connected", ConnectionStatus::is_connected)
        .await;
    // NetBird drops mid-session.
    rig.net.send_replace(snapshot(false, true));
    rig.wait_for("NetBird offline again", |s| {
        matches!(s, ConnectionStatus::NetOffline { .. })
    })
    .await;
    assert_eq!(rig.handle.serial(), None);
    rig.net.send_replace(snapshot(true, true));
    rig.wait_for("reconnected", ConnectionStatus::is_connected)
        .await;
    rig.handle.shutdown().await;
}

#[tokio::test]
async fn unauthorized_then_allowed() {
    let rig = Rig::new(true, true, DialMode::Ok, true).await;
    rig.set_adb_state("unauthorized");
    rig.wait_for("unauthorized", |s| *s == ConnectionStatus::Unauthorized)
        .await;
    rig.set_adb_state("device");
    rig.wait_for("connected", ConnectionStatus::is_connected)
        .await;
    rig.handle.shutdown().await;
}

#[tokio::test]
async fn device_drop_triggers_reconnect() {
    let rig = Rig::new(true, true, DialMode::Ok, true).await;
    rig.wait_for("connected", ConnectionStatus::is_connected)
        .await;
    // adb reports the device gone (e.g. transport died).
    std::fs::remove_file(rig.dir.join("connected")).unwrap();
    rig.wait_for("reconnecting", |s| !s.is_connected()).await;
    rig.wait_for("connected again", ConnectionStatus::is_connected)
        .await;
    rig.handle.shutdown().await;
}

#[tokio::test]
async fn unresponsive_phone_detected_by_liveness_and_reboot_detected() {
    let rig = Rig::new(true, true, DialMode::Ok, true).await;
    rig.wait_for("connected", ConnectionStatus::is_connected)
        .await;
    // Phone reboots: shell stops answering, and 5555 now refuses.
    std::fs::write(rig.dir.join("shell_fail"), "").unwrap();
    rig.set_dial(DialMode::Refused);
    rig.wait_for("needs usb", |s| {
        matches!(s, ConnectionStatus::PhoneNeedsUsb { .. })
    })
    .await;
    rig.handle.shutdown().await;
}

#[tokio::test]
async fn disconnect_and_reconnect_on_request() {
    let rig = Rig::new(true, true, DialMode::Ok, true).await;
    rig.wait_for("connected", ConnectionStatus::is_connected)
        .await;
    rig.handle.disconnect().await;
    rig.wait_for("disconnected", |s| *s == ConnectionStatus::Disconnected)
        .await;
    // Stays disconnected (no auto-reconnect after an explicit disconnect).
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(rig.handle.status(), ConnectionStatus::Disconnected);
    rig.handle.connect_now().await;
    rig.wait_for("connected", ConnectionStatus::is_connected)
        .await;
    rig.handle.shutdown().await;
}

#[tokio::test]
async fn ip_change_is_followed() {
    let rig = Rig::new(true, true, DialMode::Ok, true).await;
    rig.wait_for("connected", ConnectionStatus::is_connected)
        .await;
    let mut snap = snapshot(true, true);
    snap.peers[0].ips = vec!["100.64.9.9".parse().unwrap()];
    rig.handle.disconnect().await;
    rig.wait_for("disconnected", |s| *s == ConnectionStatus::Disconnected)
        .await;
    rig.net.send_replace(snap);
    rig.handle.connect_now().await;
    rig.wait_for("connected", ConnectionStatus::is_connected)
        .await;
    assert_eq!(
        rig.handle.phone().unwrap().ip,
        "100.64.9.9".parse::<IpAddr>().unwrap()
    );
    rig.handle.shutdown().await;
}
