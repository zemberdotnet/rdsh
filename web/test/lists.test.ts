import assert from "node:assert/strict";
import { test } from "node:test";
import { App } from "../src/app.ts";
import { Keys } from "../src/keys.ts";
import type { FileDiff, Notice, Request } from "../src/proto.ts";

function setup() {
  const requests: Request[] = [];
  const app = new App((m) => { if (m.type === "request") requests.push(m.request); }, () => {}, { get: () => "", set() {} });
  const host = { local: true, name: "local", dir: null };
  app.hosts = [{ spec: host, name: "local", connected: false, version: null, open: new Set(), recorded: [], running: new Set(), live: new Map(), filter: null }];
  app.connected = true;
  app.models = ["deepseek-flash", "gpt-6.2-sol", "gpt-6-sol"];
  const notice = (n: Notice) => app.onNotice(0, n);
  notice({ type: "hello", version: "0.2.0" });
  notice({ type: "opened", snapshot: {
    session_id: "s1", model: "deepseek-flash", cwd: "/p", turn: [], running: false, usage: { prompt_tokens: 0, completion_tokens: 0 }, history: [],
  } });
  app.cur = [...app.views.values()][0];
  requests.splice(0);
  const keys = new Keys(app, { visibleRows: () => 20, form: { open: false } } as never, null!);
  const press = (k: string) => keys.handle({ key: k, ctrlKey: false, shiftKey: false, altKey: false });
  return { app, requests, notice, press };
}

test("F lists changed files; Enter opens one and q comes back to the list", () => {
  const { app, requests, notice, press } = setup();
  press("F");
  assert.deepEqual(requests.splice(0), [{ type: "changed_files", cwd: "/p" }]);
  notice({ type: "changed_files", cwd: "/p", files: [["M", "src/a.rs"], ["??", "notes.md"]] });
  app.relayout(80);
  assert.match(app.rows[0].text, /Changed files — 2/);
  assert.deepEqual(app.rows.slice(2).map((r) => r.text.trim().replace(/\s+/g, " ")), ["src/a.rs M", "notes.md ??"]);
  press("j");
  press("Enter");
  assert.deepEqual(requests.splice(0), [{ type: "read_file", cwd: "/p", path: "notes.md" }]);
  const file: FileDiff = { path: "/p/notes.md", rel: "notes.md", status: "Untracked", diff: [], full: [{ kind: "Add", text: "hi", old: null, new: 1 }], added: 1, removed: 0, error: null, head_source: null };
  notice({ type: "file", file });
  app.relayout(80);
  assert.match(app.rows[0].text, /notes\.md — untracked/);
  press("q");
  app.relayout(80);
  assert.match(app.rows[0].text, /Changed files/, "back to the list");
  press("q");
  assert.equal(app.listView, null);
  // A clean tree says so instead of opening an empty list.
  press("F");
  notice({ type: "changed_files", cwd: "/p", files: [] });
  assert.equal(app.listView, null);
  assert.equal(app.flash, "no uncommitted changes");
});

test(":model opens a picker on the current model; Enter picks for the next turn", () => {
  const { app } = setup();
  app.runCommand("model");
  app.relayout(80);
  assert.match(app.rows[0].text, /Model/);
  assert.match(app.rows[2].text, /deepseek-flash\s+current/);
  assert.equal(app.listView?.sel, 0);
  const keys = new Keys(app, { visibleRows: () => 20, form: { open: false } } as never, null!);
  keys.handle({ key: "j", ctrlKey: false, shiftKey: false, altKey: false });
  keys.handle({ key: "Enter", ctrlKey: false, shiftKey: false, altKey: false });
  assert.equal(app.cur.model, "gpt-6.2-sol");
  assert.equal(app.listView, null);
  assert.equal(app.flash, "model gpt-6.2-sol from the next turn");
  // Naming one still works, and an unlisted model shows in the picker.
  app.runCommand("model qwen3");
  app.runCommand("model");
  assert.equal(app.listView?.items[app.listView.sel].label, "qwen3");
});
