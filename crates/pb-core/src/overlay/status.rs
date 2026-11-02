//! NetBird status (as produced by `nb_status_json`) and the simplified snapshot
//! sent to the UI.

use std::net::{IpAddr, Ipv4Addr};

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct RawStatus {
    /// The first network map has arrived (before that, `peers` is incomplete).
    pub synced: bool,
    pub management_url: String,
    pub management_connected: bool,
    pub management_error: String,
    pub signal_connected: bool,
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
    pub pub_key: String,
    /// May carry a prefix length ("100.92.1.2/16"); handled by `parse_ip`.
    pub ip: String,
    pub ipv6: String,
    pub fqdn: String,
    /// "Idle", "Connecting" or "Connected".
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

/// High-level NetBird state shown in the UI.
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
    /// NetBird doesn't report peer OS to other peers; kept for the UI (empty).
    pub os: String,
    pub ips: Vec<IpAddr>,
    /// We have a live WireGuard connection to it.
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
    pub state: NetState,
    /// NetBird has received its first network map, so `peers` is complete.
    pub synced: bool,
    pub self_name: Option<String>,
    pub self_ips: Vec<IpAddr>,
    /// Management server host, e.g. "api.netbird.io".
    pub network: Option<String>,
    /// Warnings worth showing (management/signal unreachable).
    pub health: Vec<String>,
    /// Connected first, then by name.
    pub peers: Vec<Peer>,
}

impl NetSnapshot {
    pub fn with_state(state: NetState) -> Self {
        Self {
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
        let online = self.conn_status.eq_ignore_ascii_case("connected");
        let link = if !online {
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
        Peer {
            id: self.pub_key,
            hostname: fqdn.split('.').next().unwrap_or_default().to_string(),
            dns_name: fqdn,
            os: String::new(),
            ips: [self.ip.as_str(), self.ipv6.as_str()]
                .into_iter()
                .filter_map(parse_ip)
                .collect(),
            online,
            link,
            latency_ms: (online && self.latency_ms > 0).then_some(self.latency_ms),
        }
    }
}

/// "rels://streamline-de-fra1-0.relay.netbird.io:443" -> "streamline-de-fra1-0.relay.netbird.io".
fn host_of(addr: &str) -> String {
    let no_scheme = addr.split("://").last().unwrap_or(addr);
    let host = no_scheme.split('/').next().unwrap_or(no_scheme);
    host.rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(host)
        .to_string()
}

impl RawStatus {
    /// Snapshot for a running engine.
    pub fn into_snapshot(self) -> NetSnapshot {
        let mut health = vec![];
        if !self.management_connected {
            let why = if self.management_error.is_empty() {
                String::new()
            } else {
                format!(": {}", self.management_error)
            };
            health.push(format!("Can't reach the NetBird management server{why}"));
        }
        if !self.signal_connected {
            health.push("Can't reach the NetBird signal server; new connections may fail".into());
        }
        let mut peers: Vec<Peer> = self.peers.into_iter().map(RawPeer::into_peer).collect();
        peers.sort_by(|a, b| {
            b.online.cmp(&a.online).then_with(|| {
                a.display_name()
                    .to_lowercase()
                    .cmp(&b.display_name().to_lowercase())
            })
        });
        let network = url_host(&self.management_url);
        NetSnapshot {
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
        let snap = raw.into_snapshot();
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
        let snap = raw.into_snapshot();
        assert_eq!(snap.network.as_deref(), Some("nb.example.com"));
        assert_eq!(snap.health.len(), 1);
        assert!(snap.health[0].contains("connection refused"));
    }

    #[test]
    fn tolerates_minimal_json() {
        let raw: RawStatus = serde_json::from_str("{}").unwrap();
        assert!(raw.into_snapshot().peers.is_empty());
    }
}
