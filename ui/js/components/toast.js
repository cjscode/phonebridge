// @ts-check
import { h } from "../dom.js";

/**
 * @param {string} message
 * @param {{hint?: string, kind?: "error"|"info", timeout?: number}} [opts]
 */
export function showToast(message, opts = {}) {
  const root = document.getElementById("toasts");
  if (!root) return;
  const el = h(
    "div.toast",
    { class: opts.kind === "error" ? "error" : "" },
    h("button.close", { onclick: () => el.remove(), "aria-label": "Dismiss" }, "×"),
    h("div.selectable", null, message),
    opts.hint ? h("div.hint.selectable", null, opts.hint) : null,
  );
  root.append(el);
  // Errors stay until dismissed (or 20 s); info goes after 4 s.
  const ms = opts.timeout ?? (opts.kind === "error" ? 20000 : 4000);
  setTimeout(() => el.remove(), ms);
  while (root.children.length > 5) root.firstElementChild?.remove();
}

/** @param {any} e  AppError {message, hint} or anything else */
export function showError(e) {
  if (e && typeof e === "object" && "message" in e) {
    showToast(String(e.message), { hint: e.hint, kind: "error" });
  } else {
    showToast(String(e), { kind: "error" });
  }
}
