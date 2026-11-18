import assert from "node:assert/strict";
import { test } from "node:test";
import { App } from "../src/app.ts";
import { Keys } from "../src/keys.ts";
import type { Notice, Request, Snapshot } from "../src/proto.ts";

const queued = [{ id: 1, text: "older" }, { id: 2, text: "first line\nlast line" }];
const snapshot: Snapshot = {
  session_id: "s1", model: "m", cwd: "/p", history: [], turn: [], running: true,
  usage: { prompt_tokens: 0, completion_tokens: 0 }, queued,
};

function setup() {
  const requests: Request[] = [];
  const input = {
    text: "", selectionStart: 0, selectionEnd: 0, blur() {},
    get value() { return this.text; },
    set value(s: string) { this.text = s; this.selectionStart = this.selectionEnd = s.length; },
  };
  const app = new App((m) => { if (m.type === "request") requests.push(m.request); }, () => {}, {
    get: () => input.value, set: (s) => { input.value = s; },
  });
  const host = { local: true, name: "local", dir: null };
  app.hosts = [{ spec: host, name: host.name, connected: false, version: null, open: new Set(), recorded: [], running: new Set(), live: new Map(), filter: null }];
  app.connected = true;
  const notice = (n: Notice) => app.onNotice(0, n);
  notice({ type: "hello", version: "0.2.0" });
  assert.deepEqual(requests.splice(0), [{ type: "watch_queue" }]);
  notice({ type: "opened", snapshot });
  const view = [...app.views.values()][0];
  assert.deepEqual(view.queued, queued);
  app.cur = view;
  const keys = new Keys(app, null!, input as unknown as HTMLTextAreaElement);
  const press = (key: string) => keys.insertKey({ key } as KeyboardEvent);
  return { app, input, press, requests, notice };
}

test("Up recalls queued turns before history and preserves multiline cursor movement", () => {
  const { app, input, press, notice, requests } = setup();
  app.history = ["previous"];
  input.value = "a different draft";
  assert.equal(press("ArrowUp"), false);
  assert.equal(input.value, "a different draft");
  input.value = "";
  assert.equal(press("ArrowUp"), true);
  assert.equal(input.value, queued[1].text);
  assert.equal(press("ArrowUp"), false, "moves the cursor up within the recall first");
  input.selectionStart = input.selectionEnd = 0;
  assert.equal(press("ArrowUp"), true);
  assert.equal(input.value, queued[0].text);
  assert.equal(press("ArrowDown"), true);
  assert.equal(input.value, queued[1].text);
  assert.equal(press("ArrowDown"), true);
  assert.equal(input.value, "");
  press("ArrowUp");
  input.value = "discard this edit";
  assert.equal(press("ArrowUp"), false);
  press("Escape");
  assert.equal(input.value, "");
  assert.deepEqual(app.cur.queued, queued);
  assert.deepEqual(requests, []);
  notice({ type: "queue", session_id: "s1", queued: [] });
  assert.equal(press("ArrowUp"), true);
  assert.equal(input.value, "previous");
});

test("Enter updates the recalled id instead of sending a turn or an ask answer", () => {
  const { app, input, press, requests, notice } = setup();
  press("ArrowUp");
  input.value = "revised";
  app.cur.question = { callId: "ask-1", ask: { forms: [] } };
  press("Enter");
  press("Enter");
  assert.deepEqual(requests, [{ type: "edit_queued", session_id: "s1", turn_id: 2, text: "revised" }]);
  assert.equal(input.value, "revised", "keep the draft until the hub accepts it");
  notice({ type: "queue_edited", session_id: "s1", turn_id: 2, updated: true });
  assert.equal(input.value, "");
  assert.equal(app.cur.queueEdit, null);
  assert.equal(app.cur.question?.callId, "ask-1");
});

test("a late edit stays unsent and successful edits don't erase subsequent typing", () => {
  const { app, input, press, requests, notice } = setup();
  press("ArrowUp");
  input.value = "saved revision";
  press("Enter");
  input.value = "typing while saving";
  notice({ type: "queue_edited", session_id: "s1", turn_id: 2, updated: true });
  assert.equal(input.value, "typing while saving");
  input.value = "";
  press("ArrowUp");
  input.value = "too late";
  notice({ type: "queue", session_id: "s1", queued: queued.slice(0, 1) });
  press("Enter");
  notice({ type: "queue_edited", session_id: "s1", turn_id: 2, updated: false });
  assert.equal(input.value, "too late");
  assert.equal(app.cur.queueEdit, null);
  assert.match(app.flash!, /unsent draft/);
  assert.deepEqual(requests.map((r) => r.type), ["edit_queued", "edit_queued"]);
  assert.deepEqual(app.cur.queued, queued.slice(0, 1));
});

test("disconnect clears stale queue ids but not the edit text", () => {
  const { app, input, press } = setup();
  press("ArrowUp");
  input.value = "keep my words";
  app.onServer({ type: "disconnected", host: 0, reason: null });
  assert.deepEqual(app.cur.queued, []);
  assert.equal(app.cur.queueEdit, null);
  assert.equal(input.value, "keep my words");
});

test("Ctrl-Enter sends the prompt, then starts the whole queue now", () => {
  const { app, input, requests } = setup();
  const keys = new Keys(app, null!, input as unknown as HTMLTextAreaElement);
  input.value = "urgent";
  keys.insertKey({ key: "Enter", ctrlKey: true } as KeyboardEvent);
  assert.deepEqual(requests.map((r) => r.type), ["send", "run_queued_now"]);
  assert.match(app.flash ?? "", /3 waiting messages start now/);
  // Nothing typed but messages waiting: just hurry them.
  requests.splice(0);
  keys.insertKey({ key: "Enter", ctrlKey: true } as KeyboardEvent);
  assert.deepEqual(requests, [{ type: "run_queued_now", session_id: "s1" }]);
});

test("Ctrl-Enter on a queued edit waits for the hub to accept it before hurrying", () => {
  const { app, input, press, requests, notice } = setup();
  const keys = new Keys(app, null!, input as unknown as HTMLTextAreaElement);
  press("ArrowUp");
  input.value = "revised";
  keys.insertKey({ key: "Enter", ctrlKey: true } as KeyboardEvent);
  assert.deepEqual(requests.splice(0), [{ type: "edit_queued", session_id: "s1", turn_id: 2, text: "revised" }]);
  notice({ type: "queue_edited", session_id: "s1", turn_id: 2, updated: true });
  assert.deepEqual(requests, [{ type: "run_queued_now", session_id: "s1" }]);
});

test("Ctrl-Enter answering a form in words does not interrupt the turn it serves", () => {
  const { app, input, requests } = setup();
  const keys = new Keys(app, null!, input as unknown as HTMLTextAreaElement);
  app.cur.question = { callId: "ask-1", ask: { forms: [] } };
  input.value = "use the second option";
  keys.insertKey({ key: "Enter", ctrlKey: true } as KeyboardEvent);
  assert.deepEqual(requests.map((r) => r.type), ["answer"]);
});

test("Tab completes the path before the cursor, and several matches open a menu", () => {
  const { app, input, requests, notice } = setup();
  const t = Object.assign(input, {
    setRangeText(s: string, a: number, b: number) { this.text = this.text.slice(0, a) + s + this.text.slice(b); this.selectionStart = this.selectionEnd = a + s.length; },
  });
  const keys = new Keys(app, null!, t as unknown as HTMLTextAreaElement);
  const tab = () => keys.insertKey({ key: "Tab" } as KeyboardEvent);
  let offered: string[] = [];
  let pick: (p: string) => void = () => {};
  app.choosePath = (paths, p) => { offered = paths; pick = p; };

  input.value = "look at src/m";
  assert.equal(tab(), true);
  assert.deepEqual(requests.splice(0), [{ type: "complete_path", session_id: "s1", request_id: 1, prefix: "src/m" }]);
  notice({ type: "path_completions", request_id: 1, paths: ["src/main.rs"] });
  assert.equal(input.value, "look at src/main.rs");

  // Several: the shared part goes in, then the menu's pick replaces it.
  input.value = "open sr";
  tab();
  notice({ type: "path_completions", request_id: 2, paths: ["src/tui/app.rs", "src/tui/mod.rs"] });
  assert.equal(input.value, "open src/tui/");
  assert.deepEqual(offered, ["src/tui/app.rs", "src/tui/mod.rs"]);
  pick("src/tui/mod.rs");
  assert.equal(input.value, "open src/tui/mod.rs");

  // A stale answer (the user kept typing) leaves the prompt alone.
  input.value = "x/";
  tab();
  input.value = "something else";
  notice({ type: "path_completions", request_id: 3, paths: ["x/y"] });
  assert.equal(input.value, "something else");
});

test("gx opens the link under the cursor, else the row's first link", () => {
  const { app } = setup();
  const opened: string[] = [];
  app.openUrl = (u) => opened.push(u);
  const keys = new Keys(app, null!, null!);
  const row = (segs: [string, string, string?][]) => ({ segs, g: ["  ", ""] as [string, string], e: null!, text: segs.map((s) => s[0]).join("") });
  app.rows = [
    row([["see ", ""], ["docs", "link", "https://a.dev/docs"], [" or ", ""], ["more", "link", "https://b.dev/"]]),
    row([["plain text", ""]]),
  ];
  const at = (r: number, c: number) => { app.cur.cur.row = r; app.cur.cur.col = c; };
  const gx = () => { keys.handle({ key: "g", ctrlKey: false, shiftKey: false, altKey: false }); keys.handle({ key: "x", ctrlKey: false, shiftKey: false, altKey: false }); };
  at(0, 12); gx();
  at(0, 0); gx();
  at(1, 3); gx();
  assert.deepEqual(opened, ["https://b.dev/", "https://a.dev/docs"]);
  assert.equal(app.flash, "no link on this line");
});
