// Draws the app onto the grid: transcript rows, rail, separator, status line, help.

import type { App } from "./app.ts";
import { frameUrl, Frames } from "./diagram.ts";
import { FormPanel } from "./form.ts";
import { fit, SPIN, type Seg } from "./layout.ts";
import { matches } from "./motion.ts";

export const GUTTER = 2;
export const RAIL_W = 28;

const $ = (s: string) => document.querySelector(s) as HTMLElement;
/** Queued messages listed above the prompt; more collapse into a "+N more" row. */
const QUEUE_ROWS = 4;
const esc = (s: string) => s.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]!);

export class View {
  cw = 8.4;
  lh = 18;
  frames: Frames;
  form: FormPanel;
  /** Called when a new question opens the form panel. */
  onQuestion: () => void = () => {};
  main = $("#main");
  private app: App;
  constructor(app: App) {
    this.app = app;
    this.frames = new Frames($("#overlays"), () => frameUrl(location));
    this.form = new FormPanel($("#form"));
  }

  /**
   * Sizes the grid like a terminal: a cell and a row are whole numbers of device pixels, so
   * every glyph starts on a pixel instead of being smeared across two. The font's own advance
   * (8.43px for DejaVu Sans Mono at 14px) is rounded, and letter spacing makes up the rest.
   * Runs again on resize, which is also how browser zoom (a new device pixel ratio) arrives.
   */
  measure() {
    const root = document.documentElement.style;
    const dpr = window.devicePixelRatio || 1;
    root.setProperty("--ls", "0px");
    const advance = this.width("M");
    const cell = Math.max(1, Math.round(advance * dpr));
    root.setProperty("--ls", `${(cell - advance * dpr) / dpr}px`);
    this.cw = cell / dpr;
    this.lh = Math.round(parseFloat(getComputedStyle(document.body).fontSize) * 1.3 * dpr) / dpr;
    root.setProperty("--lh", `${this.lh}px`);
    $("#rail").style.width = `${RAIL_W * this.cw + 1}px`;
  }

  /** Width of one `ch` as laid out now (letter spacing included). */
  private width(ch: string): number {
    const s = document.createElement("span");
    s.textContent = ch.repeat(200);
    s.style.cssText = "position:absolute;visibility:hidden;white-space:pre";
    document.body.appendChild(s);
    const w = s.getBoundingClientRect().width / 200;
    s.remove();
    return w;
  }

  cols() {
    return Math.max(20, Math.floor(this.main.clientWidth / this.cw));
  }

  visibleRows() {
    return Math.max(3, Math.floor(this.main.clientHeight / this.lh));
  }

  draw() {
    const a = this.app;
    a.refreshRail();
    $("#rail").classList.toggle("hidden", !a.railOpen);
    // The form belongs to the session on screen; previews and traces show none.
    const q = a.previewing() || a.cur.readOnly ? null : a.cur.question;
    const wasOpen = this.form.open;
    this.form.sync(q);
    if (q && !wasOpen) this.onQuestion();
    a.relayout(this.cols());
    this.drawTranscript();
    this.drawRail();
    this.drawQueue();
    this.drawSep();
    this.drawPrompt();
    this.drawStatus();
    this.drawHelp();
    const v = a.cur;
    const title = a.railRows.find((r) => r.host === v.host && r.id === v.id)?.title;
    document.title = title ? `${fit(title, 40)} · rdsh` : "rdsh";
  }

  /** Keeps the cursor inside the rows and the rows around it on screen. */
  clamp() {
    const a = this.app;
    const c = a.cursor();
    const rows = a.rows;
    const vis = this.visibleRows();
    if (c.follow) c.row = Math.max(0, rows.length - 1);
    c.row = Math.max(0, Math.min(c.row, rows.length - 1));
    const len = rows[c.row]?.text.length ?? 0;
    c.col = Math.max(0, Math.min(c.col, a.mode === "visual" ? len : Math.max(0, len - 1)));
    if (c.row < c.top) c.top = c.row;
    if (c.row >= c.top + vis) c.top = c.row - vis + 1;
    c.top = Math.max(0, Math.min(c.top, Math.max(0, rows.length - vis)));
    if (c.row < c.top) c.top = c.row;
  }

  private visual(i: number): { a: number; b: number; cls: string }[] {
    const app = this.app;
    const c = app.cursor();
    const v = app.vAnchor;
    if (!v) return [];
    if (app.mode === "vline") {
      return i >= Math.min(v.row, c.row) && i <= Math.max(v.row, c.row) ? [{ a: 0, b: 1e9, cls: "vis" }] : [];
    }
    if (app.mode === "visual") {
      let s = v;
      let e = { row: c.row, col: c.col };
      if (e.row < s.row || (e.row === s.row && e.col < s.col)) [s, e] = [e, s];
      if (i < s.row || i > e.row) return [];
      return [{ a: i === s.row ? s.col : 0, b: i === e.row ? e.col + 1 : 1e9, cls: "vis" }];
    }
    return [];
  }

  private drawTranscript() {
    const a = this.app;
    this.clamp();
    const c = a.cursor();
    const rows = a.rows;
    const vis = this.visibleRows();
    const q = a.mode === "search" ? a.minibuf : a.lastSearch;
    const from = Math.max(0, c.top - 4);
    const to = Math.min(rows.length, c.top + vis + 4);
    let html = "";
    for (let i = from; i < to; i++) {
      const r = rows[i];
      const ranges = [...this.visual(i), ...(q ? matches(r.text, q).map(([a, b]) => ({ a, b, cls: "match" })) : [])];
      const dead = r.dia && r.diaRow === 0 ? this.frames.dead.get(r.dia.key) : undefined;
      if (dead) {
        html += `<div class="row"><span>  </span><span class="bad">✗ diagram removed: ${esc(dead)}</span></div>`;
        continue;
      }
      html += `<div class="row${r.bg ? " " + r.bg : ""}"><span class="${r.g[1]}">${esc(r.g[0])}</span>${paint(r.segs, ranges)}</div>`;
    }
    const lines = $("#lines");
    lines.style.paddingTop = `${from * this.lh}px`;
    lines.innerHTML = html;
    const content = $("#content");
    // A plain offset, not a transform: a transformed layer can be drawn less sharply.
    content.style.top = `${-c.top * this.lh}px`;
    content.style.height = `${rows.length * this.lh}px`;

    // Diagram frames over their reserved rows.
    const live = new Set<string>();
    const width = this.main.clientWidth;
    rows.forEach((r, i) => {
      if (!r.dia || r.diaRow !== 0) return;
      const d = r.dia;
      if (i + d.rows < c.top - 20 || i > c.top + vis + 20) return; // far off screen: hidden
      live.add(d.key);
      const f = this.frames.get(d.key, d.script, d.title);
      if (!f) return;
      f.style.display = "";
      f.style.top = `${i * this.lh + 2}px`;
      f.style.left = `${GUTTER * this.cw}px`;
      f.style.width = `${Math.min(width - (GUTTER + 2) * this.cw, 1200)}px`;
      f.style.height = `${d.rows * this.lh - 4}px`;
      f.classList.toggle("active", a.diagram?.key === d.key);
    });
    this.frames.hideExcept(live);

    const cur = $("#cursor");
    const r = rows[c.row];
    cur.className = a.mode === "insert" || a.mode === "diagram" || a.mode === "form" || !r || r.dia ? "off" : a.railFocus ? "dim" : "";
    cur.style.top = `${c.row * this.lh}px`;
    cur.style.left = `${(GUTTER + c.col) * this.cw}px`;
    cur.style.width = `${this.cw}px`;
    // The character under the cursor, drawn inverted (no blend mode: it can blur the text).
    cur.textContent = r && !r.dia ? (r.g[0] + r.text)[GUTTER + c.col] ?? " " : " ";
  }

  private drawRail() {
    const a = this.app;
    const el = $("#rail");
    if (!a.railOpen) return;
    const head = `<div class="row"><span class="${a.railFocus ? "aqua" : "dim"} b"> SESSIONS</span>${a.railHidden ? `<span class="dim"> +${a.railHidden} old</span>` : ""}</div>`;
    const rows = a.railRows;
    const vis = Math.max(1, Math.floor(el.clientHeight / this.lh) - 1);
    const sel = a.railIndex();
    const top = Math.max(0, Math.min(sel - Math.floor(vis / 2), rows.length - vis));
    const spin = SPIN[a.spin % SPIN.length] + " ";
    let html = head;
    if (!rows.length) html += `<div class="row dim"> ${a.connected ? "no sessions here yet" : "connecting…"}</div>`;
    rows.slice(top, top + vis).forEach((r, k) => {
      const i = top + k;
      const selected = a.railFocus && i === sel;
      const current = !r.header && r.host === a.cur.host && r.id === a.cur.id;
      if (r.header) {
        const ok = a.hosts[r.host]?.connected;
        const label = fit(`${ok ? "◆" : "✗"} ${r.title}`, RAIL_W - 2);
        html += `<div class="row rhead ${selected ? "rsel" : ""}" data-i="${i}"> <span class="${ok ? "aqua" : "bad"}">${esc(label)}</span></div>`;
        return;
      }
      let pre: string, glyph: string, gc: string;
      if (r.depth === 0) {
        pre = " ";
        [glyph, gc] = current ? ["▸ ", "orange"] : r.live === "running" ? [spin, "run"] : ["  ", "dim"];
      } else {
        pre = "   ";
        [glyph, gc] = r.live === "running" ? [spin, "run"] : r.live === "done" ? ["● ", "ok"] : r.live === "failed" ? ["✗ ", "bad"] : ["· ", "dim"];
      }
      const label = fit(r.title.replace(/\s+/g, " "), RAIL_W - pre.length - glyph.length - 1);
      const base = (r.depth ? "fgdim" : "") + (current ? " b" : "");
      html += `<div class="row ${selected ? "rsel" : ""}" data-i="${i}">${pre}<span class="${gc}">${glyph}</span><span class="${selected ? "" : base}">${esc(label)}</span></div>`;
    });
    el.innerHTML = html;
  }

  /** Messages waiting for the running turn, oldest first, right above the prompt. */
  private drawQueue() {
    const a = this.app;
    const el = $("#queue");
    const v = a.shown();
    const queued = a.previewing() ? [] : v.queued;
    el.hidden = !queued.length;
    if (!queued.length) {
      el.innerHTML = "";
      return;
    }
    const n = Math.floor(el.clientWidth / this.cw) || 80;
    const shown = queued.length > QUEUE_ROWS ? QUEUE_ROWS - 1 : queued.length;
    const editing = v.queueEdit?.id;
    let html = "";
    queued.slice(0, shown).forEach((t, i) => {
      const first = t.text.split("\n").find((l) => l.trim())?.trim() ?? "";
      const more = t.text.trim().includes("\n") ? " …" : "";
      const head = ` ${t.id === editing ? "✎" : "○"} ${i + 1}. `;
      const body = fit(first, Math.max(1, n - head.length - more.length));
      html += `<div class="q${t.id === editing ? " edit" : ""}"><span class="purple">${esc(head)}</span>${esc(body)}<span class="dim">${more}</span></div>`;
    });
    if (queued.length > shown) html += `<div class="q dim">   +${queued.length - shown} more waiting</div>`;
    el.innerHTML = html;
  }

  private drawSep() {
    const a = this.app;
    const v = a.shown();
    const n = Math.floor($("#sep").clientWidth / this.cw);
    let left = "";
    let cls = "blue";
    const preview = a.previewing();
    if (v.activity) {
      const secs = Math.floor((Date.now() - v.turnStarted) / 1000);
      left = ` ${SPIN[a.spin % SPIN.length]} ${v.activity} · ${secs}s `;
      cls = "run";
    } else if (preview) left = v.readOnly ? " preview · Enter opens " : " preview · Enter switches ";
    const edit = preview ? null : v.queueEdit;
    let queue = v.queued.length ? ` ${v.queued.length} queued` : "";
    if (edit) {
      const hint = edit.submitted !== null ? "saving edit…" : a.mode === "insert" ? "editing · Enter saves · Esc discards" : "editing · i to resume";
      queue += `${queue ? " · " : " "}${hint}`;
    } else if (queue && !preview) {
      queue += a.mode === "insert" ? " · ↑ recall" : " · i then ↑ recall";
    }
    if (queue) queue += " ";
    if (!a.connected) {
      left = " disconnected from rdsh web — reconnecting… ";
      cls = "bad";
    }
    $("#sep").innerHTML = `<span class="dim">──</span>${queue ? `<span class="aqua">${esc(queue)}</span>` : ""}${left ? `<span class="${cls}">${esc(left)}</span>` : ""}<span class="dim">${"─".repeat(Math.max(0, n - 2 - left.length - queue.length))}</span>`;
  }

  drawPrompt() {
    const inp = $("#input") as HTMLTextAreaElement;
    const a = this.app;
    inp.classList.toggle("ro", a.previewing() || a.cur.readOnly);
    inp.placeholder = a.cur.readOnly ? "read-only trace" : "";
    inp.rows = Math.min(10, Math.max(1, inp.value.split("\n").length));
  }

  private drawStatus() {
    const a = this.app;
    const label = { normal: "NORMAL", insert: "INSERT", visual: "VISUAL", vline: "V-LINE", command: "COMMAND", search: "SEARCH", diagram: "DIAGRAM", form: "FORM" }[a.mode];
    // Classes, not style attributes: the page's CSP allows no inline styles.
    let left = `<span class="badge m-${a.mode}"> ${label} </span>`;
    if (a.mode === "command") left += ` :${esc(a.minibuf)}<span class="caret"> </span>`;
    else if (a.mode === "search") left += ` ${a.searchFwd ? "/" : "?"}${esc(a.minibuf)}<span class="caret"> </span>`;
    else {
      const asking = a.mode !== "form" && this.form.open;
      const what = a.railFocus ? "sessions" : a.mode === "diagram" ? "hjkl pan · +/- zoom · 0 reset · Esc leaves" : a.mode === "form" ? "answering" : asking ? "the agent asks: i answers the form · o replies in words" : "chat";
      left += `<span class="dim"> ${what} </span>`;
      if (a.flash) left += `<span class="${a.flashErr ? "err" : "run"}"> ${esc(a.flash)}</span>`;
    }
    const v = a.shown();
    const k = (n: number) => (n >= 1000 ? `${(n / 1000).toFixed(1)}k` : String(n));
    const pend = `${a.count ?? ""}${a.pending === "W" ? "^␣" : a.pending ?? ""}`;
    const host = a.hosts.length > 1 ? `${a.hosts[v.host]?.name ?? "?"} @ ` : "";
    const right = v.id
      ? `${v.length ? `len ${esc(v.length)} · ` : ""}<span class="e-${a.effort}">${esc(v.model || "?")}</span> · ↑${k(v.usage.prompt_tokens)} ↓${k(v.usage.completion_tokens)} · ${esc(host + v.id.slice(0, 15))} `
      : `<span class="e-${a.effort}">thinking ${a.effort}</span> `;
    $("#status .l").innerHTML = left;
    $("#status .r").innerHTML = `${pend ? `<span class="fgdim">${esc(pend)}</span>   ` : ""}${right}`;
  }

  private drawHelp() {
    const el = $("#help");
    el.classList.toggle("open", this.app.help);
    if (!this.app.help) return;
    const H = (t: string) => `<span class="h">${t}</span>\n`;
    const K = (k: string, d: string) => `  <span class="k">${esc(k.padEnd(28))}</span>${esc(d)}\n`;
    el.innerHTML =
      H("rdsh web") + `  <span class="dim">rdsh ${esc(this.app.version)} · the same keys as the TUI where the browser allows</span>\n\n` +
      H("Moving") + K("h j k l  w b e  0 ^ $", "counts work: 5j, 3w") + K("gg G 12G", "top, bottom, row") +
      K("Ctrl-d Ctrl-u Ctrl-f Ctrl-b", "half page, page") + K("Ctrl-e Ctrl-y  zz zt zb", "scroll") + K("H M L", "top, middle, bottom of the screen") +
      K("{ }", "previous / next entry") + "\n" +
      H("Transcript") + K("Tab  za  Enter", "fold or unfold tool output and reasoning") + K("/ ? n N", "search (smart case)") + K("gx", "open the link under the cursor in a new tab") +
      K("v V y yy", "select and copy") + K("Enter on a diagram", "interact: hjkl pan, +/- zoom, 0 reset, Esc leaves") +
      K("click in a diagram", "pick an item; it lands in the prompt as context") + "\n" +
      H("Sessions") + K("Ctrl-Space h / l", "focus the rail / back (the browser keeps Ctrl-w)") + K("j k 3j g G 5G", "move; the main view previews") +
      K("Enter, double-click", "switch to the session (resuming it); a host heading starts one") + K("zh", "show or hide subagents finished over 5 min ago") +
      K("y  S  r", "copy id, hide rail, refresh") + "\n" +
      H("Prompt") + K("i a", "type; Enter sends, Shift-Enter or Ctrl-j newline, Esc leaves") +
      K("↑ ↓ in an empty prompt", "recall queued messages first, or browse history if none") +
      K("Enter / Esc in queued edit", "save in place / discard changes") +
      K("Ctrl-Enter", "send now: every waiting message starts at once, as one") +
      K("Tab", "complete the path before the cursor; a menu offers several matches") +
      K("Shift-Tab", "thinking effort (the model name's color)") + K("Ctrl-c", "interrupt the running turn") + "\n" +
      H("Commands") + K(":new  :model [id]  :host user@box[:/dir]", "new session, model, add an SSH host") + K(":restart  :rail  :help", "restart the hub, toggle rail, this") +
      H("Questions") + K("i, when the agent asks", "answer its form: Tab fields, Enter next, Ctrl-Enter next form or submit") +
      K("o, or reply in words", "type an answer instead: your next message answers the form") + K("Esc", "back to the chat; the form waits") + "\n" +
      "\n" + H("Diagrams") + `  <span class="dim">Ask for a diagram: the model answers with a \`\`\`d3 block, drawn in a sandboxed frame.\n  It gets d3, svg, g (pan/zoom layer), W, H, C (palette), CH (char width) and pick(label).\n  "// rows: N" sets the height, "// title: ..." the caption.</span>\n\n` +
      `  <span class="dim">any key closes</span>`;
  }
}

/** HTML of styled segments with `ranges` (columns) painted over them. */
export function paint(segs: Seg[], ranges: { a: number; b: number; cls: string }[]): string {
  let html = "";
  let col = 0;
  for (const [t, c] of segs) {
    let i = 0;
    while (i < t.length) {
      const at = (x: number) => ranges.find((r) => col + x >= r.a && col + x < r.b);
      const r = at(i);
      let j = i + 1;
      while (j < t.length && at(j) === r) j++;
      const cls = `${c}${r ? " " + r.cls : ""}`.trim();
      const piece = esc(t.slice(i, j));
      html += cls ? `<span class="${cls}">${piece}</span>` : piece;
      i = j;
    }
    col += t.length;
  }
  return html;
}
