// @ts-check
// Tiny DOM helper: h("div.card", {onclick}, child, "text", ...)

/**
 * @param {string} tag  e.g. "button.primary#go"
 * @param {Record<string, any> | null} [props]
 * @param {...any} children
 * @returns {HTMLElement}
 */
export function h(tag, props, ...children) {
  const [name, ...rest] = tag.split(/(?=[.#])/);
  const el = document.createElement(name || "div");
  for (const part of rest) {
    if (part.startsWith(".")) el.classList.add(part.slice(1));
    else if (part.startsWith("#")) el.id = part.slice(1);
  }
  for (const [k, v] of Object.entries(props || {})) {
    if (v === undefined || v === null || v === false) continue;
    if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else if (k === "class") el.className += " " + v;
    else if (k === "style") el.setAttribute("style", v);
    else if (k in el && k !== "list") /** @type {any} */ (el)[k] = v;
    else el.setAttribute(k, v === true ? "" : String(v));
  }
  for (const c of children.flat()) {
    if (c === null || c === undefined || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

/** @param {Element} el @param {...any} children */
export function replace(el, ...children) {
  el.replaceChildren(...children.flat().filter((c) => c !== null && c !== undefined && c !== false));
}

/**
 * Run an async action with the button disabled and a spinner.
 * @param {HTMLButtonElement} btn
 * @param {() => Promise<any>} fn
 */
export async function busy(btn, fn) {
  const label = btn.textContent;
  btn.disabled = true;
  btn.replaceChildren(h("span.spinner"), " ", label || "");
  try {
    return await fn();
  } finally {
    btn.disabled = false;
    btn.textContent = label;
  }
}
