// Text onto a character grid: wrapping, markdown, code highlighting and tables, and transcript
// entries to rows. Pure functions, so tests run them without a browser.

import { COLLAPSED_REASONING_LINES, type Entry } from "./session.ts";
import { diffLines, mark as markRanges, pairRanges } from "./worddiff.ts";

/** A run of text with CSS classes and an optional HTTP(S) link target. */
export type Seg = [text: string, cls: string, href?: string];

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

/** The link at a text column, excluding the gutter. */
export function linkAt(line: Line | undefined, col: number): string | undefined {
  if (col < 0) return;
  for (const [text, , href] of line?.segs ?? []) {
    if (col < text.length) return href;
    col -= text.length;
  }
}

// Accept balanced brackets (e.g. IPv6) and parentheses (e.g. wiki paths) inside bare URLs.
const BARE_URL = /(?<bare>(?<![\w/:@.-])https?:\/\/(?:[^\s<>"'`()\[\]{}]|\([^()\s<>"'`]*\)|\[[^\s<>"'`\[\]]*\])+)/gi;
const INLINE = new RegExp([
  /(?<code>`[^`]+`)|(?<bold>\*\*[^*]+\*\*)|(?<italic>\*[^*\s][^*]*\*)/.source,
  /\[(?<label>[^\]\n]+)\]\(\s*(?:<(?<angle>[^<>\s]+)>|(?<dest>(?:[^\s<>()]|\([^()\s]*\))+))(?:\s+(?:"[^"]*"|'[^']*'|\([^()]*\)))?\s*\)/.source,
  /<(?<auto>https?:\/\/[^<>\s]+)>/.source,
  BARE_URL.source,
].join("|"), "gi");

function webUrl(dest: string): string | undefined {
  if (!/^https?:\/\//i.test(dest)) return;
  try { return new URL(dest).href; } catch { return; }
}

/**
 * Looks up path-like words on the session's host while laying out, so only files that exist
 * there become links (`something/else` stays text). The App keeps the answers per host and
 * directory and asks the hub about new words after each layout.
 */
export interface PathLinks {
  /** The file's absolute path; null when it is not a file; undefined while unknown (asked). */
  lookup(path: string): string | null | undefined;
  /** Bumped when answers arrive, so cached layouts that might hold new links are rebuilt. */
  epoch: number;
}

/** Set by `layout` for the duration of one call. */
let paths: PathLinks | null = null;

/** Same shape as the TUI's candidates (src/links.rs): a path, then up to two `:N` parts. */
const PATH = /[~\w./@+\-]+(?::\d+){0,2}/g;

/** Splits `src/a.rs:12:5` into the path and its line. */
export function splitLine(s: string): [string, number | undefined] {
  const [p, l] = s.split(":");
  const n = Number(l);
  return [p, l && Number.isInteger(n) && n > 0 ? n : undefined];
}

/** Worth asking the host about: has a `/` or an extension, and is not just dots and slashes. */
export function pathLike(p: string): boolean {
  return p.length >= 2 && (p.includes("/") || /\.[A-Za-z]\w*$/.test(p)) && !/^[./]+$/.test(p) && !/^\w+:\/\//.test(p);
}

export const fileHref = (abs: string, line?: number) => `rdsh-file:${abs}${line ? `#L${line}` : ""}`;

/** The file and line of a `fileHref` link, or null for any other link. */
export function parseFileHref(href: string): { path: string; line?: number } | null {
  if (!href.startsWith("rdsh-file:")) return null;
  const m = href.slice(10).match(/^(.*?)(?:#L(\d+))?$/)!;
  return { path: m[1], line: m[2] ? Number(m[2]) : undefined };
}

/** A path-like markdown link target that is a file on the host. */
function fileTarget(dest: string): string | undefined {
  const [p, line] = splitLine(dest);
  const abs = paths && pathLike(p) ? paths.lookup(p) : null;
  return abs ? fileHref(abs, line) : undefined;
}

/** Inline markdown: `code`, **bold**, *italic*, [links](url), and bare HTTP(S) URLs. */
export function inline(text: string, base = ""): Seg[] {
  return linkedText(text, base, INLINE);
}

function linkedText(text: string, base = "", pattern = BARE_URL): Seg[] {
  const out: Seg[] = [];
  let last = 0;
  const add = (t: string, href?: string) => {
    if (t) out.push(href ? [t, `${base} link`.trim(), href] : [t, base]);
  };
  // Plain text: link the paths the host confirmed are files.
  const plain = (t: string) => {
    if (!paths) return add(t);
    let at = 0;
    for (const m of t.matchAll(PATH)) {
      const raw = m[0].replace(/[.,:;]+$/, "");
      const [p, line] = splitLine(raw);
      const abs = pathLike(p) ? paths.lookup(p) : null;
      if (!abs) continue;
      add(t.slice(at, m.index));
      add(raw, fileHref(abs, line));
      at = m.index + raw.length;
    }
    add(t.slice(at));
  };
  for (const m of text.matchAll(pattern)) {
    const { code, bold, italic, label, angle, dest, auto, bare } = m.groups!;
    plain(text.slice(last, m.index));
    if (code) out.push(...linkedText(code.slice(1, -1), `${base} ic`.trim()));
    // Bold and italic text can hold code and links (`**`path`**`): parse inside them too.
    else if (bold) out.push(...inline(bold.slice(2, -2), `${base} b`.trim()));
    else if (italic) out.push(...inline(italic.slice(1, -1), `${base} i`.trim()));
    else if (label) add(label, webUrl(angle ?? dest) ?? fileTarget(angle ?? dest));
    else if (auto) {
      const href = webUrl(auto);
      add(href ? auto : m[0], href);
    } else {
      const url = bare.replace(/[.,;:!?]+$/, "");
      add(url, webUrl(url));
      add(bare.slice(url.length));
    }
    last = m.index + m[0].length;
  }
  plain(text.slice(last));
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
  for (const [t, ...style] of segs) for (const p of t.split(/(\s+)/)) if (p) toks.push([p, ...style]);
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
  for (let [t, ...style] of toks) {
    if (/^\s+$/.test(t)) {
      if (w === base && lines.length) continue;
      if (w + 1 > W) { push(); continue; }
      cur.push([" ", ...style]);
      w += 1;
      continue;
    }
    const room = Math.max(1, W - base);
    while (t.length > room) {
      if (w > base) push();
      cur.push([t.slice(0, room), ...style]);
      t = t.slice(room);
      w = W;
      push();
    }
    if (!t) continue;
    if (w + t.length > W) push();
    cur.push([t, ...style]);
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

/** Fixed-width wrapping that preserves spaces and link targets in tool output. */
export function* chunkSegs(segs: Seg[], width: number): Generator<Seg[]> {
  let row: Seg[] = [];
  let col = 0;
  for (const [text, ...style] of segs) {
    for (let i = 0; i < text.length;) {
      const n = Math.min(width - col, text.length - i);
      row.push([text.slice(i, i + n), ...style]);
      i += n;
      col += n;
      if (col === width) {
        yield row;
        row = [];
        col = 0;
      }
    }
  }
  if (row.length || !segs.length) yield row;
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
      const truncated = segText(c).length > w;
      let left = w;
      segs.push([" ", ""]);
      for (const [t, cls, ...link] of c) {
        if (!left) break;
        const visible = truncated && t.length >= left ? t.slice(0, left - 1) + "…" : t;
        segs.push([visible, ri === 0 ? `${cls} b`.trim() : cls, ...link]);
        left -= visible.length;
      }
      segs.push([" ".repeat(left), ""], [" ", ""], ["│", "dim"]);
    });
    out.push({ segs });
    if (ri === 0 && cells.length > 1) out.push(bar("├", "┼", "┤"));
  });
  out.push(bar("└", "┴", "┘"));
  return out;
}

const DEFAULT_DIAGRAM_ROWS = 18;

/**
 * Markdown to lines. A closed ```d3 block becomes a diagram (`// rows: N` sets the height,
 * `// title: …` the caption). An open one shows a placeholder, animated when `streamSpin`
 * is supplied for a live response.
 */
export function markdown(text: string, W: number, key = "", streamSpin?: number): Line[] {
  const L = text.split("\n");
  const out: Line[] = [];
  let diagrams = 0;
  for (let i = 0; i < L.length;) {
    const l = L[i];
    const fence = l.match(/^(\s*)(`{3,}|~{3,})\s*([\w+-]*)/);
    if (fence) {
      const close = fence[2];
      const lang = fence[3];
      const start = ++i;
      while (i < L.length && !L[i].trimStart().startsWith(close)) i++;
      const end = i++;
      if (lang === "d3") {
        if (end === L.length) {
          const segs: Seg[] = streamSpin === undefined
            ? [[fit("Diagram incomplete", W), "dim i"]]
            : [[SPIN[streamSpin % SPIN.length] + " ", "purple"], [fit("Generating diagram…", W - 2), "dim"]];
          out.push({ segs });
          continue;
        }
        const src = L.slice(start, end).join("\n");
        const rows = Number(src.match(/\/\/\s*rows:\s*(\d+)/)?.[1] ?? DEFAULT_DIAGRAM_ROWS);
        const title = src.match(/\/\/\s*title:\s*(.+)/)?.[1]?.trim() ?? "diagram";
        const d: Diagram = { key: `${key}#${diagrams++}`, script: src, rows: Math.max(4, Math.min(60, rows)), title };
        out.push({ segs: [["◆ ", "purple"], [fit(title, W - 40), "b purple"], ["   Enter to interact · click to pick", "dim"]] });
        for (let r = 0; r < d.rows; r++) out.push({ segs: [], dia: d, diaRow: r });
        continue;
      }
      out.push({ segs: [[` ${lang || "code"} `, "lang"]], bg: "code" });
      for (let j = start; j < end; j++) {
        for (const p of chunks(L[j], Math.max(1, W - 1))) out.push({ segs: [[" ", ""], ...highlight(p, lang)], bg: "code" });
      }
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

const cache = new WeakMap<Entry, { v: number; W: number; spin: number; epoch: number; lines: Line[] }>();

/** Rows of one entry (cached until the entry or the width changes), ending with a spacer. */
export function entryLines(e: Entry, W: number, spin: number): Line[] {
  const animating = e.tool?.status === "run" || (e.k === "assistant" && e.streaming);
  const c = cache.get(e);
  const epoch = paths?.epoch ?? 0;
  if (c && c.v === e.v && c.W === W && c.epoch === epoch && (!animating || c.spin === spin)) return c.lines;
  const lines = build(e, W, spin);
  cache.set(e, { v: e.v, W, spin, epoch, lines });
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
      out = markdown(e.text, W, String(e.id), e.streaming ? spin : undefined);
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
      if (t.edit && t.status !== "bad") out.push(...editLines(t.edit, t.output, W, e.collapsed));
      const ls = t.output ? t.output.split("\n") : [];
      const shown = e.collapsed ? Math.min(COLLAPSED_OUTPUT, ls.length) : ls.length;
      const cls = t.status === "bad" ? "err" : "fgdim";
      for (const l of ls.slice(0, shown)) for (const p of chunkSegs(linkedText(l, cls), Math.max(1, W - 2))) out.push({ g: ["│ ", "dim"], segs: p });
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
/** Line-number column of diffs: four digits and a space. */
export const NUM_W = 5;

/**
 * Rows of one diff line: `- `/`+ ` gutter, a line number, the code highlighted for its language,
 * the changed words marked, and the row tinted; long lines wrap under the number column.
 */
export function diffRows(kind: "ctx" | "del" | "add", text: string, num: number | null, lang: string, W: number, ranges?: [number, number][]): Line[] {
  const cls = kind === "del" ? "delw" : "addw";
  const code = ranges?.length ? markRanges(highlight(text, lang), ranges, cls) : highlight(text, lang);
  const bg = kind === "del" ? "del" : kind === "add" ? "add" : undefined;
  const sign: Seg = kind === "del" ? ["- ", "bad"] : kind === "add" ? ["+ ", "ok"] : ["  ", "dim"];
  const out: Line[] = [];
  let first = true;
  for (const part of chunkSegs(code, Math.max(10, W - NUM_W))) {
    const lead: Seg = [first ? String(num ?? "").padStart(NUM_W - 1) + " " : " ".repeat(NUM_W), "dim"];
    out.push({ g: first ? sign : ["  ", ""], segs: [lead, ...part], bg });
    first = false;
  }
  return out;
}

/** The diff of an `edit_file` call, numbered from the line its output reports. */
function editLines(edit: { path: string; old: string; new: string }, output: string, W: number, collapsed: boolean): Line[] {
  const lang = edit.path.split(".").pop() ?? "";
  const ops = diffLines(edit.old, edit.new);
  const ranges = pairRanges(ops, (o) => (o.kind === "ctx" ? "other" : o.kind), (o) => o.text);
  const start = Number(output.match(/:(\d+) \(\d+ replacement/)?.[1]) || null;
  let oldN = start, newN = start;
  const lines: Line[] = [];
  const shown = collapsed ? ops.slice(0, COLLAPSED_EDIT) : ops;
  for (const o of shown) {
    const n = o.kind === "del" ? oldN : newN;
    lines.push(...diffRows(o.kind, o.text, n, lang, W, ranges.get(o)));
    if (o.kind !== "add" && oldN != null) oldN++;
    if (o.kind !== "del" && newN != null) newN++;
  }
  if (shown.length < ops.length) lines.push({ g: ["  ", ""], segs: [[`… +${ops.length - shown.length} diff lines (Tab to expand)`, "dim"]] });
  return lines;
}
const COLLAPSED_EDIT = 12;

/** All rows of a transcript, without the spacer after the last entry. */
export function layout(entries: Entry[], W: number, spin: number, links?: PathLinks): Row[] {
  const rows: Row[] = [];
  paths = links ?? null;
  try {
    for (const e of entries) {
      for (const l of entryLines(e, W, spin)) rows.push({ ...l, g: l.g ?? ["  ", ""], e, text: segText(l.segs) });
    }
  } finally {
    paths = null;
  }
  if (rows.length) rows.pop();
  return rows;
}
