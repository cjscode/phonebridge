// @ts-check
// "Set up via USB": detect phone → pick tailnet peer → enable adb over TCP.

import { call } from "../api.js";
import { busy, h, replace } from "../dom.js";
import { set, state } from "../store.js";
import { showToast } from "../components/toast.js";

/** @param {HTMLElement} root */
export function mount(root) {
  /** @type {{phone: any, suggested_peer_id: string|null, peers: import("../store.js").Peer[]}|null} */
  let detected = null;
  /** @type {string|null} */
  let selected = null;
  const body = h("div");

  const render = () => {
    const detectBtn = /** @type {HTMLButtonElement} */ (h("button.primary", null, detected ? "Detect again" : "Detect phone"));
    detectBtn.onclick = () =>
      busy(detectBtn, async () => {
        detected = await call("usb_detect");
        selected = detected?.suggested_peer_id ?? null;
        render();
      }).catch(() => {});

    const steps = h(
      "ol.steps",
      null,
      h("li", null, "On the phone: Settings → About phone → tap ", h("strong", null, "Build number"), " 7 times to enable Developer options."),
      h("li", null, "Settings → System → Developer options → turn on ", h("strong", null, "USB debugging"), "."),
      h("li", null, "Make sure the ", h("strong", null, "Tailscale"), " app on the phone is connected."),
      h("li", null, "Plug the phone into this Mac with a data cable, unlock it, and tap ", h("strong", null, "Allow"), " if asked."),
    );

    /** @type {any[]} */
    let pick = [];
    if (detected) {
      const p = detected.phone;
      const peers = detected.peers;
      pick = [
        h("h2", null, `Found ${p.model || "a phone"}${p.android_version ? ` (Android ${p.android_version})` : ""}`),
        p.tailnet_ip
          ? h("p.muted", null, `Its tailnet address is ${p.tailnet_ip}.`)
          : h("p", null, "Tailscale doesn't seem to be running on the phone. Open the Tailscale app on it, then detect again, or pick it below."),
        h("p", null, "Which tailnet device is this phone?"),
        peers.length
          ? h(
              "ul.peer-list",
              null,
              peers.map((peer) =>
                h(
                  "li",
                  { class: peer.id === selected ? "selected" : "", onclick: () => ((selected = peer.id), render()) },
                  h("input", { type: "radio", name: "peer", checked: peer.id === selected }),
                  h("div.spacer", null, h("div", null, peer.dns_name.split(".")[0] || peer.hostname), h("div.small.muted", null, `${peer.os} · ${peer.ips[0] ?? ""}`)),
                  peer.id === detected?.suggested_peer_id ? h("span.tag.match", null, "this phone") : null,
                  h("span.tag", null, peer.online ? "online" : "offline"),
                ),
              ),
            )
          : h("p.muted", null, "No other devices on your tailnet yet."),
        h("div.row", null, h("div.spacer"), enableButton(p.serial)),
      ];
    }

    replace(
      body,
      h(
        "div.card",
        null,
        h("div.row", null, h("h1.spacer", null, "Set up via USB"), state.settings?.phone ? h("button", { onclick: () => set({ forceSetup: false }) }, "Cancel") : null),
        h("p.muted", null, "One time only: PhoneBridge turns on wireless debugging over your tailnet, so the phone works from anywhere, even on cellular."),
        steps,
        h("div.row", null, detectBtn),
        pick,
      ),
    );
  };

  /** @param {string} serial */
  const enableButton = (serial) => {
    const b = /** @type {HTMLButtonElement} */ (h("button.primary", { disabled: !selected }, "Enable and connect"));
    b.onclick = () =>
      busy(b, async () => {
        const phone = await call("usb_setup", { serial, peerId: selected });
        showToast(`Saved ${phone.name}. You can unplug the cable now.`);
        set({ forceSetup: false });
      }).catch(() => {});
    return b;
  };

  render();
  replace(root, body);
  return { update: () => {} };
}
