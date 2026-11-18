import assert from "node:assert/strict";
import { test } from "node:test";
import { App } from "../src/app.ts";
import { composeMessage } from "../src/comments.ts";
import { Keys } from "../src/keys.ts";
import type { FileDiff, Notice, Request } from "../src/proto.ts";

test("composed messages quote each comment in a review block", () => {
  const msg = composeMessage([
    { anchor: { kind: "chat", source: "your response", quote: "line one\nline two" }, body: "why?" },
    { anchor: { kind: "file", path: "src/a.rs", lines: [3, 4], snippet: "+let x = 1;\n+```" }, body: "rename x" },
  ], "  and run tests ");
  assert.equal(msg, [
    "<review_comments>",
    "I left comments on specific parts of the conversation and the working tree. Each quotes what it refers to. Address every comment.",
    "",
    "[1] On your response:",
    "> line one",
    "> line two",
    "Comment: why?",
    "",
    "[2] On src/a.rs:3-4 (current diff):",
    "````diff",
    "+let x = 1;",
    "+```",
    "````",
    "Comment: rename x",
    "</review_comments>",
    "",
    "and run tests",
  ].join("\n"));
  assert.equal(composeMessage([], " hi "), "hi");
});

function setup() {
  const requests: Request[] = [];
  let prompt = "";
  const app = new App((m) => { if (m.type === "request") requests.push(m.request); }, () => {}, { get: () => prompt, set: (s) => { prompt = s; } });
  const host = { local: true, name: "local", dir: null };
  app.hosts = [{ spec: host, name: "local", connected: false, version: null, open: new Set(), recorded: [], running: new Set(), live: new Map(), filter: null }];
  app.connected = true;
  const notice = (n: Notice) => app.onNotice(0, n);
  notice({ type: "hello", version: "0.2.0" });
  notice({ type: "opened", snapshot: {
    session_id: "s1", model: "m", cwd: "/p", turn: [], running: false, usage: { prompt_tokens: 0, completion_tokens: 0 },
    history: [{ role: "user", content: "start" }, { role: "assistant", content: "I renamed b to b2.\nSecond line here." }],
  } });
  app.cur = [...app.views.values()][0];
  requests.splice(0);
  const keys = new Keys(app, { visibleRows: () => 20, form: { open: false } } as never, null!);
  const press = (k: string) => keys.handle({ key: k, ctrlKey: false, shiftKey: false, altKey: false });
  const setPrompt = (s: string) => { prompt = s; };
  return { app, requests, notice, press, setPrompt };
}

test("c comments on a chat line, V selections quote several, and the next message carries them", () => {
  const { app, requests, press, setPrompt } = setup();
  app.relayout(80);
  const row = app.rows.findIndex((r) => r.text.includes("I renamed"));
  app.cursor().row = row;
  press("c");
  assert.equal(app.mode, "comment");
  assert.equal(app.commentDraft()?.title, "Comment on your response");
  app.saveComment("why rename?");
  assert.equal(app.cur.comments.length, 1);
  app.cursor().row = row;
  press("V");
  press("j");
  press("c");
  // Quotes are the text as shown: markdown joins a paragraph's lines.
  assert.equal(app.commentDraft()?.excerpt, "I renamed b to b2. Second line here.");
  app.saveComment("drop the second");
  // An empty comment is dropped.
  app.cursor().row = row;
  press("c");
  app.saveComment("   ");
  assert.equal(app.cur.comments.length, 2);

  setPrompt("and run tests");
  app.submit();
  const send = requests.find((r) => r.type === "send") as Extract<Request, { type: "send" }>;
  assert.match(send.text, /^<review_comments>/);
  assert.match(send.text, /\[1\] On your response:\n> I renamed b to b2\. Second line here\.\nComment: why rename\?/);
  assert.match(send.text, /\[2\] On your response:\n> I renamed b to b2\. Second line here\.\nComment: drop the second/);
  assert.ok(send.text.endsWith("</review_comments>\n\nand run tests"));
  assert.equal(app.cur.comments.length, 0);
});

test("comments alone can be sent, and Q reviews, edits and deletes them", () => {
  const { app, requests, press } = setup();
  app.relayout(80);
  app.cursor().row = app.rows.findIndex((r) => r.text.includes("I renamed"));
  press("c"); app.saveComment("one");
  press("c"); app.saveComment("two");
  press("Q");
  app.relayout(80);
  assert.match(app.rows[0].text, /Comment queue — 2/);
  press("j");
  press("e");
  assert.equal(app.commentDraft()?.body, "two");
  app.saveComment("two, edited");
  assert.equal(app.cur.comments[1].body, "two, edited");
  press("k");
  press("d"); press("d");
  assert.deepEqual(app.cur.comments.map((c) => c.body), ["two, edited"]);
  press("q");
  assert.equal(app.queueView, null);
  app.submit();
  const send = requests.find((r) => r.type === "send") as Extract<Request, { type: "send" }>;
  assert.match(send.text, /Comment: two, edited\n<\/review_comments>$/);
});

test("Enter in a file view comments on its lines with numbers and diff signs", () => {
  const { app, notice, press } = setup();
  const file: FileDiff = {
    path: "/p/src/a.rs", rel: "src/a.rs", status: "Modified", added: 1, removed: 1, error: null, head_source: null, full: [],
    diff: [
      { kind: "Hunk", text: "@@ -1,2 +1,2 @@", old: null, new: null },
      { kind: "Ctx", text: "fn a() {}", old: 1, new: 1 },
      { kind: "Del", text: "fn b() {}", old: 2, new: null },
      { kind: "Add", text: "fn b2() {}", old: null, new: 2 },
    ],
  };
  app.openFile("/p/src/a.rs");
  notice({ type: "file", file });
  app.relayout(80);
  const c = app.cursor();
  c.row = app.rows.findIndex((r) => r.text.includes("fn b() {}"));
  press("V"); press("j");
  press("Enter");
  assert.equal(app.commentDraft()?.title, "Comment on src/a.rs:2");
  app.saveComment("keep the old name");
  assert.deepEqual(app.cur.comments[0].anchor, { kind: "file", path: "src/a.rs", lines: [2, 2], snippet: "-fn b() {}\n+fn b2() {}" });
  app.relayout(80);
  assert.ok(app.rows.some((r) => r.g[0] === "● " && r.text.includes("fn b2")), "commented lines are marked");
});
