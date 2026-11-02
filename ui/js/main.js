// @ts-check
// Entry point: load the overview, subscribe to backend events, pick the view.

import { call, on } from "./api.js";
import { renderBanner, renderPills } from "./components/status-banner.js";
import { showError } from "./components/toast.js";
import { h, replace } from "./dom.js";
import { set, state, subscribe } from "./store.js";
import * as home from "./views/home.js";
import * as login from "./views/login.js";
import { openSettings } from "./views/settings.js";
import * as setup from "./views/setup.js";

/** @type {{name: string, update: () => void} | null} */
let current = null;

function pickView() {
  if (state.net?.state.kind !== "running") return "login";
  if (!state.settings?.phone || state.forceSetup) return "setup";
  return "home";
}

function render() {
  renderPills();
  renderBanner();
  const fatal = document.getElementById("fatal");
  if (fatal) {
    fatal.hidden = !state.toolsError;
    if (state.toolsError) replace(fatal, h("div.text", null, h("strong", null, state.toolsError.message), " ", state.toolsError.hint ?? ""));
  }
  const name = pickView();
  const root = /** @type {HTMLElement} */ (document.getElementById("view"));
  if (current?.name !== name) {
    const mod = { login, setup, home }[name];
    current = { name, ...mod.mount(root) };
  }
  current.update();
}

async function init() {
  document.getElementById("settings-btn")?.addEventListener("click", openSettings);
  subscribe(render);

  // Subscribe first so no event is missed between the overview and listening.
  await Promise.all([
    on("net", (net) => set({ net })),
    on("connection", (connection) => set({ connection })),
    on("settings", (settings) => set({ settings, managementUrl: settings.management_url })),
    on("windows", (windows) => set({ windows })),
    on("app-error", (e) => showError(e)),
  ]);

  const o = await call("get_overview");
  set({
    net: o.net,
    connection: o.connection,
    settings: o.settings,
    windows: o.windows,
    deviceName: o.device_name,
    managementUrl: o.management_url,
    toolsError: o.tools_error,
  });
}

init().catch((e) => {
  showError(e);
  render();
});
