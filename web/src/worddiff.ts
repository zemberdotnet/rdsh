// Line diffs for edits, and which part of a changed line changed (word level), for the
// second, stronger highlight on removed and added lines.

import type { Seg } from "./layout.ts";

export type Op = { kind: "ctx" | "del" | "add"; text: string };
/** Character ranges `[start, end)` within one line. */
export type Ranges = [number, number][];

/** Longest-common-subsequence table walk: a minimal edit script from `a` to `b`. */
function lcs<T>(a: T[], b: T[], eq: (x: T, y: T) => boolean): ("ctx" | "del" | "add")[] {
  const n = a.length, m = b.length;
  const t: Uint32Array[] = Array.from({ length: n + 1 }, () => new Uint32Array(m + 1));
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) t[i][j] = eq(a[i], b[j]) ? t[i + 1][j + 1] + 1 : Math.max(t[i + 1][j], t[i][j + 1]);
  }
  const out: ("ctx" | "del" | "add")[] = [];
  let i = 0, j = 0;
  while (i < n || j < m) {
    if (i < n && j < m && eq(a[i], b[j])) { out.push("ctx"); i++; j++; }
    else if (j < m && (i === n || t[i][j + 1] >= t[i + 1][j])) { out.push("add"); j++; }
    else { out.push("del"); i++; }
  }
  return out;
}

/** Lines of `before` and `after` as context, removed and added lines (removals first). */
export function diffLines(before: string, after: string): Op[] {
  const a = before.split("\n"), b = after.split("\n");
  // Common head and tail first: edits are small, so the table stays small.
  let head = 0;
  while (head < a.length && head < b.length && a[head] === b[head]) head++;
  let tail = 0;
  while (tail < a.length - head && tail < b.length - head && a[a.length - 1 - tail] === b[b.length - 1 - tail]) tail++;
  const midA = a.slice(head, a.length - tail), midB = b.slice(head, b.length - tail);
  const ops: Op[] = a.slice(0, head).map((text) => ({ kind: "ctx", text }));
  if (midA.length * midB.length > 250_000) {
    ops.push(...midA.map((text) => ({ kind: "del" as const, text })), ...midB.map((text) => ({ kind: "add" as const, text })));
  } else {
    let i = 0, j = 0;
    const mid: Op[] = [];
    for (const k of lcs(midA, midB, (x, y) => x === y)) {
      if (k === "add") mid.push({ kind: k, text: midB[j++] });
      else {
        mid.push({ kind: k, text: midA[i++] });
        if (k === "ctx") j++;
      }
    }
    ops.push(...regroup(mid));
  }
  ops.push(...a.slice(a.length - tail).map((text) => ({ kind: "ctx" as const, text })));
  return ops;
}

/** Puts each run of changes as its removals, then its additions. */
function regroup(ops: Op[]): Op[] {
  const out: Op[] = [];
  for (let i = 0; i < ops.length;) {
    if (ops[i].kind === "ctx") { out.push(ops[i++]); continue; }
    const run: Op[] = [];
    while (i < ops.length && ops[i].kind !== "ctx") run.push(ops[i++]);
    out.push(...run.filter((o) => o.kind === "del"), ...run.filter((o) => o.kind === "add"));
  }
  return out;
}

const TOKEN = /\w+|\s+|[^\w\s]/g;

/**
 * The changed parts of a line that was replaced: ranges in `before` that were removed and in
 * `after` that were added, by word. Whitespace-only differences between changed words are
 * folded into them so the highlight reads as one span.
 */
export function changedRanges(before: string, after: string): [Ranges, Ranges] {
  const a = before.match(TOKEN) ?? [], b = after.match(TOKEN) ?? [];
  if (a.length * b.length > 40_000) return [[[0, before.length]], [[0, after.length]]];
  const ra: Ranges = [], rb: Ranges = [];
  let pa = 0, pb = 0, i = 0, j = 0;
  const add = (r: Ranges, s: number, e: number) => {
    const last = r[r.length - 1];
    if (last && last[1] === s) last[1] = e;
    else r.push([s, e]);
  };
  for (const k of lcs(a, b, (x, y) => x === y)) {
    if (k === "ctx") { pa += a[i++].length; pb += b[j++].length; }
    else if (k === "del") { add(ra, pa, pa + a[i].length); pa += a[i++].length; }
    else { add(rb, pb, pb + b[j].length); pb += b[j++].length; }
  }
  return [bridge(ra, before), bridge(rb, after)];
}

/** Joins ranges separated only by whitespace. */
function bridge(r: Ranges, text: string): Ranges {
  const out: Ranges = [];
  for (const [s, e] of r) {
    const last = out[out.length - 1];
    if (last && !text.slice(last[1], s).trim()) last[1] = e;
    else out.push([s, e]);
  }
  return out;
}

/**
 * Word ranges for each line of a diff: removed and added lines paired up within each run of
 * changes (first removal with first addition, and so on). Unpaired lines get none, since the
 * whole line is new or gone.
 */
export function pairRanges<T>(lines: T[], kind: (l: T) => "del" | "add" | "other", text: (l: T) => string): Map<T, Ranges> {
  const out = new Map<T, Ranges>();
  for (let i = 0; i < lines.length;) {
    if (kind(lines[i]) !== "del") { i++; continue; }
    const dels: T[] = [];
    while (i < lines.length && kind(lines[i]) === "del") dels.push(lines[i++]);
    const adds: T[] = [];
    while (i < lines.length && kind(lines[i]) === "add") adds.push(lines[i++]);
    for (let k = 0; k < Math.min(dels.length, adds.length); k++) {
      const [ra, rb] = changedRanges(text(dels[k]), text(adds[k]));
      // A line changed nearly everywhere reads better without the second highlight.
      if (covers(ra, text(dels[k])) && covers(rb, text(adds[k]))) continue;
      out.set(dels[k], ra);
      out.set(adds[k], rb);
    }
  }
  return out;
}

function covers(r: Ranges, text: string): boolean {
  const n = r.reduce((s, [a, b]) => s + b - a, 0);
  return text.trim().length > 0 && n >= text.trim().length * 0.8;
}

/** Adds `cls` to the parts of `segs` inside `ranges` (offsets from the start of `segs`). */
export function mark(segs: Seg[], ranges: Ranges, cls: string, offset = 0): Seg[] {
  if (!ranges.length) return segs;
  const out: Seg[] = [];
  let col = offset;
  for (const [t, c, ...rest] of segs) {
    let i = 0;
    while (i < t.length) {
      const inside = ranges.some(([s, e]) => col + i >= s && col + i < e);
      let j = i + 1;
      while (j < t.length && ranges.some(([s, e]) => col + j >= s && col + j < e) === inside) j++;
      out.push([t.slice(i, j), inside ? `${c} ${cls}`.trim() : c, ...rest]);
      i = j;
    }
    col += t.length;
  }
  return out;
}
