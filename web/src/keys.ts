// Vim keys: normal/visual on the transcript, the rail, the command and search lines, diagram
// mode. The browser keeps Ctrl-w (close tab), so window commands use Ctrl-Space as the prefix;
// Ctrl-w still works where the browser lets the page have it (an app window).

import type { App } from "./app.ts";
import { linkAt, parseFileHref, type Row } from "./layout.ts";
import { entryJump, find, wordBack, wordEnd, wordForward } from "./motion.ts";
import type { View } from "./view.ts";

export interface Key { key: string; ctrlKey: boolean; shiftKey: boolean; altKey: boolean; metaKey?: boolean }

export function keyName(e: Key): string {
  if (e.ctrlKey && e.key === " ") return "C-Space";
  if (e.ctrlKey && e.key.length === 1) return `C-${e.key.toLowerCase()}`;
  if (e.key === "Tab" && e.shiftKey) return "S-Tab";
  return e.key;
}

const isCount = (app: App, k: string) => /^[0-9]$/.test(k) && (k !== "0" || app.count != null);

export class Keys {
  private app: App;
  private view: View;
  private input: HTMLTextAreaElement;
  constructor(app: App, view: View, input: HTMLTextAreaElement) {
    this.app = app;
    this.view = view;
    this.input = input;
  }

  /** Handles a key; `true` when it was used (the caller then prevents the default). */
  handle(e: Key): boolean {
    const a = this.app;
    const key = keyName(e);
    if (["Shift", "Control", "Alt", "Meta", "CapsLock", "Dead"].includes(key) || e.metaKey) return false;
    if (key === "C-c" && a.cancel()) return true;
    if (key === "Enter" && e.ctrlKey && !this.view.form.open) {
      a.sendNow();
      return true;
    }
    if (key === "S-Tab") {
      // An open file switches its diff base instead.
      if (a.file && ["normal", "visual", "vline"].includes(a.mode)) a.toggleFileBase();
      else a.cycleEffort();
      return true;
    }
    if (a.help) {
      a.help = false;
      return true;
    }
    a.setFlash(null);

    if (a.pending === "W") {
      a.pending = null;
      if (/^(h|C-h|ArrowLeft)$/.test(key)) a.focusRail(true);
      else if (/^(l|C-l|w|C-w|C-Space|ArrowRight)$/.test(key)) a.focusRail(false);
      return true;
    }
    if ((key === "C-Space" || key === "C-w") && !["insert", "command", "search"].includes(a.mode)) {
      if (a.mode === "diagram") this.leaveDiagram();
      a.pending = "W";
      return true;
    }

    switch (a.mode) {
      case "insert":
      case "form":
        return false;
      case "command":
      case "search":
        return this.minibuf(key);
      case "diagram":
        if (key === "Escape" || key === "q") this.leaveDiagram();
        else if (a.diagram) this.view.frames.send(a.diagram.key, { type: "act", key });
        return true;
    }
    if (a.railFocus) return this.rail(key);
    return this.buffer(key);
  }

  private minibuf(key: string): boolean {
    const a = this.app;
    if (key === "Escape") a.mode = "normal";
    else if (key === "Enter") {
      const text = a.minibuf;
      const search = a.mode === "search";
      a.mode = "normal";
      if (!search) a.runCommand(text);
      else if (text) {
        a.lastSearch = text;
        this.search(a.searchFwd);
      }
    } else if (key === "Backspace") {
      if (!a.minibuf) a.mode = "normal";
      a.minibuf = a.minibuf.slice(0, -1);
    } else if (key === "C-u") a.minibuf = "";
    else if (key === "C-w") a.minibuf = a.minibuf.replace(/\S*\s*$/, "");
    else if ([...key].length === 1) a.minibuf += key;
    return true;
  }

  private rail(key: string): boolean {
    const a = this.app;
    if (a.pending === "z") {
      a.pending = null;
      a.count = null;
      if (key === "h") {
        const before = a.railRows.length;
        a.showStale = !a.showStale;
        a.refreshRail();
        const n = a.railRows.length - before;
        a.setFlash(a.showStale ? (n > 0 ? `showing ${n} finished subagents` : "no finished subagents to show") : "hiding subagents finished over 5 minutes ago");
      }
      return true;
    }
    if (isCount(a, key)) {
      a.count = Math.min(100000, (a.count ?? 0) * 10 + Number(key));
      return true;
    }
    const count = a.count;
    a.count = null;
    const n = count ?? 1;
    const i = a.railIndex();
    const last = a.railRows.length - 1;
    const half = Math.max(1, Math.floor(this.view.visibleRows() / 2));
    switch (key) {
      case "j": case "ArrowDown": a.selectRail(Math.min(last, i + n)); break;
      case "k": case "ArrowUp": a.selectRail(Math.max(0, i - n)); break;
      case "g": case "Home": a.selectRail(0); break;
      case "G": case "End": a.selectRail(count != null ? Math.min(last, count - 1) : last); break;
      case "C-d": case "PageDown": a.selectRail(Math.min(last, i + half)); break;
      case "C-u": case "PageUp": a.selectRail(Math.max(0, i - half)); break;
      case "z": a.pending = "z"; break;
      case "Enter": a.openSelected(); break;
      case "y": {
        const r = a.railRows[i];
        if (r && !r.header) this.copy(r.id, r.id);
        break;
      }
      case "r": a.request(a.railRows[i]?.host ?? 0, { type: "list" }); a.setFlash("sessions reloaded"); break;
      case "S": a.railOpen = false; a.focusRail(false); break;
      case "i": case "a": a.focusRail(false); this.startInsert(); break;
      case ":": a.mode = "command"; a.minibuf = ""; break;
      case "Escape": case "Tab": case "l": case "q": case "ArrowRight": a.focusRail(false); break;
      default: return false;
    }
    return true;
  }

  private buffer(key: string): boolean {
    const a = this.app;
    const c = a.cursor();
    const rows = a.rows;
    if (a.pending) {
      const p = a.pending;
      a.pending = null;
      const count = a.count;
      a.count = null;
      if (p === "g" && key === "g") this.setRow(count != null ? count - 1 : 0);
      else if (p === "g" && key === "x") this.openLink(rows[c.row], c.col);
      else if (p === "g" && key === "f") this.openLink(rows[c.row], c.col, true);
      else if (p === "z" && key === "a") this.fold();
      else if (p === "z" && /^[ztb]$/.test(key)) {
        const vis = this.view.visibleRows();
        c.top = key === "z" ? c.row - Math.floor(vis / 2) : key === "t" ? c.row : c.row - vis + 1;
        c.follow = false;
      } else if (p === "y" && key === "y") this.copy(rows[c.row]?.text ?? "", "1 line");
      return true;
    }
    if (isCount(a, key)) {
      a.count = Math.min(100000, (a.count ?? 0) * 10 + Number(key));
      return true;
    }
    const visual = a.mode === "visual" || a.mode === "vline";
    if (visual) {
      if (key === "Escape" || (key === "v" && a.mode === "visual") || (key === "V" && a.mode === "vline")) {
        a.mode = "normal";
        a.count = null;
        return true;
      }
      if (key === "v" || key === "V") {
        a.mode = key === "v" ? "visual" : "vline";
        return true;
      }
      if (key === "y") return this.yankSelection();
    }
    if (key === "g" || key === "z" || (key === "y" && !visual)) {
      a.pending = key;
      return true;
    }
    const count = a.count;
    a.count = null;
    if (this.motion(key, count)) return true;
    if (a.file && this.fileKey(key)) return true;
    switch (key) {
      case "i": case "a": case "A": case "I": case "o":
        if (key !== "o" && this.view.form.open && !a.previewing()) {
          this.view.form.focus();
          break;
        }
        if (a.previewing()) {
          a.setFlash("previewing — Enter on the rail switches to it", true);
          break;
        }
        if (a.cur.readOnly) {
          a.setFlash("read-only trace", true);
          break;
        }
        this.startInsert();
        break;
      case ":": a.mode = "command"; a.minibuf = ""; break;
      case "/": case "?": a.mode = "search"; a.searchFwd = key === "/"; a.minibuf = ""; break;
      case "n": this.search(a.searchFwd); break;
      case "N": this.search(!a.searchFwd); break;
      case "v": case "V": a.mode = key === "v" ? "visual" : "vline"; a.vAnchor = { row: c.row, col: c.col }; break;
      case "Tab": if (!this.fold()) a.setFlash("nothing to fold here"); break;
      case "Enter": {
        const r = rows[c.row];
        const href = linkAt(r, c.col);
        const task = r?.e.tool?.name === "task" ? r.e.tool.session : undefined;
        if (href) a.openHref(href);
        else if (task) a.openSubagent(task);
        else if (r?.dia) this.enterDiagram(r.dia.key, true);
        else if (rows[c.row + 1]?.dia && rows[c.row + 1].diaRow === 0) this.enterDiagram(rows[c.row + 1].dia!.key, true);
        else if (!this.fold()) return false;
        break;
      }
      case "S": a.railOpen = !a.railOpen; if (!a.railOpen) a.focusRail(false); break;
      case "C-o": if (!a.goBack()) a.setFlash("nothing to go back to"); break;
      case "q": if (!a.cur.readOnly || !a.goBack()) return false; break;
      case "Escape": a.lastSearch = null; break;
      case "F1": a.help = true; break;
      default: return false;
    }
    return true;
  }

  private setRow(r: number) {
    const a = this.app;
    const c = a.cursor();
    c.row = Math.max(0, Math.min(r, a.rows.length - 1));
    if (c.want != null) c.col = c.want;
    c.follow = c.row === a.rows.length - 1;
  }

  private motion(key: string, count: number | null): boolean {
    const a = this.app;
    const c = a.cursor();
    const n = count ?? 1;
    const vis = this.view.visibleRows();
    const lines = a.rows.map((r) => r.text);
    const go = (p: { row: number; col: number }) => {
      c.row = p.row;
      c.col = p.col;
      c.want = p.col;
    };
    const len = () => lines[c.row]?.length ?? 0;
    switch (key) {
      case "j": case "ArrowDown": this.setRow(c.row + n); break;
      case "k": case "ArrowUp": this.setRow(c.row - n); break;
      case "h": case "ArrowLeft": c.col = Math.max(0, c.col - n); c.want = c.col; break;
      case "l": case "ArrowRight": c.col = Math.min(Math.max(0, len() - 1), c.col + n); c.want = c.col; break;
      case "w": for (let i = 0; i < n; i++) go(wordForward(lines, c)); break;
      case "b": for (let i = 0; i < n; i++) go(wordBack(lines, c)); break;
      case "e": for (let i = 0; i < n; i++) go(wordEnd(lines, c)); break;
      case "0": case "Home": c.col = 0; c.want = 0; break;
      case "^": c.col = Math.max(0, (lines[c.row] ?? "").search(/\S/)); c.want = c.col; break;
      case "$": case "End": c.col = Math.max(0, len() - 1); c.want = 1e9; break;
      case "G": this.setRow(count != null ? count - 1 : a.rows.length - 1); break;
      case "C-d": this.setRow(c.row + Math.floor(vis / 2) * n); c.top += Math.floor(vis / 2) * n; break;
      case "C-u": this.setRow(c.row - Math.floor(vis / 2) * n); c.top -= Math.floor(vis / 2) * n; break;
      case "C-f": case "PageDown": this.setRow(c.row + vis * n); c.top += vis * n; break;
      case "C-b": case "PageUp": this.setRow(c.row - vis * n); c.top -= vis * n; break;
      case "C-e": c.top += n; if (c.row < c.top) this.setRow(c.top); break;
      case "C-y": c.top = Math.max(0, c.top - n); if (c.row >= c.top + vis) this.setRow(c.top + vis - 1); break;
      case "H": this.setRow(c.top); break;
      case "M": this.setRow(c.top + Math.floor(Math.min(vis, a.rows.length - c.top) / 2)); break;
      case "L": this.setRow(Math.min(a.rows.length, c.top + vis) - 1); break;
      case "}": case "{": {
        const owners = a.rows.map((r) => r.e);
        let r = c.row;
        for (let i = 0; i < n; i++) r = entryJump(owners, r, key === "}" ? 1 : -1);
        c.row = r;
        c.col = 0;
        c.want = 0;
        break;
      }
      default: return false;
    }
    c.follow = c.row === a.rows.length - 1 && !["C-e", "C-y"].includes(key);
    return true;
  }

  private search(forward: boolean) {
    const a = this.app;
    const q = a.lastSearch;
    if (!q) return;
    const c = a.cursor();
    const hit = find(a.rows.map((r) => r.text), q, c, forward);
    if (!hit) return a.setFlash(`not found: ${q}`, true);
    c.row = hit.pos.row;
    c.col = hit.pos.col;
    c.want = c.col;
    c.follow = false;
    if (hit.wrapped) a.setFlash(forward ? "search hit bottom, continuing at top" : "search hit top, continuing at bottom");
  }

  /** `gx`: opens the link under the cursor in a new tab, else the row's first link. */
  /** `gx` (any link) or `gf` (files only): the link under the cursor, else the row's first. */
  private openLink(row: Row | undefined, col: number, files = false) {
    const a = this.app;
    const ok = (h: string | undefined): h is string => !!h && (!files || !!parseFileHref(h));
    const here = linkAt(row, col);
    const href = ok(here) ? here : row?.segs.map((s) => s[2]).find(ok);
    if (!href) return a.setFlash(files ? "no file link on this line" : "no link on this line");
    a.openHref(href);
  }

  /** Keys of the file view; false for keys it leaves to the transcript's handling. */
  private fileKey(key: string): boolean {
    const a = this.app;
    const f = a.file!;
    switch (key) {
      case "Tab": f.toggleFull(); break;
      case "r": a.reloadFile(); a.setFlash("reloaded"); break;
      case "q": case "Escape": a.closeFile(); break;
      case "]": case "[": {
        const bs = f.blocks();
        const row = f.cur.row;
        const to = key === "]" ? bs.find((b) => b > row) : [...bs].reverse().find((b) => b < row);
        if (to == null) a.setFlash(key === "]" ? "last change" : "first change");
        else f.cur.row = to;
        break;
      }
      default: return false;
    }
    return true;
  }

  /** Folds or unfolds the tool output or reasoning under the cursor. */
  fold(): boolean {
    const a = this.app;
    if (a.file) return false;
    const v = a.shown();
    const c = v.cur;
    const e = a.rows[c.row]?.e;
    if (!e || (e.k !== "tool" && e.k !== "reasoning") || e.streaming) return false;
    e.collapsed = !e.collapsed;
    v.touch(e);
    a.relayout(a.cols);
    c.row = Math.max(0, a.rows.findIndex((r) => r.e === e));
    c.col = 0;
    c.follow = false;
    return true;
  }

  private yankSelection(): boolean {
    const a = this.app;
    const c = a.cursor();
    const v = a.vAnchor!;
    const b = { row: c.row, col: c.col };
    const [s, t] = v.row < b.row || (v.row === b.row && v.col <= b.col) ? [v, b] : [b, v];
    const sel = a.rows.slice(s.row, t.row + 1);
    const text = a.mode === "vline"
      ? sel.map((r) => r.text).join("\n")
      : sel.map((r, i) => r.text.slice(i === 0 ? s.col : 0, i === sel.length - 1 ? t.col + 1 : undefined)).join("\n");
    this.copy(text, `${sel.length} line${sel.length === 1 ? "" : "s"}`);
    a.mode = "normal";
    c.row = s.row;
    c.col = s.col;
    return true;
  }

  copy(text: string, what: string) {
    navigator.clipboard?.writeText(text).then(
      () => this.app.setFlash(`copied ${what}`),
      () => this.app.setFlash("the browser refused the clipboard", true),
    ).finally(() => this.app.redraw());
  }

  startInsert() {
    this.app.mode = "insert";
    this.input.focus();
    const n = this.input.value.length;
    this.input.setSelectionRange(n, n);
  }

  enterDiagram(key: string, scroll: boolean) {
    const a = this.app;
    const first = a.rows.findIndex((r) => r.dia?.key === key && r.diaRow === 0);
    if (first < 0) return;
    const d = a.rows[first].dia!;
    a.diagram = d;
    a.mode = "diagram";
    const c = a.cursor();
    c.row = first;
    c.follow = false;
    if (scroll && (first < c.top || first + d.rows > c.top + this.view.visibleRows())) c.top = Math.max(0, first - 1);
  }

  leaveDiagram() {
    this.app.diagram = null;
    this.app.mode = "normal";
    // Keys typed in a frame go to the frame: hand the focus back to the page.
    if (document.activeElement instanceof HTMLIFrameElement) document.activeElement.blur();
  }

  /** Keys typed in the prompt box: only what the textarea does not do itself. */
  insertKey(e: KeyboardEvent): boolean {
    const a = this.app;
    const key = keyName(e);
    const t = this.input;
    switch (key) {
      case "Escape":
        a.discardQueueEdit();
        a.mode = "normal";
        t.blur();
        return true;
      case "S-Tab":
        a.cycleEffort();
        return true;
      case "Enter":
        if (e.shiftKey || e.altKey) return false;
        if (e.ctrlKey) a.sendNow();
        else a.submit();
        return true;
      case "C-j":
        t.setRangeText("\n", t.selectionStart, t.selectionEnd, "end");
        return true;
      case "Tab": {
        // Complete the path before the cursor on the session's host.
        if (e.ctrlKey || e.altKey || e.metaKey || t.selectionStart !== t.selectionEnd) return false;
        const end = t.selectionStart;
        const token = t.value.slice(0, end).match(/[^\s'"`()<>[\]{},;]*$/)![0];
        const start = end - token.length;
        let current = token;
        return a.completePath(token, (path) => {
          // Typing since the request moved or changed the token: leave the prompt alone.
          if (t.value.slice(start, start + current.length) !== current) return false;
          t.setRangeText(path, start, start + current.length, "end");
          current = path;
          return true;
        });
      }
      case "C-c":
        if (a.cancel() || a.discardQueueEdit()) return true;
        t.value = "";
        return true;
      case "ArrowUp":
      case "ArrowDown": {
        if (e.altKey || e.ctrlKey || e.shiftKey || e.metaKey || t.selectionStart !== t.selectionEnd) return false;
        const up = key === "ArrowUp";
        // Move the cursor within multiline text before entering queue or history navigation.
        const side = up ? t.value.slice(0, t.selectionStart) : t.value.slice(t.selectionEnd);
        if (side.includes("\n")) return false;
        if (a.recallQueued(up)) return true;
        // History only from an empty prompt or while already walking it, never while editing.
        if (a.cur.queueEdit || (t.value && a.histPos == null) || !a.history.length) return false;
        if (up) a.histPos = a.histPos == null ? a.history.length - 1 : Math.max(0, a.histPos - 1);
        else if (a.histPos == null) return false;
        else a.histPos = a.histPos + 1 < a.history.length ? a.histPos + 1 : null;
        t.value = a.histPos == null ? "" : a.history[a.histPos];
        return true;
      }
    }
    return false;
  }
}
