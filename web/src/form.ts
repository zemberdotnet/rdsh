// The form panel: an `ask` call's forms, one page at a time, docked above the prompt.
//
// Real inputs (so typing, pasting and IME work), styled to the grid. Keys: Tab / Shift-Tab
// move between fields, Enter in a one-line field goes to the next, Ctrl-Enter goes to the next
// form or submits, Ctrl-p goes back, Esc returns to the chat (the form stays until answered).

import type { Answer, Ask, Field, Value } from "./proto.ts";

const esc = (s: string) => s.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]!);

/** Starting values from the fields' defaults. */
export function initialValues(ask: Ask): Record<string, Value | undefined> {
  const v: Record<string, Value | undefined> = {};
  for (const f of ask.forms.flatMap((f) => f.fields)) {
    if (f.type === "choice") v[f.id] = f.multiple ? [...(f.default ?? [])] : f.default?.[0];
    else v[f.id] = f.default;
  }
  return v;
}

/** Why `value` cannot be sent for `field`, if it cannot. */
export function problem(field: Field, value: Value | undefined): string | null {
  const empty = value === undefined || value === "" || (Array.isArray(value) && value.length === 0);
  if (empty) return field.required === false ? null : "needs an answer";
  if (field.type === "number") {
    const n = Number(value);
    if (!Number.isFinite(n)) return "needs a number";
    if (field.min != null && n < field.min) return `at least ${field.min}`;
    if (field.max != null && n > field.max) return `at most ${field.max}`;
  }
  return null;
}

/** The answer to send: values of the filled-in fields, by id. */
export function collect(ask: Ask, values: Record<string, Value | undefined>): Answer {
  const out: Record<string, Value> = {};
  for (const f of ask.forms.flatMap((f) => f.fields)) {
    let v = values[f.id];
    if (typeof v === "string") v = v.trim();
    if (v === undefined || v === "" || (Array.isArray(v) && !v.length)) continue;
    out[f.id] = f.type === "number" ? Number(v) : v;
  }
  return { type: "form", values: out };
}

export class FormPanel {
  private el: HTMLElement;
  private id: string | null = null;
  private ask: Ask | null = null;
  private page = 0;
  private values: Record<string, Value | undefined> = {};
  /** "Other" text of choice fields, by id. */
  private other: Record<string, string> = {};
  private sent = false;
  /** The user answers; the page sends it. */
  onAnswer: (a: Answer) => void = () => {};
  /** Esc, or "reply in words": back to the chat (`words` focuses the prompt). */
  onLeave: (words: boolean) => void = () => {};
  /** A message for the status line. */
  onFlash: (s: string, err: boolean) => void = () => {};

  constructor(el: HTMLElement) {
    this.el = el;
    el.addEventListener("keydown", (e) => this.key(e));
    el.addEventListener("input", (e) => this.read(e.target as HTMLElement));
    el.addEventListener("change", (e) => this.read(e.target as HTMLElement));
    el.addEventListener("click", (e) => {
      const b = (e.target as HTMLElement).closest("button[data-act]") as HTMLButtonElement | null;
      if (b) this.act(b.dataset.act!);
    });
  }

  /** Shows `q`'s forms (or hides the panel). Leaves an open form alone, inputs and all. */
  sync(q: { callId: string; ask: Ask } | null) {
    if (!q) {
      this.id = this.ask = null;
      this.el.hidden = true;
      this.el.innerHTML = "";
      return;
    }
    if (q.callId === this.id) return;
    this.id = q.callId;
    this.ask = q.ask;
    this.page = 0;
    this.sent = false;
    this.values = initialValues(q.ask);
    this.other = {};
    this.el.hidden = false;
    this.render();
  }

  get open(): boolean {
    return !this.el.hidden;
  }

  has(node: Node | null): boolean {
    return !!node && this.el.contains(node);
  }

  /** Puts the cursor in the first field of this page that still needs an answer. */
  focus() {
    const f = this.ask?.forms[this.page];
    if (!f) return;
    const first = f.fields.find((x) => problem(x, this.values[x.id])) ?? f.fields[0];
    const input = this.el.querySelector(`[data-field="${CSS.escape(first.id)}"] input, [data-field="${CSS.escape(first.id)}"] textarea`) as HTMLElement | null;
    input?.focus();
  }

  private render() {
    const ask = this.ask!;
    const form = ask.forms[this.page];
    const n = ask.forms.length;
    const last = this.page === n - 1;
    const head = [ask.title, form.title].filter(Boolean).join(" · ") || "The agent asks";
    let html = `<div class="fhead"><span class="b">? ${esc(head)}</span>${n > 1 ? `<span class="dim">form ${this.page + 1} of ${n}</span>` : ""}</div>`;
    if (form.description) html += `<div class="fdesc">${esc(form.description)}</div>`;
    for (const f of form.fields) html += this.field(f);
    html += `<div class="fbar">`;
    if (this.page > 0) html += `<button data-act="back">◂ back</button>`;
    html += `<button data-act="next" class="primary">${last ? "submit" : "next ▸"}</button>`;
    html += `<button data-act="words">reply in words</button><button data-act="dismiss">dismiss</button>`;
    html += `<span class="dim">Tab fields · Enter next · Ctrl-Enter ${last ? "submit" : "next form"} · Esc chat</span></div>`;
    this.el.innerHTML = html;
  }

  private field(f: Field): string {
    const v = this.values[f.id];
    const req = f.required === false ? `<span class="dim"> (optional)</span>` : "";
    let body = "";
    const name = `f-${esc(f.id)}`;
    switch (f.type) {
      case "text":
        body = f.multiline
          ? `<textarea rows="4" placeholder="${esc(f.placeholder ?? "")}">${esc(String(v ?? ""))}</textarea>`
          : `<input type="text" placeholder="${esc(f.placeholder ?? "")}" value="${esc(String(v ?? ""))}">`;
        break;
      case "number":
        body = `<input type="number" ${f.min != null ? `min="${f.min}"` : ""} ${f.max != null ? `max="${f.max}"` : ""} value="${v ?? ""}">`;
        break;
      case "confirm":
        body = [["yes", true], ["no", false]].map(([l, b]) =>
          `<label class="opt"><input type="radio" name="${name}" value="${l}" ${v === b ? "checked" : ""}> ${l}</label>`).join("");
        break;
      case "choice": {
        const kind = f.multiple ? "checkbox" : "radio";
        const chosen = (x: string) => (Array.isArray(v) ? v.includes(x) : v === x);
        body = f.options.map((o) =>
          `<label class="opt"><input type="${kind}" name="${name}" value="${esc(o.label)}" ${chosen(o.label) ? "checked" : ""}> ${esc(o.label)}${o.description ? `<span class="dim"> — ${esc(o.description)}</span>` : ""}</label>`).join("");
        if (f.other) {
          const t = this.other[f.id] ?? "";
          body += `<label class="opt"><input type="${kind}" name="${name}" value="\u0000other" ${t && chosen(t) ? "checked" : ""}> other: <input type="text" class="other" value="${esc(t)}"></label>`;
        }
        break;
      }
    }
    return `<div class="field" data-field="${esc(f.id)}"><div class="label">${esc(f.label)}${req}</div>${body}${f.help ? `<div class="help dim">${esc(f.help)}</div>` : ""}<div class="err"></div></div>`;
  }

  /** Reads an input back into `values`. */
  private read(t: HTMLElement) {
    const box = t.closest(".field") as HTMLElement | null;
    const f = this.fieldById(box?.dataset.field);
    if (!box || !f) return;
    if (f.type === "text" || f.type === "number") {
      this.values[f.id] = (t as HTMLInputElement).value;
    } else if (f.type === "confirm") {
      const c = box.querySelector("input:checked") as HTMLInputElement | null;
      this.values[f.id] = c ? c.value === "yes" : undefined;
    } else {
      const otherBox = box.querySelector("input.other") as HTMLInputElement | null;
      if (otherBox) this.other[f.id] = otherBox.value.trim();
      // Typing in "other" selects it.
      if (t === otherBox && otherBox.value.trim()) {
        const r = box.querySelector('input[value="\u0000other"]') as HTMLInputElement | null;
        if (r) r.checked = true;
      }
      const picked = [...box.querySelectorAll("input:checked")].map((i) =>
        (i as HTMLInputElement).value === "\u0000other" ? this.other[f.id] ?? "" : (i as HTMLInputElement).value).filter((x) => x);
      this.values[f.id] = f.multiple ? picked : picked[0];
    }
    (box.querySelector(".err") as HTMLElement).textContent = "";
  }

  private fieldById(id: string | undefined): Field | undefined {
    return this.ask?.forms.flatMap((f) => f.fields).find((f) => f.id === id);
  }

  /** Marks this page's problems; true when there are none. */
  private check(): boolean {
    let ok = true;
    for (const f of this.ask!.forms[this.page].fields) {
      const p = problem(f, this.values[f.id]);
      const box = this.el.querySelector(`[data-field="${CSS.escape(f.id)}"] .err`) as HTMLElement | null;
      if (box) box.textContent = p ? `✗ ${p}` : "";
      if (p && ok) {
        ok = false;
        (this.el.querySelector(`[data-field="${CSS.escape(f.id)}"] input, [data-field="${CSS.escape(f.id)}"] textarea`) as HTMLElement | null)?.focus();
      }
    }
    return ok;
  }

  private act(what: string) {
    if (!this.ask || this.sent) return;
    switch (what) {
      case "back":
        if (this.page > 0) {
          this.page--;
          this.render();
          this.focus();
        }
        break;
      case "next":
        if (!this.check()) return this.onFlash("some answers are missing", true);
        if (this.page < this.ask.forms.length - 1) {
          this.page++;
          this.render();
          this.focus();
        } else this.send(collect(this.ask, this.values));
        break;
      case "dismiss":
        this.send({ type: "dismissed" });
        break;
      case "words":
        this.onLeave(true);
        break;
    }
  }

  private send(a: Answer) {
    this.sent = true;
    this.el.classList.add("sent");
    this.onAnswer(a);
  }

  private key(e: KeyboardEvent) {
    const t = e.target as HTMLElement;
    if (e.key === "Escape") {
      e.preventDefault();
      t.blur();
      this.onLeave(false);
    } else if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      this.act("next");
    } else if (e.key === "p" && e.ctrlKey) {
      e.preventDefault();
      this.act("back");
    } else if (e.key === "Enter" && !(t instanceof HTMLTextAreaElement) && !(t instanceof HTMLButtonElement)) {
      // Enter in a one-line field or on an option moves on; on the last field it submits.
      e.preventDefault();
      const all = [...this.el.querySelectorAll("input, textarea, button.primary")] as HTMLElement[];
      const next = all.slice(all.indexOf(t) + 1).find((x) => x.closest(".field") !== t.closest(".field") || x instanceof HTMLButtonElement);
      if (next instanceof HTMLButtonElement) this.act("next");
      else next?.focus();
    }
    // Everything else (typing, Tab, arrows on options, Space) is the browser's.
    e.stopPropagation();
  }
}
