// @ts-check
// App state mirrored from the Rust side via events, plus a little UI state.

/**
 * @typedef {{kind: string, auth_url?: string|null, user_code?: string|null, reason?: string|null, message?: string, retry_in_secs?: number|null}} NetState
 * @typedef {{kind: string, addr?: string, via?: string}} PeerLink
 * @typedef {{id: string, hostname: string, dns_name: string, os: string, ips: string[], online: boolean, link: PeerLink, latency_ms: number|null}} Peer
 * @typedef {{state: NetState, self_name: string|null, self_ips: string[], network: string|null, health: string[], peers: Peer[]}} NetSnapshot
 * @typedef {{kind: string, serial?: string, link?: PeerLink|null, attempt?: number, retry_in_secs?: number, reason?: string, detail?: string}} ConnectionStatus
 * @typedef {{node_id: string, name: string, ip: string}} PhoneConfig
 * @typedef {{name: string, package: string, system: boolean}} AndroidApp
 * @typedef {{mirror_audio: boolean, app_audio: boolean, flex_display: boolean, h265: boolean, force_adb_forward: boolean, always_on_top: boolean}} WindowOptions
 * @typedef {{management_url: string, phone: PhoneConfig|null, forwarder_port: number|null, adb_server_port: number, favorites: string[], preset: "wifi"|"cellular", window: WindowOptions, apps: AndroidApp[]}} Settings
 * @typedef {{id: number, label: string, package: string|null, pid: number}} WindowInfo
 */

export const state = {
  /** @type {NetSnapshot|null} */ net: null,
  /** @type {ConnectionStatus} */ connection: { kind: "no_phone" },
  /** @type {Settings|null} */ settings: null,
  /** @type {WindowInfo[]} */ windows: [],
  deviceName: "phonebridge-mac",
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
