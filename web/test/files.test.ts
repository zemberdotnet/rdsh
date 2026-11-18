import assert from "node:assert/strict";
import { test } from "node:test";
import { App } from "../src/app.ts";
import { Keys } from "../src/keys.ts";
import { layout, parseFileHref, type PathLinks } from "../src/layout.ts";
import type { FileDiff, Notice, Request, Snapshot } from "../src/proto.ts";
import { SessionView } from "../src/session.ts";

test("only paths the host confirms become file links", () => {
  const v = SessionView.fromSnapshot(0, {
    session_id: "s1", model: "m", cwd: "/p", turn: [], running: false, usage: { prompt_tokens: 0, completion_tokens: 0 },
    history: [{ role: "assistant", content: "Changed `src/a.rs:12` and something/else, see [b](src/b.rs)." }],
  } as Snapshot, { subStarted() {}, subState() {} });
  const asked: string[] = [];
  const known: Record<string, string | null> = {};
  const links: PathLinks = { epoch: 0, lookup: (p) => (p in known ? known[p] : (asked.push(p), undefined)) };
  layout(v.entries, 80, 0, links);
  assert.deepEqual(asked.sort(), ["something/else", "src/a.rs", "src/b.rs"]);
  Object.assign(known, { "src/a.rs": "/p/src/a.rs", "src/b.rs": "/p/src/b.rs", "something/else": null });
  links.epoch = 1;
  const segs = layout(v.entries, 80, 0, links).flatMap((r) => r.segs);
  const hrefs = segs.filter((s) => s[2]).map((s) => [s[0], parseFileHref(s[2]!)]);
  assert.deepEqual(hrefs, [
    ["src/a.rs:12", { path: "/p/src/a.rs", line: 12 }],
    ["b", { path: "/p/src/b.rs", line: undefined }],
  ]);
});

const diff: FileDiff = {
  path: "/p/src/a.rs", rel: "src/a.rs", status: "Modified", added: 1, removed: 1, error: null, head_source: null,
  diff: [
    { kind: "Hunk", text: "@@ -1,2 +1,2 @@", old: null, new: null },
    { kind: "Ctx", text: "fn a() {}", old: 1, new: 1 },
    { kind: "Del", text: "fn b() {}", old: 2, new: null },
    { kind: "Add", text: "fn b2() {}", old: null, new: 2 },
  ],
  full: [
    { kind: "Ctx", text: "fn a() {}", old: null, new: 1 },
    { kind: "Add", text: "fn b2() {}", old: null, new: 2 },
  ],
};

function setup() {
  const requests: Request[] = [];
  const app = new App((m) => { if (m.type === "request") requests.push(m.request); }, () => {}, { get: () => "", set() {} });
  const host = { local: true, name: "local", dir: null };
  app.hosts = [{ spec: host, name: "local", connected: false, version: null, open: new Set(), recorded: [], running: new Set(), live: new Map(), filter: null }];
  app.connected = true;
  const notice = (n: Notice) => app.onNotice(0, n);
  notice({ type: "hello", version: "0.2.0" });
  notice({ type: "opened", snapshot: {
    session_id: "s1", model: "m", cwd: "/p", turn: [], running: false, usage: { prompt_tokens: 0, completion_tokens: 0 },
    history: [{ role: "assistant", content: "I changed src/a.rs:2 and notes/todo." }],
  } });
  app.cur = [...app.views.values()][0];
  requests.splice(0);
  const keys = new Keys(app, { visibleRows: () => 20, form: { open: false } } as never, null!);
  const press = (k: string) => keys.handle({ key: k, ctrlKey: false, shiftKey: k === "S-Tab", altKey: false });
  const ctrl = (k: string) => keys.handle({ key: k, ctrlKey: true, shiftKey: false, altKey: false });
  return { app, requests, notice, press, ctrl };
}

test("the chat asks the host about paths, links the files, and Enter opens one", () => {
  const { app, requests, notice, press } = setup();
  app.relayout(80);
  assert.deepEqual(requests.splice(0), [{ type: "resolve_paths", cwd: "/p", request_id: 1, paths: ["src/a.rs", "notes/todo"] }]);
  notice({ type: "resolved_paths", request_id: 1, cwd: "/p", found: [["src/a.rs", "/p/src/a.rs"]] });
  app.relayout(80);
  assert.deepEqual(requests, [], "answers are cached; nothing is asked twice");
  const row = app.rows.findIndex((r) => r.text.includes("src/a.rs"));
  const c = app.cursor();
  c.row = row;
  c.col = app.rows[row].text.indexOf("src/a.rs") + 2;
  press("Enter");
  assert.deepEqual(requests.splice(0), [{ type: "read_file", cwd: "/p", path: "/p/src/a.rs" }]);

  notice({ type: "file", file: diff });
  assert.ok(app.file);
  app.relayout(80);
  assert.match(app.rows[0].text, /src\/a\.rs — \+1 -1 — diff view/);
  assert.equal(app.rows[app.cursor().row].text.trim(), "2 fn b2() {}", "the cursor lands on the linked line");
  assert.deepEqual(app.rows.map((r) => r.bg ?? ""), ["code", "hunk", "", "del", "add"]);
  press("Tab");
  app.relayout(80);
  assert.match(app.rows[0].text, /file view/);
  press("S-Tab");
  assert.deepEqual(requests.splice(0), [{ type: "read_file", cwd: "/p", path: "/p/src/a.rs", base: "default" }]);
  press("q");
  assert.equal(app.file, null);
  app.relayout(80);
  assert.ok(app.rows.some((r) => r.text.includes("I changed")), "back to the chat");
});

test("Enter on a task opens its subagent's session, and Ctrl-o comes back", () => {
  const { app, requests, notice, press, ctrl } = setup();
  notice({ type: "opened", snapshot: {
    session_id: "s2", model: "m", cwd: "/p", turn: [], running: false, usage: { prompt_tokens: 0, completion_tokens: 0 },
    history: [
      { role: "user", content: "go" },
      { role: "assistant", content: "", tool_calls: [{ id: "c1", name: "task", arguments: JSON.stringify({ description: "survey files", prompt: "x" }) }] },
      { role: "tool", call_id: "c1", name: "task", content: "[subagent session s2-sub1]\nfound three" },
    ],
  } });
  const parent = app.views.get(`0\u0000s2`)!;
  app.show(parent);
  requests.splice(0);
  app.relayout(80);
  requests.splice(0);
  app.cursor().row = app.rows.findIndex((r) => r.text.includes("survey files"));
  press("Enter");
  assert.deepEqual(requests.splice(0), [{ type: "read_trace", session_id: "s2-sub1" }]);
  notice({ type: "trace", session_id: "s2-sub1", cwd: "/p", history: [{ role: "user", content: "x" }, { role: "assistant", content: "found three" }] });
  assert.equal(app.cur.id, "s2-sub1");
  assert.ok(app.cur.readOnly);
  press("q");
  assert.equal(app.cur.id, "s2", "q in the subagent's trace goes back");
  press("Enter");
  assert.equal(app.cur.id, "s2-sub1", "a loaded trace opens at once");
  assert.deepEqual(requests, []);
  ctrl("o");
  assert.equal(app.cur.id, "s2");
});

