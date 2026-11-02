// @ts-check
// Shown until our tailnet node is Running.

import { call } from "../api.js";
import { busy, h, replace } from "../dom.js";
import { state } from "../store.js";
import { showToast } from "../components/toast.js";

/** @param {HTMLElement} root */
export function mount(root) {
  const status = h("div");
  const keyInput = /** @type {HTMLInputElement} */ (h("input", { type: "password", placeholder: "tskey-auth-…", autocomplete: "off" }));
  const keyBtn = /** @type {HTMLButtonElement} */ (h("button", null, "Use auth key"));
  keyBtn.onclick = () =>
    busy(keyBtn, async () => {
      await call("tailnet_login_with_key", { key: keyInput.value });
      keyInput.value = "";
      showToast("Auth key accepted. Joining your tailnet…");
    }).catch(() => {});

  replace(
    root,
    h(
      "div.card",
      null,
      h("h1", null, "Connect PhoneBridge to your tailnet"),
      h(
        "p.muted",
        null,
        `PhoneBridge joins your Tailscale network as its own device ("${state.hostname}"). Nothing is installed on your Mac, and no VPN is turned on.`,
      ),
      status,
      h("h2", null, "Or use an auth key"),
      h("p.muted.small", null, "Create one at login.tailscale.com → Settings → Keys. Useful if the browser login doesn't work."),
      h("div.row", null, keyInput, keyBtn),
    ),
  );
  return { update: () => updateStatus(status) };
}

/** @param {HTMLElement} el */
function updateStatus(el) {
  const s = state.tailnet?.state ?? { kind: "starting" };
  const openBtn = /** @type {HTMLButtonElement} */ (h("button.primary", null, "Open login page"));
  openBtn.onclick = () => busy(openBtn, () => call("tailnet_open_login")).catch(() => {});
  switch (s.kind) {
    case "needs_login":
      replace(
        el,
        s.auth_url
          ? [
              h("p", null, h("span.spinner"), " Waiting for Tailscale login… Finish signing in in your browser."),
              h("div.row", null, openBtn),
              h("p.small.muted", null, "Link: ", h("code", null, s.auth_url)),
            ]
          : h("p", null, h("span.spinner"), " Getting a login link from Tailscale…"),
      );
      break;
    case "needs_machine_auth":
      replace(el, h("p", null, h("span.spinner"), " Logged in. Waiting for an admin to approve this device in the Tailscale admin console."));
      break;
    case "error":
      replace(
        el,
        h("p", null, h("strong", null, "Tailnet error: "), s.message ?? "unknown"),
        h("p.muted.small", null, "Check your internet connection. PhoneBridge keeps retrying."),
      );
      break;
    case "stopped":
      replace(el, h("p", null, "The tailnet node is stopped."));
      break;
    default:
      replace(el, h("p", null, h("span.spinner"), " Starting the tailnet node…"));
  }
}
