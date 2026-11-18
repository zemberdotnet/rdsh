// A file opened from the chat: its diff against HEAD (or the default branch) or the whole file
// with changed lines marked. Ported from src/tui/fileview.rs; drawn with the transcript's rows.

import { chunks, highlight, type Row, type Seg } from "./layout.ts";
import type { Base, DiffLine, FileDiff } from "./proto.ts";
import type { Cursor, Entry } from "./session.ts";

/** Rows of a file view belong to no transcript entry. */
const NO_ENTRY: Entry = { id: -1, k: "info", text: "", collapsed: false, streaming: false, v: 0 };
/** Line-number column: four digits and a space. */
const NUM_W = 5;

export class FileView {
  host: number;
  /** The session's directory, which `read_file` resolves against. */
  cwd: string;
  file: FileDiff;
  /** Whole file (true) or only the hunks (false). */
  full: boolean;
  base: Base;
  cur: Cursor = { row: 0, col: 0, top: 0, follow: false, want: null };
  /** The diff line each row shows (null for the title row). */
  lineOf: (DiffLine | null)[] = [];
  /** A line number to put the cursor on once rows are laid out; "first" for the first change. */
  private jump: number | "first" | null;

  constructor(host: number, cwd: string, file: FileDiff, line?: number) {
    this.host = host;
    this.cwd = cwd;
    this.file = file;
    this.full = file.diff.length === 0;
    this.base = file.base ? "default" : "head";
    this.jump = line ?? "first";
  }

  /** Shows a freshly loaded copy, keeping the line under the cursor. */
  replace(file: FileDiff) {
    this.jump = this.currentLine();
    this.file = file;
    this.full = this.full || file.diff.length === 0;
  }

  toggleFull() {
    this.jump = this.currentLine();
    this.full = !this.full;
  }

  /** Line number under the cursor: the new side, else the old one. */
  currentLine(): number | null {
    const l = this.lineOf[this.cur.row];
    return l ? (l.new ?? l.old) : null;
  }

  title(): string {
    const f = this.file;
    if (f.error) return `${f.rel} — ${f.error}`;
    const state = f.status === "Modified" ? `+${f.added} -${f.removed}` : f.status === "Untracked" ? `untracked, ${f.full.length} lines`
      : f.status === "Clean" ? "no changes" : "not in a git repository";
    return `${f.rel} — ${state}${f.base ? ` vs ${f.base}` : ""} — ${this.full ? "file" : "diff"} view`;
  }

  /** Rows for width `W`: a title, then one or more rows per line (long lines wrap). */
  rows(W: number): Row[] {
    const lang = this.file.rel.split(".").pop() ?? "";
    const src = (this.full ? this.file.full : this.file.diff).filter((l) => l.kind !== "Meta");
    const rows: Row[] = [];
    this.lineOf = [];
    const push = (r: Omit<Row, "e" | "text">, line: DiffLine | null) => {
      rows.push({ ...r, e: NO_ENTRY, text: r.segs.map((s) => s[0]).join("") });
      this.lineOf.push(line);
    };
    push({ g: ["  ", ""], segs: [[this.title(), "b"]], bg: "code" }, null);
    const room = Math.max(10, W - NUM_W);
    for (const l of src) {
      if (l.kind === "Hunk") {
        push({ g: ["  ", ""], segs: [[l.text, "aqua"]], bg: "hunk" }, l);
        continue;
      }
      const sign = l.kind === "Add" ? "+ " : l.kind === "Del" ? "- " : "  ";
      const num = String((l.kind === "Del" ? l.old : l.new) ?? "").padStart(NUM_W - 1) + " ";
      const bg = l.kind === "Add" ? "add" : l.kind === "Del" ? "del" : undefined;
      chunks(l.text, room).forEach((part, i) => {
        const segs: Seg[] = [[i ? " ".repeat(NUM_W) : num, "dim"], ...highlight(part, lang)];
        push({ g: [i ? "  " : sign, l.kind === "Add" ? "ok" : l.kind === "Del" ? "bad" : "dim"], segs, bg }, l);
      });
    }
    if (this.jump != null && rows.length > 1) {
      const want = this.jump;
      this.jump = null;
      let i = want === "first"
        ? this.lineOf.findIndex((l) => l?.kind === "Add" || l?.kind === "Del")
        : this.lineOf.findIndex((l) => l != null && l.new != null && l.new >= want);
      if (i < 1) i = want === "first" ? 1 : rows.length - 1;
      this.cur.row = i;
      this.cur.col = 0;
      this.cur.top = Math.max(0, i - 5);
    }
    return rows;
  }

  /** Rows where hunks (diff view) or runs of added lines (file view) start. */
  blocks(): number[] {
    return this.lineOf.flatMap((l, i) => {
      if (!l) return [];
      if (!this.full) return l.kind === "Hunk" ? [i] : [];
      const prev = this.lineOf[i - 1];
      return l.kind === "Add" && prev?.kind !== "Add" && prev !== l ? [i] : [];
    });
  }
}
