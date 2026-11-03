// @ts-check
// Shown until our NetBird peer / Tailscale node is running: pick the network,
// then browser login, setup/auth key, or a self-hosted server.

import { call } from "../api.js";
import { busy, h, replace } from "../dom.js";
import { currentNetwork, netLabel, set, state } from "../store.js";
import { networkPicker } from "../components/network-picker.js";
import { showToast } from "../components/toast.js";

/** Per-network wording. */
function texts() {
  if (currentNetwork() === "tailscale") {
    return {
      keyPlaceholder: "Auth key (tskey-auth-…)",
      keyButton: "Use auth key",
      keyTitle: "Or use an auth key",
      keyHelp: "Create one in the Tailscale admin console under Settings → Keys. Devices added with a non-ephemeral key stay signed in until their key expires (you can disable expiry for this device in the admin console).",
      serverTitle: "Headscale server",
      serverHelp: "Leave this as Tailscale's server unless you run Headscale. Clear the field to go back to Tailscale.",
    };
  }
  return {
    keyPlaceholder: "Setup key (XXXXXXXX-XXXX-…)",
    keyButton: "Use setup key",
    keyTitle: "Or use a setup key",
    keyHelp: "Create one in the NetBird dashboard under Setup Keys. Devices added with a setup key never need to sign in again.",
    serverTitle: "Self-hosted NetBird server",
    serverHelp: "Leave this as NetBird Cloud unless you run your own NetBird management server.",
  };
}

/** @param {HTMLElement} root */
export function mount(root) {
  const status = h("div");
  const net = netLabel();
  const t = texts();

  const keyInput = /** @type {HTMLInputElement} */ (
    h("input", { type: "password", placeholder: t.keyPlaceholder, autocomplete: "off" })
  );
  const keyBtn = /** @type {HTMLButtonElement} */ (h("button", null, t.keyButton));
  keyBtn.onclick = () =>
    busy(keyBtn, async () => {
      await call("net_login_setup_key", { key: keyInput.value });
      keyInput.value = "";
      showToast(`Joined your ${net} network.`);
    }).catch(() => {});

  const serverInput = /** @type {HTMLInputElement} */ (h("input", { type: "text", value: state.managementUrl, spellcheck: false }));
  const serverBtn = /** @type {HTMLButtonElement} */ (h("button", null, "Save"));
  serverBtn.onclick = () =>
    busy(serverBtn, async () => {
      const url = await call("net_set_management_url", { url: serverInput.value });
      serverInput.value = url;
      set({ managementUrl: url });
      showToast(`Using ${url}. Sign in again.`);
    }).catch(() => {});

  replace(
    root,
    h(
      "div.card",
      null,
      h("div.row", null, h("h1.spacer", null, `Connect PhoneBridge to ${net}`), networkPicker()),
      h(
        "p.muted",
        null,
        `PhoneBridge joins your ${net} network as its own device ("${state.deviceName}"). Nothing is installed on your Mac, and no VPN is turned on. Use the network your phone's VPN app is on.`,
      ),
      status,
      h("h2", null, t.keyTitle),
      h("p.muted.small", null, t.keyHelp),
      h("div.row", null, keyInput, keyBtn),
      h(
        "details",
        { style: "margin-top:16px" },
        h("summary.small.muted", null, t.serverTitle),
        h("p.muted.small", null, t.serverHelp),
        h("div.row", null, serverInput, serverBtn),
      ),
    ),
  );
  return { update: () => updateStatus(status) };
}

/** @param {HTMLElement} el */
function updateStatus(el) {
  const net = netLabel();
  const s = state.net?.state ?? { kind: "starting" };
  const signIn = /** @type {HTMLButtonElement} */ (h("button.primary", null, "Sign in with browser"));
  signIn.onclick = () => busy(signIn, () => call("net_login_browser")).catch(() => {});
  const reopen = /** @type {HTMLButtonElement} */ (h("button", null, "Open login page again"));
  reopen.onclick = () => busy(reopen, () => call("net_open_login")).catch(() => {});
  const retry = /** @type {HTMLButtonElement} */ (h("button", null, "Retry now"));
  retry.onclick = () => busy(retry, () => call("net_retry")).catch(() => {});

  switch (s.kind) {
    case "needs_login":
      if (s.auth_url) {
        replace(
          el,
          h("p", null, h("span.spinner"), ` Waiting for ${net} login… Finish signing in in your browser.`),
          s.user_code ? h("p", null, "If asked, enter the code ", h("strong.selectable", null, s.user_code), ".") : null,
          h("div.row", null, reopen, signIn),
          h("p.small.muted", null, "Link: ", h("code", null, s.auth_url)),
        );
      } else {
        replace(
          el,
          s.reason ? h("p", null, h("strong", null, s.reason)) : null,
          h("div.row", null, signIn),
          h("p.small.muted", null, `Server: ${state.managementUrl}`),
        );
      }
      break;
    case "error":
      replace(
        el,
        h("p", null, h("strong", null, `Can't connect to ${net}: `), s.message ?? "unknown error"),
        h(
          "p.muted.small",
          null,
          s.retry_in_secs ? `Retrying in ${s.retry_in_secs}s. ` : "",
          "Check your internet connection.",
        ),
        h("div.row", null, retry),
      );
      break;
    case "stopped":
      replace(el, h("p", null, `${net} is stopped.`));
      break;
    default:
      replace(el, h("p", null, h("span.spinner"), ` Connecting to ${net}…`));
  }
}
