// @ts-check
// Thin wrapper over Tauri IPC. Every failed command shows a toast, so nothing
// fails silently; callers can still catch to update their own UI.

import { showError } from "./components/toast.js";

/** @typedef {{code: string, message: string, hint?: string}} AppError */

const tauri = /** @type {any} */ (window).__TAURI__;

/**
 * @param {string} cmd
 * @param {Record<string, unknown>} [args]
 * @param {{quiet?: boolean}} [opts] quiet: don't toast on error
 */
export async function call(cmd, args = {}, opts = {}) {
  try {
    return await tauri.core.invoke(cmd, args);
  } catch (e) {
    if (!opts.quiet) showError(e);
    throw e;
  }
}

/**
 * @param {string} event
 * @param {(payload: any) => void} handler
 */
export function on(event, handler) {
  return tauri.event.listen(event, (/** @type {{payload: any}} */ e) => handler(e.payload));
}
