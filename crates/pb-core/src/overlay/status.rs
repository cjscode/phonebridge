//! Overlay status (as produced by `nb_status_json` / `ts_status_json`, which
//! share one JSON shape) and the simplified snapshot sent to the UI.

use std::net::{IpAddr, Ipv4Addr};

use serde::{Deserialize, Serialize};

/// Which overlay network PhoneBridge joins.
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    #[default]
    Netbird,
    Tailscale,
}

impl Network {
    pub const ALL: [Network; 2] = [Network::Netbird, Network::Tailscale];

    pub fn label(self) -> &'static str {
        match self {
            Network::Netbird => "NetBird",
            Network::Tailscale => "Tailscale",
        }
    }

    /// Folder (under the app's data folder) holding this network's login.
    pub fn dir_name(self) -> &'static str {
        match self {
            Network::Netbird => "netbird",
            Network::Tailscale => "tailscale",
        }
    }

    /// NetBird Cloud's management server, or Tailscale's control server.
    pub fn default_server_url(self) -> &'static str {
        match self {
            Network::Netbird => super::DEFAULT_MANAGEMENT_URL,
            Network::Tailscale => super::DEFAULT_CONTROL_URL,
        }
    }

    /// What a pre-made join key is called.
    pub fn key_name(self) -> &'static str {
        match self {
            Network::Netbird => "setup key",
            Network::Tailscale => "auth key",
        }
    }

    pub fn parse(s: &str) -> Option<Network> {
        match s.trim().to_ascii_lowercase().as_str() {
            "netbird" | "nb" => Some(Network::Netbird),
            "tailscale" | "ts" | "headscale" => Some(Network::Tailscale),
            _ => None,
        }
    }
}

impl std::fmt::Display for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct RawStatus {
    /// The first network map has arrived (before that, `peers` is incomplete).
    pub synced: bool,
    /// Tailscale only: NoState, NeedsLogin, NeedsMachineAuth, Starting,
    /// Running, Stopped. Empty for NetBird (a started client is running).
    pub backend_state: String,
    /// Tailscale only: login URL while it needs a login.
    pub auth_url: String,
    /// Tailscale only: the tailnet's name.
    pub network_name: String,
    pub management_url: String,
    /// NetBird only.
    pub management_connected: Option<bool>,
    /// NetBird: management error. Tailscale: last backend error (e.g. bad auth key).
    pub management_error: String,
    /// NetBird only.
    pub signal_connected: Option<bool>,
    /// Tailscale only: health warnings.
    #[serde(deserialize_with = "null_as_empty")]
    pub health: Vec<String>,
    pub local_ip: String,
    pub local_ipv6: String,
    pub local_fqdn: String,
    pub local_pub_key: String,
    #[serde(deserialize_with = "null_as_empty")]
    pub peers: Vec<RawPeer>,
}

/// Go encodes an empty slice as `null`.
fn null_as_empty<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(d)?.unwrap_or_default())
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct RawPeer {
    /// Stable peer ID (NetBird: WireGuard key; Tailscale: stable node ID).
    pub pub_key: String,
    /// Set when the FQDN is empty (Tailscale without MagicDNS).
    pub hostname: String,
    /// Tailscale only.
    pub os: String,
    /// May carry a prefix length ("100.92.1.2/16"); handled by `parse_ip`.
    pub ip: String,
    pub ipv6: String,
    pub fqdn: String,
    /// "Idle", "Connecting", "Connected", or (Tailscale) "Online": reachable
    /// but no traffic yet.
    pub conn_status: String,
    pub relayed: bool,
    pub local_endpoint: String,
    pub remote_endpoint: String,
    pub relay_address: String,
    pub latency_ms: i64,
    pub last_handshake_unix: i64,
    pub bytes_rx: i64,
    pub bytes_tx: i64,
}

/// High-level network state shown in the UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NetState {
    /// Creating the client / logging in / bringing the engine up.
    Starting,
    /// Not logged in. `auth_url` is set while a browser login is waiting.
    NeedsLogin {
        auth_url: Option<String>,
        user_code: Option<String>,
        /// Why we're logged out (e.g. login expired), if known.
        reason: Option<String>,
    },
    Running,
    Stopped,
    /// Couldn't start (offline, server unreachable); retrying.
    Error {
        message: String,
        retry_in_secs: Option<u64>,
    },
}

/// How traffic to a peer currently flows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PeerLink {
    Direct {
        addr: String,
    },
    Relayed {
        via: String,
    },
    /// Not connected right now.
    Idle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Peer {
    /// WireGuard public key: stable for as long as the peer keeps its identity.
    pub id: String,
    pub hostname: String,
    pub dns_name: String,
    /// Tailscale reports it ("android", "macOS"...); NetBird doesn't (empty).
    pub os: String,
    pub ips: Vec<IpAddr>,
    /// NetBird: we have a live WireGuard connection to it. Tailscale: it is
    /// connected to the control server.
    pub online: bool,
    pub link: PeerLink,
    pub latency_ms: Option<i64>,
}

impl Peer {
    pub fn ipv4(&self) -> Option<Ipv4Addr> {
        self.ips.iter().find_map(|ip| match ip {
            IpAddr::V4(v4) => Some(*v4),
            IpAddr::V6(_) => None,
        })
    }

    pub fn display_name(&self) -> String {
        if self.hostname.is_empty() {
            self.ips.first().map(|i| i.to_string()).unwrap_or_default()
        } else {
            self.hostname.clone()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NetSnapshot {
    /// Which network this is about.
    pub kind: Network,
    pub state: NetState,
    /// The first network map has arrived, so `peers` is complete.
    pub synced: bool,
    pub self_name: Option<String>,
    pub self_ips: Vec<IpAddr>,
    /// Tailnet name, or the management server's host (e.g. "api.netbird.io").
    pub network: Option<String>,
    /// Warnings worth showing (servers unreachable, Tailscale health).
    pub health: Vec<String>,
    /// Connected first, then by name.
    pub peers: Vec<Peer>,
}

impl NetSnapshot {
    pub fn with_state(state: NetState) -> Self {
        Self {
            kind: Network::default(),
            state,
            synced: false,
            self_name: None,
            self_ips: vec![],
            network: None,
            health: vec![],
            peers: vec![],
        }
    }

    pub fn starting() -> Self {
        Self::with_state(NetState::Starting)
    }

    pub fn of(kind: Network, state: NetState) -> Self {
        Self {
            kind,
            ..Self::with_state(state)
        }
    }

    pub fn is_running(&self) -> bool {
        self.state == NetState::Running
    }

    pub fn peer_by_id(&self, id: &str) -> Option<&Peer> {
        self.peers.iter().find(|p| p.id == id)
    }

    pub fn peer_by_ip(&self, ip: IpAddr) -> Option<&Peer> {
        self.peers.iter().find(|p| p.ips.contains(&ip))
    }
}

/// "100.92.1.2/16" or "100.92.1.2" -> IpAddr.
fn parse_ip(s: &str) -> Option<IpAddr> {
    s.split('/').next()?.trim().parse().ok()
}

impl RawPeer {
    fn into_peer(self) -> Peer {
        let connected = self.conn_status.eq_ignore_ascii_case("connected");
        let online = connected || self.conn_status.eq_ignore_ascii_case("online");
        let link = if !connected {
            PeerLink::Idle
        } else if self.relayed {
            PeerLink::Relayed {
                via: host_of(&self.relay_address),
            }
        } else {
            PeerLink::Direct {
                addr: self.remote_endpoint.clone(),
            }
        };
        let fqdn = self.fqdn.trim_end_matches('.').to_string();
        let hostname = match fqdn.split('.').next() {
            Some(h) if !h.is_empty() => h.to_string(),
            _ => self.hostname.clone(),
        };
        Peer {
            id: self.pub_key,
            hostname,
            dns_name: fqdn,
            os: self.os,
            ips: [self.ip.as_str(), self.ipv6.as_str()]
                .into_iter()
                .filter_map(parse_ip)
                .collect(),
            online,
            link,
            latency_ms: (connected && self.latency_ms > 0).then_some(self.latency_ms),
        }
    }
}

/// "rels://streamline-de-fra1-0.relay.netbird.io:443" -> "streamline-de-fra1-0.relay.netbird.io".
/// Text that isn't an address ("DERP ord") is kept as is.
fn host_of(addr: &str) -> String {
    if addr.contains(' ') {
        return addr.to_string();
    }
    let no_scheme = addr.split("://").last().unwrap_or(addr);
    let host = no_scheme.split('/').next().unwrap_or(no_scheme);
    host.rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(host)
        .to_string()
}

impl RawStatus {
    /// Snapshot for a started client/node.
    pub fn into_snapshot(self, kind: Network) -> NetSnapshot {
        let state = match self.backend_state.as_str() {
            "" | "Running" => NetState::Running,
            "NeedsLogin" => NetState::NeedsLogin {
                auth_url: Some(self.auth_url.clone()).filter(|u| !u.is_empty()),
                user_code: None,
                reason: Some(self.management_error.clone())
                    .filter(|e| !e.is_empty())
                    .map(|e| super::friendly_login_reason(kind, &e)),
            },
            "NeedsMachineAuth" => NetState::NeedsLogin {
                auth_url: None,
                user_code: None,
                reason: Some(format!(
                    "This Mac is waiting for approval in the {} admin console.",
                    kind.label()
                )),
            },
            "Stopped" => NetState::Stopped,
            _ => NetState::Starting,
        };
        if state != NetState::Running {
            return NetSnapshot {
                network: url_host(&self.management_url),
                ..NetSnapshot::of(kind, state)
            };
        }
        let mut health = vec![];
        if self.management_connected == Some(false) {
            let why = if self.management_error.is_empty() {
                String::new()
            } else {
                format!(": {}", self.management_error)
            };
            health.push(format!("Can't reach the NetBird management server{why}"));
        }
        if self.signal_connected == Some(false) {
            health.push("Can't reach the NetBird signal server; new connections may fail".into());
        }
        health.extend(self.health.iter().cloned());
        let mut peers: Vec<Peer> = self.peers.into_iter().map(RawPeer::into_peer).collect();
        peers.sort_by(|a, b| {
            b.online.cmp(&a.online).then_with(|| {
                a.display_name()
                    .to_lowercase()
                    .cmp(&b.display_name().to_lowercase())
            })
        });
        let network = Some(self.network_name.clone())
            .filter(|n| !n.is_empty())
            .or_else(|| url_host(&self.management_url));
        NetSnapshot {
            kind,
            state: NetState::Running,
            synced: self.synced,
            self_name: Some(self.local_fqdn.trim_end_matches('.').to_string())
                .filter(|s| !s.is_empty()),
            self_ips: [self.local_ip.as_str(), self.local_ipv6.as_str()]
                .into_iter()
                .filter_map(parse_ip)
                .collect(),
            network,
            health,
            peers,
        }
    }
}

fn url_host(url: &str) -> Option<String> {
    let h = host_of(url);
    (!h.is_empty()).then_some(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUNNING: &str = include_str!("../../tests/fixtures/netbird_status.json");

    #[test]
    fn parses_running_status() {
        let raw: RawStatus = serde_json::from_str(RUNNING).unwrap();
        let snap = raw.into_snapshot(Network::Netbird);
        assert_eq!(snap.state, NetState::Running);
        assert!(snap.synced);
        assert_eq!(
            snap.self_name.as_deref(),
            Some("phonebridge-mac.netbird.cloud")
        );
        assert_eq!(
            snap.self_ips,
            vec!["100.92.10.1".parse::<IpAddr>().unwrap()]
        );
        assert_eq!(snap.network.as_deref(), Some("api.netbird.io"));
        assert!(snap.health.is_empty());
        // Connected peers first (then by name).
        let names: Vec<_> = snap.peers.iter().map(|p| p.hostname.as_str()).collect();
        assert_eq!(names, ["nas", "pixel-9", "old-laptop"]);
        let phone = &snap.peers[1];
        assert_eq!(phone.hostname, "pixel-9");
        assert_eq!(phone.ipv4(), Some(Ipv4Addr::new(100, 92, 33, 7)));
        assert_eq!(
            phone.link,
            PeerLink::Direct {
                addr: "203.0.113.7:51820".into()
            }
        );
        assert_eq!(phone.latency_ms, Some(42));
        let nas = snap.peers.iter().find(|p| p.hostname == "nas").unwrap();
        assert_eq!(
            nas.link,
            PeerLink::Relayed {
                via: "streamline-de-fra1-0.relay.netbird.io".into()
            }
        );
        let laptop = snap
            .peers
            .iter()
            .find(|p| p.hostname == "old-laptop")
            .unwrap();
        assert!(!laptop.online);
        assert_eq!(laptop.link, PeerLink::Idle);
        assert!(snap.peer_by_ip("100.92.33.7".parse().unwrap()).is_some());
    }

    #[test]
    fn management_down_is_a_health_warning() {
        let raw: RawStatus = serde_json::from_str(
            r#"{"management_url":"https://nb.example.com:33073","management_connected":false,"management_error":"connection refused","signal_connected":true,"peers":null}"#,
        )
        .unwrap();
        let snap = raw.into_snapshot(Network::Netbird);
        assert_eq!(snap.network.as_deref(), Some("nb.example.com"));
        assert_eq!(snap.health.len(), 1);
        assert!(snap.health[0].contains("connection refused"));
    }

    #[test]
    fn tolerates_minimal_json() {
        let raw: RawStatus = serde_json::from_str("{}").unwrap();
        let snap = raw.into_snapshot(Network::Netbird);
        assert!(snap.peers.is_empty());
        assert!(snap.health.is_empty());
        assert_eq!(snap.state, NetState::Running);
    }

    const TS_RUNNING: &str = include_str!("../../tests/fixtures/tailscale_status.json");

    #[test]
    fn parses_tailscale_status() {
        let raw: RawStatus = serde_json::from_str(TS_RUNNING).unwrap();
        let snap = raw.into_snapshot(Network::Tailscale);
        assert_eq!(snap.kind, Network::Tailscale);
        assert_eq!(snap.state, NetState::Running);
        assert!(snap.synced);
        assert_eq!(snap.network.as_deref(), Some("example.github"));
        assert_eq!(
            snap.self_name.as_deref(),
            Some("phonebridge-mac.tail1234.ts.net")
        );
        assert_eq!(
            snap.health,
            ["Tailscale can't reach the configured DNS servers."]
        );
        let names: Vec<_> = snap.peers.iter().map(|p| p.hostname.as_str()).collect();
        // Online first (direct, DERP, idle-but-online), then offline.
        assert_eq!(names, ["desktop", "nas", "pixel-9", "old-phone"]);
        let phone = snap.peer_by_id("nPhone1CNTRL").unwrap();
        assert!(phone.online);
        assert_eq!(phone.os, "android");
        assert_eq!(phone.link, PeerLink::Idle);
        assert_eq!(phone.ipv4(), Some(Ipv4Addr::new(100, 101, 102, 103)));
        let desktop = snap.peer_by_id("nDesk1CNTRL").unwrap();
        assert_eq!(
            desktop.link,
            PeerLink::Direct {
                addr: "192.168.1.20:41641".into()
            }
        );
        let nas = snap.peer_by_id("nNas1CNTRL").unwrap();
        assert_eq!(
            nas.link,
            PeerLink::Relayed {
                via: "DERP ord".into()
            }
        );
        let old = snap.peer_by_id("nOld1CNTRL").unwrap();
        assert!(!old.online);
    }

    #[test]
    fn tailscale_login_states() {
        let raw: RawStatus = serde_json::from_str(
            r#"{"backend_state":"NeedsLogin","auth_url":"https://login.tailscale.com/a/abc","management_url":"https://controlplane.tailscale.com","peers":null}"#,
        )
        .unwrap();
        let snap = raw.into_snapshot(Network::Tailscale);
        assert_eq!(
            snap.state,
            NetState::NeedsLogin {
                auth_url: Some("https://login.tailscale.com/a/abc".into()),
                user_code: None,
                reason: None
            }
        );
        let raw: RawStatus = serde_json::from_str(
            r#"{"backend_state":"NeedsLogin","management_error":"invalid key: unable to validate API key","peers":null}"#,
        )
        .unwrap();
        match raw.into_snapshot(Network::Tailscale).state {
            NetState::NeedsLogin {
                reason: Some(r),
                auth_url: None,
                ..
            } => {
                assert!(r.contains("auth key"), "{r}")
            }
            other => panic!("{other:?}"),
        }
        let raw: RawStatus =
            serde_json::from_str(r#"{"backend_state":"NeedsMachineAuth"}"#).unwrap();
        assert!(matches!(
            raw.into_snapshot(Network::Tailscale).state,
            NetState::NeedsLogin { reason: Some(r), .. } if r.contains("approval")
        ));
        let raw: RawStatus = serde_json::from_str(r#"{"backend_state":"Starting"}"#).unwrap();
        assert_eq!(
            raw.into_snapshot(Network::Tailscale).state,
            NetState::Starting
        );
    }

    #[test]
    fn network_names() {
        assert_eq!(Network::parse("Tailscale"), Some(Network::Tailscale));
        assert_eq!(Network::parse("netbird"), Some(Network::Netbird));
        assert_eq!(Network::parse("zerotier"), None);
        assert_eq!(
            serde_json::to_string(&Network::Tailscale).unwrap(),
            "\"tailscale\""
        );
    }
}
