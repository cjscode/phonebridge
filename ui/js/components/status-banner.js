// @ts-check
// Connection banner + the two status pills in the top bar.

import { call } from "../api.js";
import { busy, h, replace } from "../dom.js";
import { showToast } from "./toast.js";
import { linkText, phoneName, state } from "../store.js";

export function renderPills() {
  const t = document.getElementById("tailnet-pill");
  const c = document.getElementById("conn-pill");
  if (!t || !c) return;
  const kind = state.tailnet?.state.kind ?? "starting";
  const [tCls, tText] = {
    running: ["ok", "Tailnet online"],
    needs_login: ["warn", "Tailscale: logged out"],
    needs_machine_auth: ["warn", "Tailscale: awaiting approval"],
    error: ["err", "Tailnet error"],
    stopped: ["err", "Tailnet stopped"],
  }[kind] ?? ["", "Tailnet starting…"];
  t.className = `pill ${tCls}`;
  t.textContent = tText;

  const s = state.connection;
  const [cCls, cText] = {
    connected: ["ok", `Connected${linkText(s.link) ? " · " + linkText(s.link) : ""}`],
    connecting: ["", "Connecting…"],
    reconnecting: ["warn", "Reconnecting…"],
    phone_needs_usb: ["warn", "Needs USB"],
    phone_unreachable: ["warn", "Phone unreachable"],
    tailnet_offline: ["warn", "Waiting for tailnet"],
    unauthorized: ["warn", "Allow on phone"],
    disconnected: ["", "Disconnected"],
  }[s.kind] ?? ["", "No phone"];
  c.className = `pill ${cCls}`;
  c.textContent = cText;
  c.hidden = s.kind === "no_phone";
}

/** @param {string} label @param {string} cmd @param {string} [okMsg] */
function action(label, cmd, okMsg) {
  const b = /** @type {HTMLButtonElement} */ (h("button", null, label));
  b.onclick = () =>
    busy(b, async () => {
      const r = await call(cmd).catch(() => null);
      if (r && r.warning) showToast(r.warning, { kind: "error" });
      else if (r !== null && okMsg) showToast(okMsg);
    });
  return b;
}

export function renderBanner() {
  const root = document.getElementById("banner");
  if (!root) return;
  const s = state.connection;
  const name = phoneName();
  const retry = s.retry_in_secs ? ` Retrying in ${s.retry_in_secs}s.` : "";
  /** @type {[string, any, HTMLElement[]] | null} */
  let b = null;
  if (state.tailnet?.state.kind !== "running" || state.forceSetup) {
    b = null; // login / setup views explain themselves
  } else
    switch (s.kind) {
      case "connected":
        b = ["ok", `Connected to ${name}${linkText(s.link) ? ` (${linkText(s.link)})` : ""}.`, []];
        break;
      case "connecting":
        b = ["", [h("span.spinner"), ` Connecting to ${name}…`], []];
        break;
      case "reconnecting":
        b = ["warn", [h("span.spinner"), ` Reconnecting to ${name}… ${s.reason ?? ""}${retry}`], [action("Retry now", "connect_now")]];
        break;
      case "phone_needs_usb":
        b = [
          "warn",
          [h("strong", null, `${name} restarted`), ", which turns off wireless debugging. Plug it in with USB once, then click Re-enable."],
          [action("Re-enable via USB", "usb_reenable", "Re-enabled. Reconnecting…")],
        ];
        break;
      case "phone_unreachable":
        b = ["warn", `Can't reach ${name}: ${s.detail ?? ""}.${retry}`, [action("Retry now", "connect_now")]];
        break;
      case "tailnet_offline":
        b = ["warn", `Waiting for the tailnet: ${s.detail ?? ""}`, []];
        break;
      case "unauthorized":
        b = ["warn", `Unlock ${name} and tap "Allow" on the "Allow USB debugging?" prompt.`, []];
        break;
      case "disconnected":
        b = ["", `Disconnected from ${name}.`, [action("Connect", "connect_now")]];
        break;
    }
  if (!b) {
    replace(root);
    return;
  }
  const [cls, text, actions] = b;
  replace(root, h("div.banner", { class: cls }, h("div.text", null, text), h("div.actions", null, actions)));
}
