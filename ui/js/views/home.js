// @ts-check
// Main view: full mirror, quality preset, open windows, favourites, app grid.

import { call } from "../api.js";
import { busy, h, replace } from "../dom.js";
import { set, state } from "../store.js";

const COLORS = ["#2f7cf6", "#1f9d55", "#d9480f", "#7048e8", "#c2255c", "#0c8599", "#e67700", "#5c7cfa", "#2b8a3e", "#ae3ec9"];

/** @param {string} s */
function colorFor(s) {
  let x = 0;
  for (const c of s) x = (x * 31 + c.charCodeAt(0)) >>> 0;
  return COLORS[x % COLORS.length];
}

/** @param {HTMLElement} root */
export function mount(root) {
  const mirrorBtn = /** @type {HTMLButtonElement} */ (h("button.primary", null, "Full mirror"));
  mirrorBtn.onclick = () => busy(mirrorBtn, () => call("open_mirror")).catch(() => {});
  const refreshBtn = /** @type {HTMLButtonElement} */ (h("button", { title: "Reload the app list from the phone" }, "Refresh apps"));
  refreshBtn.onclick = () => loadApps(true, refreshBtn);
  const preset = h("div.segmented");
  const search = /** @type {HTMLInputElement} */ (h("input.search", { type: "search", placeholder: "Search apps", value: state.search }));
  search.oninput = () => {
    state.search = search.value;
    renderGrid();
  };
  const windows = h("div.chips");
  const favs = h("div");
  const grid = h("div");

  replace(
    root,
    h("div.toolbar", null, mirrorBtn, preset, h("div.spacer"), search, refreshBtn),
    windows,
    favs,
    h("div.section-title", null, "All apps"),
    grid,
  );

  const connected = () => state.connection.kind === "connected";

  /** @param {import("../store.js").AndroidApp} app */
  const tile = (app) => {
    const fav = state.settings?.favorites.includes(app.package) ?? false;
    const star = h("button.star", { class: fav ? "on" : "", title: fav ? "Remove from favorites" : "Add to favorites" }, fav ? "★" : "☆");
    star.onclick = (/** @type {Event} */ e) => {
      e.stopPropagation();
      call("set_favorite", { package: app.package, favorite: !fav })
        .then((s) => set({ settings: s }))
        .catch(() => {});
    };
    return h(
      "div.tile",
      {
        class: connected() ? "" : "disabled",
        title: `${app.name}\n${app.package}`,
        onclick: () => connected() && call("open_app", { package: app.package, name: app.name }).catch(() => {}),
      },
      star,
      h("div.avatar", { style: `background:${colorFor(app.package)}` }, (app.name.trim()[0] || "?").toUpperCase()),
      h("div.name", null, app.name),
    );
  };

  const renderGrid = () => {
    const apps = state.settings?.apps ?? [];
    const q = state.search.trim().toLowerCase();
    const shown = q ? apps.filter((a) => a.name.toLowerCase().includes(q) || a.package.includes(q)) : apps;
    if (!apps.length) {
      replace(
        grid,
        h(
          "div.empty",
          null,
          state.appsLoading ? [h("span.spinner"), " Loading apps from the phone…"] : connected() ? "No apps loaded yet." : "Apps appear here once the phone is connected.",
        ),
      );
    } else if (!shown.length) {
      replace(grid, h("div.empty", null, `No apps match "${state.search}".`));
    } else {
      replace(grid, h("div.grid", null, shown.map(tile)));
    }
  };

  const update = () => {
    mirrorBtn.disabled = !connected();
    refreshBtn.disabled = !connected() || state.appsLoading;
    const p = state.settings?.preset ?? "wifi";
    replace(
      preset,
      [
        ["wifi", "Wi-Fi quality"],
        ["cellular", "Cellular (low data)"],
      ].map(([val, label]) =>
        h(
          "button",
          {
            class: p === val ? "active" : "",
            onclick: () =>
              call("set_preset", { preset: val })
                .then((s) => set({ settings: s }))
                .catch(() => {}),
          },
          label,
        ),
      ),
    );
    replace(
      windows,
      state.windows.map((w) =>
        h("span.chip", null, w.label, h("button", { title: "Close window", onclick: () => call("close_window", { id: w.id }).catch(() => {}) }, "×")),
      ),
    );
    const apps = state.settings?.apps ?? [];
    const favApps = (state.settings?.favorites ?? [])
      .map((pkg) => apps.find((a) => a.package === pkg) ?? { name: pkg, package: pkg, system: false });
    replace(favs, favApps.length ? [h("div.section-title", null, "Favorites"), h("div.grid", null, favApps.map(tile))] : null);
    renderGrid();
  };

  /** @param {boolean} refresh @param {HTMLButtonElement} [btn] */
  const loadApps = async (refresh, btn) => {
    if (state.appsLoading) return;
    set({ appsLoading: true });
    try {
      const run = () => call("list_apps", { refresh });
      const apps = btn ? await busy(btn, run) : await run();
      if (state.settings) set({ settings: { ...state.settings, apps } });
    } catch {
      /* toast already shown */
    } finally {
      set({ appsLoading: false });
    }
  };

  // First connection with no cached list: fetch it.
  let fetchedOnce = false;
  return {
    update: () => {
      update();
      if (connected() && !fetchedOnce && !(state.settings?.apps.length)) {
        fetchedOnce = true;
        loadApps(true);
      }
    },
  };
}
