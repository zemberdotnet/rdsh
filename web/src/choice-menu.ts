// An anchored list that leaves focus and text editing with its caller.

export interface Choice<T> {
  label: string;
  detail?: string;
  value: T;
}

type ChoiceKey = Pick<KeyboardEvent, "key" | "keyCode" | "isComposing" | "defaultPrevented" | "ctrlKey" | "shiftKey" | "altKey" | "metaKey">;
type Action = "next" | "previous" | "accept" | "dismiss";

/** Maps only the menu's keys, leaving typing, text selection and other shortcuts alone. */
export function choiceMenuAction(e: ChoiceKey): Action | null {
  // Some IME keydowns (including confirmation) arrive outside isComposing with code 229.
  if (e.defaultPrevented || e.isComposing || e.keyCode === 229 || e.altKey || e.metaKey) return null;
  if (e.ctrlKey) {
    if (e.shiftKey) return null;
    const key = e.key.toLowerCase();
    return key === "n" ? "next" : key === "p" ? "previous" : null;
  }
  if (e.key === "Tab") return e.shiftKey ? "previous" : "next";
  if (e.shiftKey) return null;
  switch (e.key) {
    case "ArrowDown": return "next";
    case "ArrowUp": return "previous";
    case "Enter": return "accept";
    case "Escape": return "dismiss";
    default: return null;
  }
}

interface Menu<T> {
  el: HTMLElement;
  anchor: HTMLElement;
  rows: { el: HTMLElement; value: T }[];
  selected: number;
  onPick: (value: T) => void;
  owner?: HTMLElement;
  restoreOwner?: () => void;
}

let nextId = 0;
const clamp = (n: number, min: number, max: number) => Math.max(min, Math.min(n, max));

/** Link a focused textbox to the list without changing its role or losing any prior attrs. */
function linkOwner(owner: HTMLElement, id: string): () => void {
  const attrs = ["aria-controls", "aria-activedescendant"];
  const combobox = owner.getAttribute("role") === "combobox";
  if (combobox) attrs.push("aria-expanded");
  const saved = attrs.map((attr) => [attr, owner.getAttribute(attr)] as const);
  owner.setAttribute("aria-controls", id);
  if (combobox) owner.setAttribute("aria-expanded", "true");
  return () => {
    for (const [attr, value] of saved) {
      if (value === null) owner.removeAttribute(attr);
      else owner.setAttribute(attr, value);
    }
  };
}

export class ChoiceMenu<T> {
  private readonly label: string;
  private readonly id = `choice-menu-${++nextId}`;
  private menu: Menu<T> | null = null;

  constructor(label: string) {
    this.label = label;
  }

  get open(): boolean {
    return this.menu !== null;
  }

  /** Replaces any current popup and selects the first row. `owner` can be the focused textbox. */
  show(anchor: HTMLElement, choices: readonly Choice<T>[], onPick: (value: T) => void, options?: { empty?: string; owner?: HTMLElement }): void {
    this.close();
    const doc = anchor.ownerDocument;
    const el = doc.createElement("div");
    el.className = "choice-menu";
    el.id = this.id;
    el.tabIndex = -1; // Overflow containers can be tabbable by default.
    const rows = choices.map(({ label, detail, value }, i) => {
      const row = doc.createElement("div");
      row.className = "choice-menu-option";
      row.id = `${this.id}-option-${i}`;
      row.setAttribute("role", "option");
      row.setAttribute("aria-selected", "false");
      const name = doc.createElement("span");
      name.className = "choice-menu-label";
      name.textContent = label;
      row.append(name);
      if (detail) {
        const description = doc.createElement("span");
        description.className = "choice-menu-detail";
        description.textContent = detail;
        row.append(" ", description);
      }
      el.append(row);
      return { el: row, value };
    });
    if (rows.length) {
      el.setAttribute("role", "listbox");
      el.setAttribute("aria-label", this.label);
    } else {
      // A status message, not a selectable (or focusable) option.
      el.setAttribute("role", "status");
      const empty = doc.createElement("div");
      empty.className = "choice-menu-empty";
      empty.textContent = options?.empty ?? "No matches";
      el.append(empty);
    }
    el.addEventListener("mousedown", (e) => e.preventDefault()); // Keep the input and caret focused.
    el.addEventListener("click", (e) => {
      const menu = this.menu;
      if (!menu || menu.el !== el || e.button !== 0 || e.defaultPrevented) return;
      const row = menu.rows.find((r) => r.el.contains(e.target as Node | null));
      if (row) this.pick(menu, row.value);
    });
    doc.body.append(el); // A fixed overlay is not clipped by the anchor's ancestors.
    const owner = rows.length ? options?.owner : undefined;
    const menu: Menu<T> = { el, anchor, rows, selected: -1, onPick, owner, restoreOwner: owner && linkOwner(owner, el.id) };
    this.menu = menu;
    this.position();
    if (rows.length) this.select(menu, 0);
  }

  close(): void {
    const menu = this.menu;
    if (!menu) return;
    this.menu = null;
    menu.restoreOwner?.();
    menu.el.remove();
  }

  /** True for the popup itself and its contents, not the anchor or textbox. */
  has(target: EventTarget | null): boolean {
    const el = this.menu?.el;
    const NodeClass = el?.ownerDocument.defaultView?.Node;
    return !!el && !!NodeClass && target instanceof NodeClass && el.contains(target);
  }

  /** True if consumed; the caller should prevent default and skip further key handling. */
  handle(e: KeyboardEvent): boolean {
    const menu = this.menu;
    if (!menu) return false;
    const action = choiceMenuAction(e);
    if (!action) return false;
    if (action === "dismiss") this.close();
    else if (action === "accept") {
      const row = menu.rows[menu.selected];
      if (row) this.pick(menu, row.value);
    } else if (menu.rows.length) {
      const n = menu.rows.length;
      this.select(menu, (menu.selected + (action === "next" ? 1 : -1) + n) % n);
    }
    return true;
  }

  /** Call after moving the anchor or changing the viewport. */
  position(): void {
    const menu = this.menu;
    if (!menu) return;
    const { el, anchor } = menu;
    const doc = el.ownerDocument;
    const viewport = doc.defaultView?.visualViewport;
    const width = viewport?.width ?? doc.documentElement.clientWidth;
    const height = viewport?.height ?? doc.documentElement.clientHeight;
    const inset = Math.min(4, width / 4, height / 4);
    const left = (viewport?.offsetLeft ?? 0) + inset;
    const top = (viewport?.offsetTop ?? 0) + inset;
    const right = left + width - 2 * inset;
    const bottom = top + height - 2 * inset;
    const rect = anchor.getBoundingClientRect();
    const aboveEnd = clamp(rect.top - 2, top, bottom);
    const belowStart = clamp(rect.bottom + 2, top, bottom);
    const above = aboveEnd - top;
    const below = bottom - belowStart;
    const oldHeight = el.clientHeight;
    const rowHeight = el.firstElementChild?.getBoundingClientRect().height ?? 0;
    // If the anchor fills the viewport, allow overlap rather than showing an unusable sliver.
    const room = Math.min(bottom - top, Math.max(above, below, rowHeight + 2));
    el.style.setProperty("--choice-menu-width", `${right - left}px`);
    el.style.setProperty("--choice-menu-height", `${room}px`);
    const size = el.getBoundingClientRect();
    const y = size.height <= below || below >= above ? belowStart : aboveEnd - size.height;
    el.style.left = `${clamp(rect.left, left, right - size.width)}px`;
    el.style.top = `${clamp(y, top, bottom - size.height)}px`;
    if (el.clientHeight < oldHeight) this.reveal(menu);
  }

  private select(menu: Menu<T>, i: number): void {
    menu.rows[menu.selected]?.el.setAttribute("aria-selected", "false");
    menu.selected = i;
    const row = menu.rows[i].el;
    row.setAttribute("aria-selected", "true");
    this.reveal(menu);
    menu.owner?.setAttribute("aria-activedescendant", row.id);
  }

  private reveal(menu: Menu<T>): void {
    const row = menu.rows[menu.selected]?.el;
    if (!row) return;
    // Scroll only this list, not the page or the input that still has focus.
    const { el } = menu;
    const top = el.getBoundingClientRect().top + el.clientTop;
    const bottom = top + el.clientHeight;
    const rect = row.getBoundingClientRect();
    if (rect.top < top) el.scrollTop += rect.top - top;
    else if (rect.bottom > bottom) el.scrollTop += rect.bottom - bottom;
  }

  private pick(menu: Menu<T>, value: T): void {
    this.close();
    menu.onPick(value); // The callback may open a new menu.
  }
}
