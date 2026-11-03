// @ts-check
// NetBird | Tailscale switch. Each network keeps its own login and saved phone.

import { call } from "../api.js";
import { h } from "../dom.js";
import { currentNetwork, labelOf, serverUrlOf, set, state } from "../store.js";
import { showToast } from "./toast.js";

/** @param {{onSwitched?: () => void}} [opts] */
export function networkPicker(opts = {}) {
  const current = currentNetwork();
  /** @type {import("../store.js").Network[]} */
  const nets = ["netbird", "tailscale"];
  const buttons = nets.map((n) => {
    const b = /** @type {HTMLButtonElement} */ (h("button", { class: n === current ? "active" : "", title: `Join your ${labelOf(n)} network` }, labelOf(n)));
    b.onclick = async () => {
      if (n === currentNetwork()) return;
      if (state.windows.length && !confirm(`Switch to ${labelOf(n)}? Open phone windows will close.`)) return;
      for (const x of buttons) x.disabled = true;
      try {
        const settings = await call("net_set_network", { network: n });
        set({ settings, network: settings.network, managementUrl: serverUrlOf(settings) });
        showToast(`Using ${labelOf(n)}.`);
        opts.onSwitched?.();
      } catch {
        // call() already showed the error
      } finally {
        for (const x of buttons) x.disabled = false;
      }
    };
    return b;
  });
  return h("div.segmented", { role: "group", "aria-label": "Network" }, buttons);
}
