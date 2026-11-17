// Vim word motions over the grid's rows of plain text.

export interface Pos { row: number; col: number }

/** 0 blank, 1 word character, 2 punctuation; -1 past the end. */
const kind = (ch: string | undefined) => (ch === undefined ? -1 : /\s/.test(ch) ? 0 : /\w/.test(ch) ? 1 : 2);

/** `w`: start of the next word, across lines; an empty line is a stop. */
export function wordForward(lines: string[], p: Pos): Pos {
  let { row, col } = p;
  let t = lines[row] ?? "";
  const k0 = kind(t[col]);
  while (col < t.length && kind(t[col]) === k0 && k0 !== 0) col++;
  for (;;) {
    while (col < t.length && kind(t[col]) === 0) col++;
    if (col < t.length) return { row, col };
    if (row >= lines.length - 1) return { row, col: Math.max(0, t.length - 1) };
    row++;
    col = 0;
    t = lines[row];
    if (!t.length || kind(t[0]) !== 0) return { row, col: 0 };
  }
}

/** `b`: start of the previous word. */
export function wordBack(lines: string[], p: Pos): Pos {
  let { row, col } = p;
  let t = lines[row] ?? "";
  col--;
  for (;;) {
    while (col >= 0 && kind(t[col]) === 0) col--;
    if (col >= 0) break;
    if (row === 0) return { row: 0, col: 0 };
    row--;
    t = lines[row];
    col = t.length - 1;
    if (!t.length) return { row, col: 0 };
  }
  const k0 = kind(t[col]);
  while (col > 0 && kind(t[col - 1]) === k0) col--;
  return { row, col };
}

/** `e`: end of the current or next word. */
export function wordEnd(lines: string[], p: Pos): Pos {
  let { row, col } = p;
  let t = lines[row] ?? "";
  col++;
  for (;;) {
    while (col < t.length && kind(t[col]) === 0) col++;
    if (col < t.length) break;
    if (row >= lines.length - 1) return { row, col: Math.max(0, t.length - 1) };
    row++;
    t = lines[row];
    col = 0;
  }
  const k0 = kind(t[col]);
  while (col + 1 < t.length && kind(t[col + 1]) === k0) col++;
  return { row, col };
}

/** `}` / `{`: first row of the next entry, or the start of this (then the previous) entry. */
export function entryJump<T>(owners: T[], row: number, dir: 1 | -1): number {
  let i = row;
  const e = owners[row];
  if (dir > 0) {
    while (i < owners.length - 1 && owners[i] === e) i++;
    return i;
  }
  while (i > 0 && owners[i - 1] === e) i--;
  if (i === row && i > 0) {
    const prev = owners[--i];
    while (i > 0 && owners[i - 1] === prev) i--;
  }
  return i;
}

/** Plain-text search with smart case; returns matches as `[start, end)` columns. */
export function matches(text: string, q: string): [number, number][] {
  if (!q) return [];
  const re = new RegExp(q.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"), q === q.toLowerCase() ? "gi" : "g");
  const out: [number, number][] = [];
  let m: RegExpExecArray | null;
  while ((m = re.exec(text))) out.push([m.index, m.index + m[0].length]);
  return out;
}

/** Next match of `q` from `p` (exclusive), wrapping around; `wrapped` when it passed the end. */
export function find(lines: string[], q: string, p: Pos, forward: boolean): { pos: Pos; wrapped: boolean } | null {
  const n = lines.length;
  for (let k = 0; k <= n; k++) {
    const i = forward ? p.row + k : p.row - k;
    const row = ((i % n) + n) % n;
    const hits = matches(lines[row], q).map((m) => m[0]);
    const ok = k === 0 ? hits.filter((h) => (forward ? h > p.col : h < p.col)) : k === n ? hits.filter((h) => (forward ? h <= p.col : h >= p.col)) : hits;
    if (ok.length) return { pos: { row, col: forward ? ok[0] : ok[ok.length - 1] }, wrapped: forward ? row < p.row || (row === p.row && k > 0) : row > p.row || (row === p.row && k > 0) };
  }
  return null;
}
