// @ts-check
// App state mirrored from the Rust side via events, plus a little UI state.

/**
 * @typedef {{kind: string, auth_url?: string|null, user_code?: string|null, reason?: string|null, message?: string, retry_in_secs?: number|null}} NetState
 * @typedef {{kind: string, addr?: string, via?: string}} PeerLink
 * @typedef {{id: string, hostname: string, dns_name: string, os: string, ips: string[], online: boolean, link: PeerLink, latency_ms: number|null}} Peer
 * @typedef {"netbird"|"tailscale"} Network
 * @typedef {{kind: Network, state: NetState, synced: boolean, self_name: string|null, self_ips: string[], network: string|null, health: string[], peers: Peer[]}} NetSnapshot
 * @typedef {{kind: string, serial?: string, link?: PeerLink|null, attempt?: number, retry_in_secs?: number, reason?: string, detail?: string}} ConnectionStatus
 * @typedef {{node_id: string, name: string, ip: string}} PhoneConfig
 * @typedef {{name: string, package: string, system: boolean}} AndroidApp
 * @typedef {{mirror_audio: boolean, app_audio: boolean, flex_display: boolean, h265: boolean, force_adb_forward: boolean, always_on_top: boolean}} WindowOptions
 * @typedef {{network: Network, management_url: string, tailscale_control_url: string, phone: PhoneConfig|null, other_phones: Record<string, PhoneConfig>, forwarder_port: number|null, adb_server_port: number, favorites: string[], preset: "wifi"|"cellular", window: WindowOptions, apps: AndroidApp[]}} Settings
 * @typedef {{id: number, label: string, package: string|null, pid: number}} WindowInfo
 */

export const state = {
  /** @type {NetSnapshot|null} */ net: null,
  /** @type {ConnectionStatus} */ connection: { kind: "no_phone" },
  /** @type {Settings|null} */ settings: null,
  /** @type {WindowInfo[]} */ windows: [],
  deviceName: "phonebridge-mac",
  /** @type {Network} */ network: "netbird",
  /** Server of the current network (NetBird management URL or Tailscale control URL). */
  managementUrl: "https://api.netbird.io:443",
  /** @type {any} */ toolsError: null,
  // UI-only
  forceSetup: false,
  appsLoading: false,
  search: "",
};

/** @type {Set<() => void>} */
const listeners = new Set();

/** @param {Partial<typeof state>} patch */
export function set(patch) {
  Object.assign(state, patch);
  for (const l of listeners) l();
}

/** @param {() => void} fn */
export function subscribe(fn) {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

/** The network the user chose (settings win over a status from before a switch). */
export function currentNetwork() {
  return state.settings?.network ?? state.net?.kind ?? state.network;
}

/** "NetBird" or "Tailscale": the network in use. */
export function netLabel() {
  return labelOf(currentNetwork());
}

/** @param {Network} n */
export function labelOf(n) {
  return n === "tailscale" ? "Tailscale" : "NetBird";
}

/** The current network's server URL, from saved settings. @param {Settings} s */
export function serverUrlOf(s) {
  return s.network === "tailscale" ? s.tailscale_control_url : s.management_url;
}

export function phoneName() {
  return state.settings?.phone?.name || "your phone";
}

/** @param {PeerLink|null|undefined} link */
export function linkText(link) {
  switch (link?.kind) {
    case "direct":
      return "direct";
    case "relayed":
      return link.via ? `relayed via ${link.via}` : "relayed";
    default:
      return "";
  }
}
