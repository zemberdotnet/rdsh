// Text onto a character grid: wrapping, markdown, code highlighting and tables, and transcript
// entries to rows. Pure functions, so tests run them without a browser.

import { COLLAPSED_REASONING_LINES, type Entry } from "./session.ts";

/** A run of text with CSS classes. */
export type Seg = [text: string, cls: string];

export interface Line {
  segs: Seg[];
  /** Background class for the whole row. */
  bg?: string;
  /** Gutter text (2 columns) and its class. */
  g?: Seg;
  /** A row reserved for a diagram: which one and which of its rows. */
  dia?: Diagram;
  diaRow?: number;
}

export interface Row extends Line {
  g: Seg;
  e: Entry;
  /** Plain text of `segs`, for motions and search. */
  text: string;
}

/** A D3 diagram from a ```d3 block: its script runs in a sandboxed frame. */
export interface Diagram {
  key: string;
  script: string;
  rows: number;
  title: string;
}

// Quarter circles: in DejaVu Sans Mono, so one cell wide (braille is not in the font).
export const SPIN = ["◴", "◷", "◶", "◵"];

export const segText = (segs: Seg[]) => segs.map((s) => s[0]).join("");
export const fit = (s: string, n: number) => ([...s].length <= n ? s : n <= 0 ? "" : [...s].slice(0, n - 1).join("") + "…");

/** Inline markdown: `code`, **bold**, *italic*, [links](url). */
export function inline(text: string, base = ""): Seg[] {
  const out: Seg[] = [];
  const re = /(`[^`]+`)|(\*\*[^*]+\*\*)|(\*[^*\s][^*]*\*)|(\[[^\]]+\]\([^)]+\))/g;
  let last = 0;
  let m: RegExpExecArray | null;
  const add = (t: string, c: string) => { if (t) out.push([t, `${base} ${c}`.trim()]); };
  while ((m = re.exec(text))) {
    add(text.slice(last, m.index), "");
    if (m[1]) add(m[1].slice(1, -1), "ic");
    // Bold and italic text can hold code and links (`**`path`**`): parse inside them too.
    else if (m[2]) out.push(...inline(m[2].slice(2, -2), `${base} b`.trim()));
    else if (m[3]) out.push(...inline(m[3].slice(1, -1), `${base} i`.trim()));
    else add(m[4].replace(/^\[([^\]]+)\].*$/, "$1"), "link");
    last = re.lastIndex;
  }
  add(text.slice(last), "");
  return out;
}

/**
 * Greedy word wrap of styled segments into lines of at most `W` columns.
 *
 * @param first prefix of the first line (a bullet), styled `pcls`.
 * @param rest prefix of the following lines (the hanging indent).
 */
export function wrap(segs: Seg[], W: number, first = "", rest = " ".repeat(first.length), pcls = "dim", bg?: string): Line[] {
  const toks: Seg[] = [];
  for (const [t, c] of segs) for (const p of t.split(/(\s+)/)) if (p) toks.push([p, c]);
  const lines: Line[] = [];
  let cur: Seg[] = first ? [[first, pcls]] : [];
  let w = first.length;
  let base = w;
  const push = () => {
    while (cur.length > (base ? 1 : 0) && /^\s+$/.test(cur[cur.length - 1][0])) cur.pop();
    lines.push(bg ? { segs: cur, bg } : { segs: cur });
    cur = rest ? [[rest, pcls]] : [];
    w = base = rest.length;
  };
  for (let [t, c] of toks) {
    if (/^\s+$/.test(t)) {
      if (w === base && lines.length) continue;
      if (w + 1 > W) { push(); continue; }
      cur.push([" ", c]);
      w += 1;
      continue;
    }
    const room = Math.max(1, W - base);
    while (t.length > room) {
      if (w > base) push();
      cur.push([t.slice(0, room), c]);
      t = t.slice(room);
      w = W;
      push();
    }
    if (!t) continue;
    if (w + t.length > W) push();
    cur.push([t, c]);
    w += t.length;
  }
  if (cur.length > (rest && lines.length ? 1 : 0) || !lines.length) push();
  return lines;
}

const KW = new Set(("fn let mut pub struct enum impl use mod match if else for while loop return async await const static self Self crate " +
  "in as where move ref type trait dyn function var new of true false null undefined None Some Ok Err def class import from export").split(" "));

/** Small keyword/string/number/comment highlighter for fenced code. */
export function highlight(code: string, lang: string): Seg[] {
  const hash = /^(sh|bash|fish|zsh|toml|py|python|yaml|yml|nix)$/.test(lang);
  const re = hash
    ? /(#.*$)|("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*')|(\b\d[\d_.]*\b)|([A-Za-z_]\w*)/g
    : /(\/\/.*$)|("(?:[^"\\]|\\.)*"|`(?:[^`\\]|\\.)*`)|(\b\d[\d_.]*\b)|([A-Za-z_]\w*)/g;
  const out: Seg[] = [];
  let last = 0;
  let m: RegExpExecArray | null;
  while ((m = re.exec(code))) {
    if (m.index > last) out.push([code.slice(last, m.index), ""]);
    const cls = m[1] ? "com" : m[2] ? "str" : m[3] ? "num" : KW.has(m[4]) ? "kw" : /^[A-Z]/.test(m[4]) ? "ty" : "";
    out.push([m[0], cls]);
    last = re.lastIndex;
  }
  if (last < code.length) out.push([code.slice(last), ""]);
  return out;
}

/** Splits `s` into pieces of at most `n` characters (at least one piece). */
export function chunks(s: string, n: number): string[] {
  if (!s) return [""];
  const out: string[] = [];
  for (let i = 0; i < s.length; i += n) out.push(s.slice(i, i + n));
  return out;
}

export function table(lines: string[], W: number): Line[] {
  const cells = lines
    .filter((l) => !/^\s*\|?\s*:?-{2,}/.test(l))
    .map((l) => l.trim().replace(/^\||\|$/g, "").split("|").map((c) => inline(c.trim())));
  const n = Math.max(...cells.map((r) => r.length));
  const widths = Array.from({ length: n }, (_, i) => Math.max(1, ...cells.map((r) => (r[i] ? segText(r[i]).length : 0))));
  while (widths.reduce((a, b) => a + b, 0) + 3 * n + 1 > W && Math.max(...widths) > 4) widths[widths.indexOf(Math.max(...widths))]--;
  const bar = (l: string, m: string, r: string): Line => ({ segs: [[l + widths.map((w) => "─".repeat(w + 2)).join(m) + r, "dim"]] });
  const out: Line[] = [bar("┌", "┬", "┐")];
  cells.forEach((r, ri) => {
    const segs: Seg[] = [["│", "dim"]];
    widths.forEach((w, i) => {
      const c = r[i] ?? [];
      const txt = segText(c);
      segs.push([" ", ""]);
      if (txt.length > w) segs.push([fit(txt, w), ri === 0 ? "b" : c[0]?.[1] ?? ""]);
      else {
        for (const s of c) segs.push([s[0], ri === 0 ? `${s[1]} b`.trim() : s[1]]);
        segs.push([" ".repeat(w - txt.length), ""]);
      }
      segs.push([" ", ""], ["│", "dim"]);
    });
    out.push({ segs });
    if (ri === 0 && cells.length > 1) out.push(bar("├", "┼", "┤"));
  });
  out.push(bar("└", "┴", "┘"));
  return out;
}

const DEFAULT_DIAGRAM_ROWS = 18;

/**
 * Markdown to lines. A closed ```d3 block becomes a diagram (`// rows: N` on its first line
 * sets the height, `// title: …` the caption); while the block is still streaming, its source
 * shows as code.
 */
export function markdown(text: string, W: number, key = ""): Line[] {
  const L = text.split("\n");
  const out: Line[] = [];
  let diagrams = 0;
  for (let i = 0; i < L.length;) {
    const l = L[i];
    const fence = l.match(/^(\s*)(`{3,}|~{3,})\s*([\w+-]*)/);
    if (fence) {
      const close = fence[2];
      const lang = fence[3];
      const code: string[] = [];
      for (i++; i < L.length && !L[i].trimStart().startsWith(close); i++) code.push(L[i]);
      const closed = i < L.length;
      i++;
      if (lang === "d3" && closed) {
        const src = code.join("\n");
        const rows = Number(src.match(/\/\/\s*rows:\s*(\d+)/)?.[1] ?? DEFAULT_DIAGRAM_ROWS);
        const title = src.match(/\/\/\s*title:\s*(.+)/)?.[1]?.trim() ?? "diagram";
        const d: Diagram = { key: `${key}#${diagrams++}`, script: src, rows: Math.max(4, Math.min(60, rows)), title };
        out.push({ segs: [["◆ ", "purple"], [fit(title, W - 40), "b purple"], ["   Enter to interact · click to pick", "dim"]] });
        for (let r = 0; r < d.rows; r++) out.push({ segs: [], dia: d, diaRow: r });
        continue;
      }
      out.push({ segs: [[` ${lang || "code"} `, "lang"]], bg: "code" });
      for (const c of code) for (const p of chunks(c, Math.max(1, W - 1))) out.push({ segs: [[" ", ""], ...highlight(p, lang)], bg: "code" });
      continue;
    }
    if (/^\s*\|/.test(l)) {
      const t: string[] = [];
      while (i < L.length && /^\s*\|/.test(L[i])) t.push(L[i++]);
      out.push(...table(t, W));
      continue;
    }
    let m: RegExpMatchArray | null;
    if ((m = l.match(/^(#{1,6})\s+(.*)/))) {
      out.push(...wrap(inline(m[2], `h${Math.min(m[1].length, 3)}`), W));
      i++;
      continue;
    }
    if (/^\s*([-*_])(\s*\1){2,}\s*$/.test(l)) {
      out.push({ segs: [["─".repeat(Math.min(W, 40)), "dim"]] });
      i++;
      continue;
    }
    if ((m = l.match(/^(\s*)([-*+]|\d+[.)])\s+(.*)/))) {
      const numbered = /\d/.test(m[2]);
      const bullet = numbered ? `${m[2]} ` : "• ";
      let body = m[3];
      for (i++; i < L.length && /^\s{2,}\S/.test(L[i]) && !/^\s*([-*+]|\d+[.)])\s/.test(L[i]); i++) body += " " + L[i].trim();
      out.push(...wrap(inline(body), W, m[1] + bullet, " ".repeat(m[1].length + bullet.length), numbered ? "aqua" : "orange"));
      continue;
    }
    if ((m = l.match(/^>\s?(.*)/))) {
      out.push(...wrap(inline(m[1], "fgdim i"), W, "▌ ", "▌ ", "dim"));
      i++;
      continue;
    }
    if (!l.trim()) {
      // One blank row however many blank lines.
      if (out.length && out[out.length - 1].segs.length) out.push({ segs: [] });
      i++;
      continue;
    }
    let para = l.trim();
    for (i++; i < L.length && L[i].trim() && !/^(\s*(```|~~~)|#{1,6}\s|\s*[-*+]\s|\s*\d+[.)]\s|\s*\||>)/.test(L[i]); i++) para += " " + L[i].trim();
    out.push(...wrap(inline(para), W));
  }
  while (out.length && !out[out.length - 1].segs.length && !out[out.length - 1].dia) out.pop();
  return out;
}

const cache = new WeakMap<Entry, { v: number; W: number; spin: number; lines: Line[] }>();

/** Rows of one entry (cached until the entry or the width changes), ending with a spacer. */
export function entryLines(e: Entry, W: number, spin: number): Line[] {
  const running = e.tool?.status === "run";
  const c = cache.get(e);
  if (c && c.v === e.v && c.W === W && (!running || c.spin === spin)) return c.lines;
  const lines = build(e, W, spin);
  cache.set(e, { v: e.v, W, spin, lines });
  return lines;
}

function build(e: Entry, W: number, spin: number): Line[] {
  let out: Line[] = [];
  switch (e.k) {
    case "user":
      e.text.split("\n").forEach((l, i) =>
        wrap([[l, ""]], W, "", "", "", "user").forEach((x, j) => out.push({ ...x, g: [i === 0 && j === 0 ? "› " : "  ", "ug user"] })));
      break;
    case "assistant":
      out = markdown(e.text, W, String(e.id));
      if (out[0]) out[0].g = ["● ", ""];
      break;
    case "reasoning": {
      const ls = e.text.split("\n");
      if (e.collapsed && !e.streaming) out.push({ g: ["∴ ", "dim"], segs: [[`thought for ${ls.length} lines (Tab to expand)`, "dim i"]] });
      else {
        const skip = e.streaming ? Math.max(0, ls.length - COLLAPSED_REASONING_LINES) : 0;
        ls.slice(skip).forEach((l, i) => wrap([[l, "dim i"]], W).forEach((x, j) => out.push({ ...x, g: [i === 0 && j === 0 ? "∴ " : "  ", "dim"] })));
      }
      break;
    }
    case "tool": {
      const t = e.tool!;
      const icon: Seg = t.status === "run" ? [SPIN[spin % SPIN.length] + " ", "run"] : t.status === "ok" ? ["✓ ", "ok"] : ["✗ ", "bad"];
      const dur = t.dur == null ? "" : t.dur >= 1000 ? `  (${(t.dur / 1000).toFixed(1)}s)` : `  (${t.dur}ms)`;
      out.push({ g: icon, segs: [[t.name + " ", "b"], [fit(t.summary, W - t.name.length - dur.length - 1), "b"], [dur, "dim"]] });
      for (const c of t.children) out.push({ segs: [["  " + fit(c, W - 2), "dim"]] });
      const ls = t.output ? t.output.split("\n") : [];
      const shown = e.collapsed ? Math.min(COLLAPSED_OUTPUT, ls.length) : ls.length;
      for (const l of ls.slice(0, shown)) for (const p of chunks(l, Math.max(1, W - 2))) out.push({ g: ["│ ", "dim"], segs: [[p, t.status === "bad" ? "err" : "fgdim"]] });
      if (shown < ls.length) out.push({ g: ["│ ", "dim"], segs: [[`… +${ls.length - shown} lines (Tab to expand)`, "dim"]] });
      break;
    }
    case "info":
      e.text.split("\n").forEach((l) => out.push({ g: ["· ", "dim"], segs: [[l, "dim"]] }));
      break;
    case "error":
      e.text.split("\n").forEach((l) => out.push({ g: ["✗ ", "bad"], segs: [[l, "err"]] }));
      break;
  }
  if (!out.length) out.push({ segs: [] });
  out.push({ segs: [] });
  return out;
}

const COLLAPSED_OUTPUT = 4;

/** All rows of a transcript, without the spacer after the last entry. */
export function layout(entries: Entry[], W: number, spin: number): Row[] {
  const rows: Row[] = [];
  for (const e of entries) {
    for (const l of entryLines(e, W, spin)) rows.push({ ...l, g: l.g ?? ["  ", ""], e, text: segText(l.segs) });
  }
  if (rows.length) rows.pop();
  return rows;
}
