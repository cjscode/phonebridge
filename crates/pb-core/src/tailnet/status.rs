//! The subset of tailscale's `ipnstate.Status` JSON we use, and the
//! simplified snapshot sent to the UI.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct RawStatus {
    pub version: String,
    pub backend_state: String,
    #[serde(rename = "AuthURL")]
    pub auth_url: String,
    #[serde(rename = "TailscaleIPs")]
    pub tailscale_ips: Option<Vec<String>>,
    #[serde(rename = "Self")]
    pub self_peer: Option<RawPeer>,
    pub peer: Option<HashMap<String, RawPeer>>,
    pub health: Option<Vec<String>>,
    pub current_tailnet: Option<RawTailnet>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct RawTailnet {
    pub name: String,
    #[serde(rename = "MagicDNSSuffix")]
    pub magic_dns_suffix: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct RawPeer {
    #[serde(rename = "ID")]
    pub id: String,
    pub host_name: String,
    #[serde(rename = "DNSName")]
    pub dns_name: String,
    #[serde(rename = "OS")]
    pub os: String,
    #[serde(rename = "TailscaleIPs")]
    pub tailscale_ips: Option<Vec<String>>,
    pub online: bool,
    pub active: bool,
    /// DERP region code of the peer's home relay.
    pub relay: String,
    /// Set when we have a direct path ("ip:port").
    pub cur_addr: String,
    /// Set when traffic goes through a peer relay.
    pub peer_relay: String,
    pub expired: bool,
}

/// High-level tailnet state shown in the UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TailnetState {
    /// Node not created yet / being (re)created.
    Starting,
    /// Login required. `auth_url` appears a moment after start.
    NeedsLogin {
        auth_url: Option<String>,
    },
    /// Logged in, but an admin must approve this device.
    NeedsMachineAuth,
    Running,
    Stopped,
    /// Couldn't talk to the node.
    Error {
        message: String,
    },
}

/// How traffic to a peer currently flows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PeerLink {
    Direct {
        addr: String,
    },
    PeerRelay {
        via: String,
    },
    Derp {
        region: String,
    },
    /// No recent traffic, so no path is known yet.
    Idle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Peer {
    /// Stable node ID; what we save to remember the phone.
    pub id: String,
    pub hostname: String,
    pub dns_name: String,
    pub os: String,
    pub ips: Vec<IpAddr>,
    pub online: bool,
    pub link: PeerLink,
}

impl Peer {
    pub fn ipv4(&self) -> Option<Ipv4Addr> {
        self.ips.iter().find_map(|ip| match ip {
            IpAddr::V4(v4) => Some(*v4),
            IpAddr::V6(_) => None,
        })
    }

    /// Short display name: MagicDNS label if present, else hostname.
    pub fn display_name(&self) -> String {
        let label = self.dns_name.split('.').next().unwrap_or_default();
        if label.is_empty() {
            self.hostname.clone()
        } else {
            label.to_string()
        }
    }

    pub fn is_android(&self) -> bool {
        self.os.eq_ignore_ascii_case("android")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TailnetSnapshot {
    pub state: TailnetState,
    pub self_name: Option<String>,
    pub self_ips: Vec<IpAddr>,
    pub tailnet: Option<String>,
    pub health: Vec<String>,
    /// Android first, then online, then by name.
    pub peers: Vec<Peer>,
}

impl TailnetSnapshot {
    pub fn starting() -> Self {
        Self::with_state(TailnetState::Starting)
    }

    pub fn with_state(state: TailnetState) -> Self {
        Self {
            state,
            self_name: None,
            self_ips: vec![],
            tailnet: None,
            health: vec![],
            peers: vec![],
        }
    }

    pub fn is_running(&self) -> bool {
        self.state == TailnetState::Running
    }

    pub fn peer_by_id(&self, id: &str) -> Option<&Peer> {
        self.peers.iter().find(|p| p.id == id)
    }

    pub fn peer_by_ip(&self, ip: IpAddr) -> Option<&Peer> {
        self.peers.iter().find(|p| p.ips.contains(&ip))
    }
}

fn parse_ips(ips: &Option<Vec<String>>) -> Vec<IpAddr> {
    ips.iter()
        .flatten()
        .filter_map(|s| s.parse().ok())
        .collect()
}

impl RawPeer {
    fn into_peer(self) -> Peer {
        let link = if !self.cur_addr.is_empty() {
            PeerLink::Direct {
                addr: self.cur_addr.clone(),
            }
        } else if !self.peer_relay.is_empty() {
            PeerLink::PeerRelay {
                via: self.peer_relay.clone(),
            }
        } else if self.active && !self.relay.is_empty() {
            PeerLink::Derp {
                region: self.relay.clone(),
            }
        } else {
            PeerLink::Idle
        };
        Peer {
            ips: parse_ips(&self.tailscale_ips),
            id: self.id,
            hostname: self.host_name,
            dns_name: self.dns_name.trim_end_matches('.').to_string(),
            os: self.os,
            online: self.online,
            link,
        }
    }
}

impl RawStatus {
    pub fn into_snapshot(self) -> TailnetSnapshot {
        let state = match self.backend_state.as_str() {
            "NeedsLogin" => TailnetState::NeedsLogin {
                auth_url: Some(self.auth_url.clone()).filter(|u| !u.is_empty()),
            },
            "NeedsMachineAuth" => TailnetState::NeedsMachineAuth,
            "Running" => TailnetState::Running,
            "Stopped" => TailnetState::Stopped,
            // NoState, Starting, InUseOtherUser...
            _ if !self.auth_url.is_empty() => TailnetState::NeedsLogin {
                auth_url: Some(self.auth_url.clone()),
            },
            _ => TailnetState::Starting,
        };
        let mut peers: Vec<Peer> = self
            .peer
            .unwrap_or_default()
            .into_values()
            .map(RawPeer::into_peer)
            .collect();
        peers.sort_by(|a, b| {
            b.is_android()
                .cmp(&a.is_android())
                .then(b.online.cmp(&a.online))
                .then_with(|| {
                    a.display_name()
                        .to_lowercase()
                        .cmp(&b.display_name().to_lowercase())
                })
        });
        TailnetSnapshot {
            state,
            self_name: self
                .self_peer
                .as_ref()
                .map(|p| p.dns_name.trim_end_matches('.').to_string()),
            self_ips: parse_ips(&self.tailscale_ips),
            tailnet: self
                .current_tailnet
                .map(|t| t.name)
                .filter(|n| !n.is_empty()),
            health: self.health.unwrap_or_default(),
            peers,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUNNING: &str = include_str!("../../tests/fixtures/status_running.json");
    const NEEDS_LOGIN: &str = include_str!("../../tests/fixtures/status_needs_login.json");

    #[test]
    fn parses_running_status() {
        let raw: RawStatus = serde_json::from_str(RUNNING).unwrap();
        let snap = raw.into_snapshot();
        assert_eq!(snap.state, TailnetState::Running);
        assert_eq!(
            snap.self_name.as_deref(),
            Some("phonebridge-mac.tail1234.ts.net")
        );
        assert_eq!(snap.tailnet.as_deref(), Some("cjscode@github"));
        // Android peers sorted first.
        assert_eq!(snap.peers[0].hostname, "Pixel-9");
        assert!(snap.peers[0].is_android());
        assert_eq!(
            snap.peers[0].ipv4(),
            Some("100.101.102.103".parse().unwrap())
        );
        assert_eq!(
            snap.peers[0].link,
            PeerLink::Direct {
                addr: "203.0.113.7:41641".into()
            }
        );
        assert_eq!(snap.peers[0].display_name(), "pixel-9");
        let laptop = snap
            .peers
            .iter()
            .find(|p| p.hostname == "old-laptop")
            .unwrap();
        assert!(!laptop.online);
        assert_eq!(laptop.link, PeerLink::Idle);
        let server = snap.peers.iter().find(|p| p.hostname == "nas").unwrap();
        assert_eq!(
            server.link,
            PeerLink::Derp {
                region: "ord".into()
            }
        );
        assert!(snap.peer_by_id("nTEST1CNTRL").is_some());
    }

    #[test]
    fn parses_needs_login() {
        let raw: RawStatus = serde_json::from_str(NEEDS_LOGIN).unwrap();
        let snap = raw.into_snapshot();
        assert_eq!(
            snap.state,
            TailnetState::NeedsLogin {
                auth_url: Some("https://login.tailscale.com/a/abc123".into())
            }
        );
        assert!(snap.peers.is_empty());
    }

    #[test]
    fn tolerates_minimal_json() {
        let raw: RawStatus =
            serde_json::from_str(r#"{"BackendState":"NoState","Peer":null}"#).unwrap();
        assert_eq!(raw.into_snapshot().state, TailnetState::Starting);
    }
}
