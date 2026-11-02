// @ts-check
// Settings dialog: tailnet, phone, window options, diagnostics.

import { call } from "../api.js";
import { busy, h, replace } from "../dom.js";
import { linkText, set, state } from "../store.js";
import { showToast } from "../components/toast.js";

/** @param {string} label @param {() => Promise<any>} fn @param {string} [cls] */
function btn(label, fn, cls = "") {
  const b = /** @type {HTMLButtonElement} */ (h("button", { class: cls }, label));
  b.onclick = () => busy(b, fn).catch(() => {});
  return b;
}

/** @param {[string, any][]} rows */
function kv(rows) {
  return h("dl.kv", null, rows.flatMap(([k, v]) => [h("dt", null, k), h("dd", null, v ?? "—")]));
}

export function openSettings() {
  const dlg = /** @type {HTMLDialogElement} */ (document.getElementById("settings"));
  const body = h("div.dialog-body");
  const diag = h("div", null, h("p.muted", null, h("span.spinner"), " Loading…"));
  replace(dlg, h("div.dialog-head", null, h("strong.spacer", null, "Settings"), h("button", { onclick: () => dlg.close() }, "Done")), body);

  const render = () => {
    const t = state.tailnet;
    const s = state.settings;
    const phone = s?.phone;
    const peer = phone ? t?.peers.find((p) => p.id === phone.node_id) : null;
    const w = s?.window;
    /** @param {keyof import("../store.js").WindowOptions} key @param {string} label @param {string} [hint] */
    const opt = (key, label, hint) =>
      h(
        "label.check",
        null,
        h("input", {
          type: "checkbox",
          checked: !!w?.[key],
          onchange: (/** @type {Event} */ e) => {
            if (!w) return;
            const options = { ...w, [key]: /** @type {HTMLInputElement} */ (e.target).checked };
            call("set_window_options", { options }).then((ns) => set({ settings: ns })).catch(() => {});
          },
        }),
        h("div", null, label, hint ? h("div.small.muted", null, hint) : null),
      );

    replace(
      body,
      h("h2", null, "Tailscale"),
      kv([
        ["This device", t?.self_name ?? state.hostname],
        ["Addresses", t?.self_ips.join(", ")],
        ["Tailnet", t?.tailnet],
        ["State", t?.state.kind],
      ]),
      t?.health.length ? h("p.small.muted", null, t.health.join(" · ")) : null,
      h(
        "div.row",
        null,
        btn("Log out of Tailscale", async () => {
          if (!confirm("Log PhoneBridge out of your tailnet? Open windows will close.")) return;
          await call("tailnet_logout");
          showToast("Logged out. A new login link will open.");
          dlg.close();
        }, "danger"),
      ),

      h("h2", null, "Phone"),
      phone
        ? [
            kv([
              ["Name", phone.name],
              ["Tailnet IP", phone.ip],
              ["Status", state.connection.kind.replaceAll("_", " ")],
              ["Path", peer ? (linkText(peer.link) || (peer.online ? "idle" : "offline")) : "not on tailnet"],
            ]),
            h(
              "div.row.wrap",
              null,
              state.connection.kind === "connected" ? btn("Disconnect", () => call("disconnect")) : btn("Connect", () => call("connect_now")),
              btn("Re-enable via USB", async () => {
                const r = await call("usb_reenable");
                showToast(r.warning ?? "Re-enabled. Reconnecting…", { kind: r.warning ? "error" : "info" });
              }),
              h("button", { onclick: () => (dlg.close(), set({ forceSetup: true })) }, "Set up a phone via USB…"),
              btn("Forget phone", async () => {
                if (!confirm(`Forget ${phone.name}?`)) return;
                await call("forget_phone");
                dlg.close();
              }, "danger"),
            ),
          ]
        : h("p.muted", null, "No phone set up yet."),

      h("h2", null, "Windows"),
      opt("mirror_audio", "Play phone audio in the full mirror", "Android 11+."),
      opt("app_audio", "Play audio in app windows", "Only one window can capture audio at a time."),
      opt("flex_display", "Resizable app windows", "The app's virtual screen resizes with the window."),
      opt("h265", "Use H.265 video", "Better quality per bit. Turn off if windows stay black."),
      opt("always_on_top", "Keep windows on top"),
      opt("force_adb_forward", "Use adb forward instead of adb reverse", "Troubleshooting only."),

      h("h2", null, "Diagnostics"),
      diag,
    );
  };

  const loadDiag = async () => {
    try {
      const d = await call("diagnostics", {}, { quiet: true });
      const text = JSON.stringify(d, null, 2);
      replace(
        diag,
        kv([
          ["adb", h("code", null, d.adb_path)],
          ["adb version", d.adb_version],
          ["scrcpy", h("code", null, d.scrcpy_path)],
          ["scrcpy version", d.scrcpy_version],
          ["scrcpy-server", h("code", null, d.scrcpy_server_path)],
          ["Private adb port", d.adb_server_port],
          ["Forwarder", d.forwarder_port ? `127.0.0.1:${d.forwarder_port}` : "not running"],
          [
            "Forwarded",
            d.forwarder_stats
              ? `${fmtBytes(d.forwarder_stats.bytes_down)} down / ${fmtBytes(d.forwarder_stats.bytes_up)} up, ${d.forwarder_stats.total_connections} connection(s)`
              : "—",
          ],
          ["Tailnet state", h("code", null, d.tailnet_state_dir)],
          ["Logs", h("code", null, d.log_dir)],
          ["Other installs", d.system_copies.length ? d.system_copies.join(", ") + " (not used)" : "none found"],
        ]),
        h(
          "div.row",
          { style: "margin-top:10px" },
          btn("Open logs folder", () => call("open_logs")),
          btn("Copy diagnostics", async () => {
            await navigator.clipboard.writeText(text);
            showToast("Copied.");
          }),
        ),
        h("p.small.muted", null, "Third-party licences are in PhoneBridge.app/Contents/Resources/licenses."),
      );
    } catch (e) {
      replace(diag, h("p", null, "Couldn't load diagnostics: ", /** @type {any} */ (e)?.message ?? String(e)));
    }
  };

  render();
  loadDiag();
  dlg.showModal();
}

/** @param {number} n */
function fmtBytes(n) {
  if (n < 1024) return `${n} B`;
  if (n < 1024 ** 2) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 ** 3) return `${(n / 1024 ** 2).toFixed(1)} MB`;
  return `${(n / 1024 ** 3).toFixed(2)} GB`;
}
