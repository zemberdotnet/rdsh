// Page state: hosts, sessions, the rail, modes. Handles messages from `rdsh web` the way
// src/tui/app.rs handles hub notices, and turns user actions into requests.

import { FileView } from "./fileview.ts";
import { layout, parseFileHref, type Diagram, type PathLinks, type Row } from "./layout.ts";
import { EFFORTS, millis, type Answer, type ClientMsg, type Effort, type HostSpec, type Notice, type Request, type ServerMsg, type SessionInfo, type Snapshot } from "./proto.ts";
import { rail, type HostRail, type LiveSub, type RailRow } from "./rail.ts";
import { SessionView, type Hooks } from "./session.ts";

export type Mode = "normal" | "insert" | "command" | "search" | "visual" | "vline" | "diagram" | "form";

export interface Host extends HostRail {
  spec: HostSpec;
  version: string | null;
  open: Set<string>;
}

const key = (host: number, id: string) => `${host}\u0000${id}`;

export class App {
  hosts: Host[] = [];
  cwd = "";
  models: string[] = [];
  effort: Effort = "high";
  version = "";
  connected = false;

  /** Sessions this page has open (subscribed) or opened read-only, by host and id. */
  views = new Map<string, SessionView>();
  /** Read-only traces loaded for previewing, with their trace's age when loaded. */
  previews = new Map<string, { modified: number | null; view: SessionView }>();
  previewRequested = new Set<string>();
  cur: SessionView = placeholder("connecting…");
  /** The next matching `opened`/`trace` becomes the session on screen. */
  pendingOpen: { host: number; id: string | null } | null = null;

  railOpen = true;
  railFocus = false;
  railSel: { host: number; id: string; header: boolean } | null = null;
  showStale = false;
  railRows: RailRow[] = [];
  railHidden = 0;

  mode: Mode = "normal";
  count: number | null = null;
  pending: string | null = null;
  minibuf = "";
  searchFwd = true;
  lastSearch: string | null = null;
  flash: string | null = null;
  flashErr = false;
  spin = 0;
  help = false;
  vAnchor: { row: number; col: number } | null = null;
  diagram: Diagram | null = null;
  history: string[] = [];
  histPos: number | null = null;

  /** Laid-out rows of the session on screen, and the width they were laid out for. */
  rows: Row[] = [];
  cols = 80;

  private out: (m: ClientMsg) => void;
  /** Called when something changed that needs drawing. */
  redraw: () => void;
  /** Reads and writes the prompt box (the DOM owns its text). */
  prompt: { get(): string; set(s: string): void };

  constructor(out: (m: ClientMsg) => void, redraw: () => void, prompt: { get(): string; set(s: string): void }) {
    this.out = out;
    this.redraw = redraw;
    this.prompt = prompt;
  }

  // ------------------------------------------------------------------ requests

  request(host: number, request: Request) {
    const h = this.hosts[host];
    if (!h || !h.connected || !this.connected) {
      this.setFlash(`not connected to ${h?.name ?? "that host"}`, true);
      return false;
    }
    this.out({ type: "request", host, request });
    return true;
  }

  setFlash(s: string | null, err = false) {
    this.flash = s;
    this.flashErr = err;
  }

  hooks(host: number): Hooks {
    return {
      subStarted: (id, parent, title) => {
        this.hosts[host]?.live.set(id, { parent, title, state: "running", finished: null });
      },
      subState: (id, state) => {
        const s = this.hosts[host]?.live.get(id);
        if (s && s.state !== state) {
          s.state = state;
          s.finished = Date.now();
        }
      },
    };
  }

  // ------------------------------------------------------------------ server messages

  onServer(m: ServerMsg) {
    switch (m.type) {
      case "welcome": {
        const reconnect = this.hosts.length > 0;
        const was = this.cur.id ? { host: this.hosts[this.cur.host]?.name, id: this.cur.id, readOnly: this.cur.readOnly } : null;
        this.connected = true;
        this.version = m.version;
        this.cwd = m.cwd;
        this.models = m.models;
        if (!reconnect) this.effort = m.effort;
        this.clearQueues();
        this.hosts = m.hosts.map((spec) => this.newHost(spec));
        this.views.clear();
        this.previews.clear();
        this.previewRequested.clear();
        // Back to the session on screen (after a reconnect) or the one in the URL; else a new one.
        const want = was ?? parseHash();
        const host = want ? this.hosts.findIndex((h) => h.name === want.host) : -1;
        if (want && host >= 0) {
          this.pendingOpen = { host, id: want.id };
          this.queueOpen(host, want.id, !!(want as { readOnly?: boolean }).readOnly);
        } else {
          this.pendingOpen = { host: 0, id: null };
          this.queueNew(0);
        }
        if (reconnect) this.setFlash("reconnected");
        break;
      }
      case "host_added": {
        const i = this.hosts.length;
        this.hosts.push(this.newHost(m.host));
        this.setFlash(`added ${m.host.name} — starting a session…`);
        this.pendingOpen = { host: i, id: null };
        this.queueNew(i);
        break;
      }
      case "notice":
        this.onNotice(m.host, m.notice);
        break;
      case "disconnected": {
        const h = this.hosts[m.host];
        if (!h) break;
        h.connected = false;
        h.running.clear();
        for (const v of this.views.values()) if (v.host === m.host) v.running = false;
        this.clearQueues(m.host);
        this.setFlash(m.reason ? `${h.name}: ${m.reason}` : `lost the connection to ${h.name} — reconnecting…`, true);
        break;
      }
      case "error":
        this.setFlash(m.message, true);
        break;
    }
    this.redraw();
  }

  clearQueues(host?: number) {
    // A hub may reuse turn IDs after restarting. Keep the text, but not the edit target.
    for (const v of [this.cur, ...this.views.values()]) {
      if (host !== undefined && v.host !== host) continue;
      v.queued = [];
      v.queueEdit = null;
    }
  }

  private newHost(spec: HostSpec): Host {
    return {
      spec, name: spec.name, connected: false, version: null, recorded: [], open: new Set(), running: new Set(), live: new Map<string, LiveSub>(),
      filter: spec.dir ?? (spec.local ? this.cwd : null),
    };
  }

  /** Requests made before the host's connection is up wait for its `hello`. */
  private queued: { host: number; req: Request }[] = [];
  private queueNew(host: number) {
    this.queued.push({ host, req: { type: "new", cwd: this.newSessionDir(host) } });
  }
  private queueOpen(host: number, id: string, readOnly: boolean) {
    this.queued.push({ host, req: readOnly ? { type: "read_trace", session_id: id } : { type: "open", session_id: id } });
  }

  newSessionDir(host: number): string | null {
    const h = this.hosts[host];
    return h?.spec.dir ?? (h?.spec.local ? this.cwd : null);
  }

  onNotice(host: number, n: Notice) {
    const h = this.hosts[host];
    if (!h) return;
    switch (n.type) {
      case "hello": {
        // A hello from a host seen before is a reconnect (its hub restarted): reopen its
        // sessions so they keep updating.
        const reconnected = h.version !== null && !h.connected;
        h.version = n.version;
        h.connected = true;
        this.request(host, { type: "watch_queue" });
        if (reconnected) {
          for (const v of this.views.values()) {
            if (v.host === host && !v.readOnly && v.id) this.request(host, { type: "open", session_id: v.id });
          }
          this.setFlash(`reconnected to ${h.name}`);
        }
        const waiting = this.queued.filter((q) => q.host === host);
        this.queued = this.queued.filter((q) => q.host !== host);
        for (const q of waiting) this.request(host, q.req);
        break;
      }
      case "restarting":
        this.setFlash(`${h.name} is draining and will restart`);
        break;
      case "sessions": {
        const modified = new Map(n.recorded.map((s) => [s.id, millis(s.modified)]));
        for (const [k, p] of this.previews) {
          const [hh, id] = k.split("\u0000");
          if (Number(hh) === host && modified.get(id) !== p.modified) this.previews.delete(k);
        }
        h.recorded = n.recorded;
        h.open = new Set(n.open);
        h.running = new Set(n.running);
        break;
      }
      case "opened":
        this.install(host, n.snapshot, this.claims(host, n.snapshot.session_id), false);
        break;
      case "trace": {
        this.previewRequested.delete(key(host, n.session_id));
        const snap: Snapshot = { session_id: n.session_id, model: "trace", cwd: n.cwd, history: n.history, turn: [], running: false, usage: { prompt_tokens: 0, completion_tokens: 0 } };
        if (this.claims(host, n.session_id)) {
          this.previews.delete(key(host, n.session_id));
          this.install(host, snap, true, true);
        } else {
          const view = SessionView.fromSnapshot(host, snap, this.hooks(host));
          view.readOnly = true;
          this.previews.set(key(host, n.session_id), { modified: this.recordedModified(host, n.session_id), view });
        }
        break;
      }
      case "queue": {
        const v = this.views.get(key(host, n.session_id));
        if (v) v.queued = n.queued;
        break;
      }
      case "queue_edited": {
        const v = this.views.get(key(host, n.session_id));
        if (v) this.onQueueEdited(v, n.turn_id, n.updated);
        break;
      }
      case "event": {
        const v = this.views.get(key(host, n.session_id));
        if (v) v.apply(n.event, this.hooks(host));
        break;
      }
      case "path_completions":
        this.onPathCompletions(host, n.request_id, n.paths);
        break;
      case "resolved_paths":
        this.onResolvedPaths(host, n.request_id, n.cwd, n.found);
        break;
      case "file": {
        const want = this.pendingFile;
        if (want && want.host === host) {
          this.pendingFile = null;
          this.file = new FileView(host, want.cwd, n.file, want.line);
          this.mode = "normal";
        } else if (this.file && this.file.host === host) {
          // A reload, or the base switched.
          this.file.replace(n.file);
          if (this.file.base === "default" && !n.file.base) {
            this.file.base = "head";
            this.setFlash("no default branch (origin/HEAD, origin/main, origin/master); showing HEAD");
          }
        }
        break;
      }
      case "changed_files":
        this.setFlash("the changed-files list is TUI only for now (F)");
        break;
      case "error":
        this.pendingOpen = null;
        this.setFlash(this.hosts.length > 1 ? `${h.name}: ${n.message}` : n.message, true);
        break;
    }
  }

  private claims(host: number, id: string): boolean {
    const p = this.pendingOpen;
    const hit = !!p && p.host === host && (p.id == null || id.startsWith(p.id) || p.id === "last");
    if (hit) this.pendingOpen = null;
    return hit;
  }

  private recordedModified(host: number, id: string): number | null {
    const s = this.hosts[host]?.recorded.find((s: SessionInfo) => s.id === id);
    return s ? millis(s.modified) : null;
  }

  private install(host: number, snap: Snapshot, makeCurrent: boolean, readOnly: boolean) {
    const k = key(host, snap.session_id);
    const old = this.views.get(k);
    const fresh = SessionView.fromSnapshot(host, snap, this.hooks(host));
    fresh.readOnly = readOnly;
    const previous = old ?? (this.cur.host === host && this.cur.id === snap.session_id ? this.cur : undefined);
    if (previous) {
      // Keep the draft and position of a view being refreshed, including after reconnect.
      fresh.draft = previous === this.cur ? this.prompt.get() : previous.draft;
      fresh.cur = previous.cur;
      fresh.hasInput ||= previous.hasInput;
      if (!readOnly) fresh.queueEdit = previous.queueEdit;
    }
    this.views.set(k, fresh);
    if (makeCurrent || old === this.cur) this.show(fresh);
  }

  /** Puts a view on screen, keeping each view's own prompt draft. */
  show(v: SessionView) {
    if (this.cur !== v) {
      this.cur.draft = this.prompt.get();
      this.prompt.set(v.draft);
    }
    this.cur = v;
    this.railFocus = false;
    this.railSel = { host: v.host, id: v.id, header: false };
    if (this.mode !== "insert") this.mode = "normal";
    const name = this.hosts[v.host]?.name ?? "local";
    const hash = `#${encodeURIComponent(name)}/${encodeURIComponent(v.id)}${v.readOnly ? "/trace" : ""}`;
    if (typeof location !== "undefined" && location.hash !== hash) history.replaceState(null, "", hash);
  }

  // ------------------------------------------------------------------ rail

  refreshRail() {
    const keep = (r: RailRow) =>
      (r.host === this.cur.host && r.id === this.cur.id) || (!!this.railSel && r.host === this.railSel.host && r.id === this.railSel.id);
    const { rows, hidden } = rail(this.hosts, this.showStale, Date.now(), keep);
    this.railRows = rows;
    this.railHidden = hidden;
  }

  railIndex(): number {
    const s = this.railSel;
    if (!s) return Math.max(0, this.railRows.findIndex((r) => r.host === this.cur.host && r.id === this.cur.id));
    const i = this.railRows.findIndex((r) => r.host === s.host && r.header === s.header && r.id === s.id);
    return i >= 0 ? i : Math.max(0, this.railRows.findIndex((r) => r.host === this.cur.host && r.id === this.cur.id));
  }

  selectRail(i: number) {
    const r = this.railRows[Math.max(0, Math.min(i, this.railRows.length - 1))];
    if (r) this.railSel = { host: r.host, id: r.id, header: r.header };
    this.requestPreview();
  }

  focusRail(on: boolean) {
    if (on) {
      this.railOpen = true;
      if (!this.railFocus) this.railSel = { host: this.cur.host, id: this.cur.id, header: false };
      this.mode = "normal";
    }
    this.railFocus = on && this.railOpen;
    this.requestPreview();
  }

  /** The view the main pane shows: the rail's selection while the rail has focus, else `cur`. */
  shown(): SessionView {
    const s = this.railSel;
    if (!this.railFocus || !s || s.header || (s.host === this.cur.host && s.id === this.cur.id)) return this.cur;
    const k = key(s.host, s.id);
    return this.views.get(k) ?? this.previews.get(k)?.view ?? placeholder(`loading ${s.id}…`, s.host);
  }

  previewing(): boolean {
    return this.shown() !== this.cur;
  }

  private requestPreview() {
    const s = this.railSel;
    if (!this.railFocus || !s || s.header || (s.host === this.cur.host && s.id === this.cur.id)) return;
    const k = key(s.host, s.id);
    if (this.views.has(k) || this.previews.has(k) || this.previewRequested.has(k)) return;
    this.previewRequested.add(k);
    this.request(s.host, { type: "read_trace", session_id: s.id });
  }

  /** Sessions left by opening a subagent from the chat; Ctrl-o (or q in the trace) returns. */
  private back: SessionView[] = [];

  /** Enter on a `task` in the chat: the subagent's session, read-only like its rail row. */
  openSubagent(id: string) {
    const host = this.cur.host;
    const k = key(host, id);
    const from = this.cur;
    const ready = this.views.get(k) ?? this.previews.get(k)?.view;
    if (ready) {
      this.previews.delete(k);
      this.views.set(k, ready);
      this.show(ready);
    } else {
      this.pendingOpen = { host, id };
      this.setFlash(`loading subagent ${id}… (Ctrl-o returns)`);
      this.previewRequested.add(k);
      this.request(host, { type: "read_trace", session_id: id });
    }
    if (this.back[this.back.length - 1] !== from) this.back.push(from);
  }

  /** Back to the session a subagent was opened from. */
  goBack(): boolean {
    const v = this.back.pop();
    if (!v) return false;
    this.show(v);
    return true;
  }

  /** Enter on a rail row: show it, resuming it on its host if needed. */
  openSelected() {
    const r = this.railRows[this.railIndex()];
    this.railFocus = false;
    this.mode = "normal";
    if (!r) return;
    if (r.header) {
      this.setFlash(`starting a session on ${this.hosts[r.host].name}…`);
      return this.newSession(r.host);
    }
    if (r.host === this.cur.host && r.id === this.cur.id) return;
    const k = key(r.host, r.id);
    const open = this.views.get(k);
    if (open) return this.show(open);
    const pre = this.previews.get(k);
    if (r.depth > 0 && pre) {
      this.previews.delete(k);
      this.views.set(k, pre.view);
      return this.show(pre.view);
    }
    this.pendingOpen = { host: r.host, id: r.id };
    if (r.depth === 0) {
      this.setFlash(`opening ${r.id}…`);
      this.request(r.host, { type: "open", session_id: r.id });
    } else {
      this.setFlash(`loading subagent trace ${r.id}…`);
      if (!this.previewRequested.has(k)) {
        this.previewRequested.add(k);
        this.request(r.host, { type: "read_trace", session_id: r.id });
      }
    }
  }

  // ------------------------------------------------------------------ actions

  newSession(host = this.cur.host) {
    this.pendingOpen = { host, id: null };
    this.request(host, { type: "new", cwd: this.newSessionDir(host) });
  }

  /** Script errors of diagrams, passed to the model with the next message so it can fix them. */
  diagramErrors: () => { title: string; message: string }[] = () => [];

  /** Sends the answer to the `ask` waiting in the session on screen. */
  answer(answer: Answer) {
    const v = this.cur;
    if (!v.question) return;
    this.request(v.host, { type: "answer", session_id: v.id, question_id: v.question.callId, answer });
    this.setFlash(answer.type === "dismissed" ? "form dismissed" : "answer sent");
  }

  /** Browse queued turns before history, without replacing a draft or modified recall. */
  recallQueued(older: boolean): boolean {
    const v = this.cur;
    if (v.readOnly) return false;
    const edit = v.queueEdit;
    let i: number;
    if (edit) {
      if (edit.submitted !== null || this.prompt.get() !== edit.original) return false;
      const at = v.queued.findIndex((t) => t.id === edit.id);
      if (older && at === -1) return true;
      i = older ? Math.max(0, at - 1) : at + 1;
      if (i === v.queued.length) {
        this.discardQueueEdit();
        return true;
      }
    } else {
      if (!older || this.prompt.get() || !v.queued.length) return false;
      i = v.queued.length - 1;
    }
    const turn = v.queued[i];
    if (edit?.id !== turn.id) {
      this.histPos = null;
      v.queueEdit = { id: turn.id, original: turn.text, submitted: null };
      this.prompt.set(turn.text);
    }
    return true;
  }

  discardQueueEdit(): boolean {
    const v = this.cur;
    if (!v.queueEdit) return false;
    if (v.queueEdit.submitted !== null) {
      this.setFlash("edit already sent — waiting for the hub");
    } else {
      v.queueEdit = null;
      this.prompt.set("");
    }
    return true;
  }

  private onQueueEdited(v: SessionView, id: number, updated: boolean) {
    const edit = v.queueEdit;
    if (!edit || edit.id !== id || edit.submitted === null) return;
    v.queueEdit = null;
    if (!updated) {
      const msg = "queued turn already started — edit kept as an unsent draft";
      this.setFlash(v === this.cur ? msg : `${v.id}: ${msg}`, true);
    } else {
      if (edit.now && v.running) this.runQueuedNow(v, v.queued.length);
      const text = v === this.cur ? this.prompt.get() : v.draft;
      if (text === edit.submitted) {
        v.draft = "";
        if (v === this.cur) this.prompt.set("");
      }
    }
  }

  submit() {
    const v = this.cur;
    let text = this.prompt.get().trim();
    if (v.queueEdit) {
      const edit = v.queueEdit;
      if (edit.submitted !== null) return this.setFlash("edit already sent — waiting for the hub");
      if (!text) return this.setFlash("queued turn cannot be empty — Esc discards the edit", true);
      // This is a replacement, even if the current turn opened a form in the meantime.
      text = this.prompt.get();
      if (this.request(v.host, { type: "edit_queued", session_id: v.id, turn_id: edit.id, text })) edit.submitted = text;
      return;
    }
    if (!text) return;
    if (v.question && !v.readOnly) {
      // The agent is waiting on a form: a typed message answers it in words.
      this.prompt.set("");
      v.draft = "";
      if (this.history[this.history.length - 1] !== text) this.history.push(text);
      this.histPos = null;
      return this.answer({ type: "text", text });
    }
    if (v.readOnly || !v.id) {
      this.setFlash(v.readOnly ? "read-only trace — open a session to send" : "no session yet", true);
      return;
    }
    this.prompt.set("");
    v.draft = "";
    v.hasInput = true;
    if (this.history[this.history.length - 1] !== text) this.history.push(text);
    this.histPos = null;
    // The hub queues turns; the user entry appears when the turn starts.
    if (v.running) this.setFlash("queued — sends when the current turn finishes");
    const errors = this.diagramErrors();
    if (errors.length) {
      text += `\n\n<diagram_errors>\n${errors.map((e) => `"${e.title}": ${e.message}`).join("\n")}\n</diagram_errors>`;
    }
    const model = v.model && v.model !== "trace" ? v.model : undefined;
    this.request(v.host, { type: "send", session_id: v.id, text, effort: this.effort, model, diagrams: true });
  }

  /** Tab completion in flight: its request id, where it was asked, and how to put a path in. */
  private completion: { id: number; host: number; session: string; token: string; apply: (path: string) => boolean } | null = null;
  private completionIds = 0;
  // ------------------------------------------------------------------ files and links

  /** The file open over the chat, if any. */
  file: FileView | null = null;
  private pendingFile: { host: number; cwd: string; line?: number } | null = null;

  /** The cursor of what the main pane shows: the open file, else the session. */
  cursor() {
    return this.file?.cur ?? this.shown().cur;
  }

  /** Opens a link: a file from the chat in the file view, anything else in a new tab. */
  openHref(href: string) {
    const f = parseFileHref(href);
    if (f) this.openFile(f.path, f.line);
    else {
      this.openUrl(href);
      this.setFlash(`opened ${href}`);
    }
  }

  /** Asks the session's host for `path` with its diff; the view opens when it arrives. */
  openFile(path: string, line?: number) {
    const v = this.shown();
    if (this.request(v.host, { type: "read_file", cwd: v.cwd, path })) this.pendingFile = { host: v.host, cwd: v.cwd, line };
  }

  closeFile() {
    this.file = null;
  }

  /** Re-reads the open file (against its current base). */
  reloadFile() {
    const f = this.file;
    if (f) this.request(f.host, { type: "read_file", cwd: f.cwd, path: f.file.path, base: f.base });
  }

  /** Shift-Tab in a file: its uncommitted changes, or the branch's against the default branch. */
  toggleFileBase() {
    const f = this.file;
    if (!f) return;
    f.base = f.base === "head" ? "default" : "head";
    this.setFlash(f.base === "head" ? "diff against HEAD" : "diff against the default branch");
    this.reloadFile();
  }

  /** Files on each host and directory: absolute path, null for "not a file", absent if unknown. */
  private pathCache = new Map<string, Map<string, string | null>>();
  /** Paths layout asked about that have not been sent yet, per host and directory. */
  private pathWanted = new Map<string, Set<string>>();
  private pathRequests = new Map<number, { host: number; cwd: string; paths: string[] }>();
  private pathIds = 0;
  private pathEpoch = 0;

  private pathLinks(host: number, cwd: string): PathLinks | undefined {
    if (!cwd) return;
    const k = key(host, cwd);
    let known = this.pathCache.get(k);
    if (!known) this.pathCache.set(k, (known = new Map()));
    const cache = known;
    return {
      epoch: this.pathEpoch,
      lookup: (p) => {
        const hit = cache.get(p);
        if (hit !== undefined) return hit;
        let want = this.pathWanted.get(k);
        if (!want) this.pathWanted.set(k, (want = new Set()));
        want.add(p);
        return undefined;
      },
    };
  }

  /** Sends what the last layout asked about; answers bump the epoch and redraw. */
  private flushPaths() {
    for (const [k, want] of this.pathWanted) {
      const [hostStr, cwd] = k.split("\u0000");
      const host = Number(hostStr);
      const cache = this.pathCache.get(k)!;
      const paths = [...want].slice(0, 200);
      this.pathWanted.delete(k);
      if (!this.hosts[host]?.connected) continue;
      const id = ++this.pathIds;
      for (const p of paths) cache.set(p, null); // asked; a file if the answer says so
      this.pathRequests.set(id, { host, cwd, paths });
      this.out({ type: "request", host, request: { type: "resolve_paths", cwd, request_id: id, paths } });
    }
  }

  private onResolvedPaths(host: number, id: number, cwd: string, found: [string, string][]) {
    const req = this.pathRequests.get(id);
    if (!req || req.host !== host) return;
    this.pathRequests.delete(id);
    const cache = this.pathCache.get(key(host, cwd));
    if (!cache || !found.length) return;
    for (const [p, abs] of found) cache.set(p, abs);
    this.pathEpoch++;
  }

  /** Opens a link in a new tab (`gx`); tests stub it. */
  openUrl: (url: string) => void = (url) => { window.open(url, "_blank", "noopener"); };
  /** Offers several completions; the page shows a menu (tests can stub it). */
  choosePath: (paths: string[], pick: (path: string) => void) => void = () => {};

  /**
   * Asks the session's host to complete `token`, a path relative to the session's directory.
   * `apply` replaces the token in the prompt and returns false when the prompt changed since.
   */
  completePath(token: string, apply: (path: string) => boolean): boolean {
    const v = this.cur;
    if (v.readOnly || !v.id) return false;
    const id = ++this.completionIds;
    if (this.request(v.host, { type: "complete_path", session_id: v.id, request_id: id, prefix: token })) {
      this.completion = { id, host: v.host, session: v.id, token, apply };
    }
    return true;
  }

  private onPathCompletions(host: number, id: number, paths: string[]) {
    const c = this.completion;
    if (!c || c.id !== id || c.host !== host) return;
    this.completion = null;
    if (this.cur.host !== c.host || this.cur.id !== c.session) return;
    if (!paths.length) return this.setFlash(c.token ? `no paths start with ${c.token}` : "no files here");
    if (paths.length === 1) {
      c.apply(paths[0]);
      return;
    }
    // Fill in what every match shares, then offer the matches.
    const common = paths.reduce((p, s) => {
      let i = 0;
      while (i < p.length && p[i] === s[i]) i++;
      return p.slice(0, i);
    });
    if (common.length > c.token.length && !c.apply(common)) return;
    this.choosePath(paths, (p) => { c.apply(p); this.redraw(); });
  }

  /**
   * Ctrl-Enter: send the prompt (or save the queued edit), then start every waiting message at
   * once — the hub merges them and interrupts the running turn. A form answer is not hurried.
   */
  sendNow() {
    const v = this.cur;
    if (v.readOnly || !v.id) return this.submit();
    if (v.queueEdit) {
      const edit = v.queueEdit;
      this.submit();
      if (edit.submitted !== null) edit.now = true;
      return;
    }
    const typed = !!this.prompt.get().trim();
    if (!typed && !v.queued.length) return this.setFlash("nothing to send — type with i");
    const answering = typed && !!v.question;
    if (typed) this.submit();
    if (v.running && !answering) this.runQueuedNow(v, v.queued.length + (typed ? 1 : 0));
  }

  /** `n`: messages that will start, for the flash. */
  private runQueuedNow(v: SessionView, n: number) {
    if (!this.request(v.host, { type: "run_queued_now", session_id: v.id })) return;
    if (v === this.cur) this.setFlash(n > 1 ? `interrupting — the ${n} waiting messages start now, as one` : "interrupting — the waiting message starts now");
  }

  cancel(): boolean {
    if (!this.cur.running) return false;
    this.request(this.cur.host, { type: "cancel", session_id: this.cur.id });
    this.setFlash("interrupting…");
    return true;
  }

  cycleEffort() {
    this.effort = EFFORTS[(EFFORTS.indexOf(this.effort) + 1) % EFFORTS.length];
    this.setFlash(`thinking ${this.effort}`);
  }

  runCommand(cmd: string) {
    const [name, ...rest] = cmd.trim().split(/\s+/);
    const arg = rest.join(" ");
    switch (name) {
      case "": return;
      case "help": case "h": this.help = true; return;
      case "new": this.newSession(); return;
      case "q": case "q!": case "qa": case "quit": this.setFlash("close the tab to leave; sessions keep running in the hub"); return;
      case "model": {
        if (arg) this.cur.model = arg;
        else if (this.models.length) this.cur.model = this.models[(this.models.indexOf(this.cur.model) + 1) % this.models.length];
        else return this.setFlash("no models configured; :model <id> names one", true);
        return this.setFlash(`model ${this.cur.model} from the next turn`);
      }
      case "host":
        if (!arg) return this.setFlash(":host user@box[:/dir]", true);
        this.out({ type: "add_host", spec: arg });
        return this.setFlash(`connecting to ${arg}…`);
      case "restart":
        this.request(this.cur.host, { type: "restart" });
        return;
      case "rail": this.railOpen = !this.railOpen; return;
      case "e": case "files": this.setFlash("file views come in the next step"); return;
    }
    this.setFlash(`unknown command: ${name}`, true);
  }

  // ------------------------------------------------------------------ layout

  relayout(cols: number) {
    this.cols = cols;
    if (this.file) {
      this.rows = this.file.rows(Math.max(10, cols - 3));
      return;
    }
    const v = this.shown();
    this.rows = layout(v.entries, Math.max(10, cols - 3), this.spin, this.pathLinks(v.host, v.cwd));
    this.flushPaths();
  }

  busy(): boolean {
    for (const v of this.views.values()) if (v.running) return true;
    return this.hosts.some((h) => h.running.size > 0 || [...h.live.values()].some((l) => l.state === "running"));
  }
}

function placeholder(text: string, host = 0): SessionView {
  const v = new SessionView(host, "", "", "");
  v.readOnly = true;
  v.push("info", text);
  return v;
}

function parseHash(): { host: string; id: string; readOnly: boolean } | null {
  const m = location.hash.match(/^#([^/]+)\/([^/]+)(\/trace)?$/);
  return m ? { host: decodeURIComponent(m[1]), id: decodeURIComponent(m[2]), readOnly: !!m[3] } : null;
}
