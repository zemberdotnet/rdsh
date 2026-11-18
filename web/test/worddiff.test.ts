import assert from "node:assert/strict";
import { test } from "node:test";
import { layout } from "../src/layout.ts";
import { SessionView } from "../src/session.ts";
import { changedRanges, diffLines, mark, pairRanges } from "../src/worddiff.ts";

test("edits diff by line, removals before additions", () => {
  const ops = diffLines("a\nold one\nold two\nz", "a\nnew one\nz");
  assert.deepEqual(ops.map((o) => `${o.kind}:${o.text}`), ["ctx:a", "del:old one", "del:old two", "add:new one", "ctx:z"]);
});

test("the changed words of a replaced line are found", () => {
  const before = "let total = items.len() + 1;";
  const after = "let total = items.count() + offset;";
  const [a, b] = changedRanges(before, after);
  assert.deepEqual(a.map(([s, e]) => before.slice(s, e)), ["len", "1"]);
  assert.deepEqual(b.map(([s, e]) => after.slice(s, e)), ["count", "offset"]);
  // A line rewritten nearly everywhere gets no second highlight.
  const ops = diffLines("alpha beta", "gamma delta");
  assert.equal(pairRanges(ops, (o) => (o.kind === "ctx" ? "other" : o.kind), (o) => o.text).size, 0);
});

test("mark splits segments at range edges and keeps links", () => {
  assert.deepEqual(mark([["fn len(", "kw"], ["x)", "", "href"]], [[3, 6], [7, 8]], "addw"), [
    ["fn ", "kw"], ["len", "kw addw"], ["(", "kw"], ["x", "addw", "href"], [")", "", "href"],
  ]);
});

test("an edit_file call is drawn as a numbered diff with changed words marked", () => {
  const args = JSON.stringify({ path: "src/a.rs", old_string: "fn a() {}\nlet n = len();", new_string: "fn a() {}\nlet n = count();" });
  const v = SessionView.fromSnapshot(0, {
    session_id: "s", model: "m", cwd: "/p", turn: [], running: false, usage: { prompt_tokens: 0, completion_tokens: 0 },
    history: [
      { role: "assistant", content: "", tool_calls: [{ id: "c1", name: "edit_file", arguments: args }] },
      { role: "tool", call_id: "c1", name: "edit_file", content: "Edited src/a.rs:7 (1 replacement)" },
    ],
  }, { subStarted() {}, subState() {} });
  const rows = layout(v.entries, 80, 0);
  const diff = rows.filter((r) => r.bg === "del" || r.bg === "add");
  assert.deepEqual(diff.map((r) => [r.g[0], r.text]), [["- ", "   8 let n = len();"], ["+ ", "   8 let n = count();"]]);
  assert.deepEqual(diff[0].segs.filter((s) => s[1].includes("delw")).map((s) => s[0]), ["len"]);
  assert.deepEqual(diff[1].segs.filter((s) => s[1].includes("addw")).map((s) => s[0]), ["count"]);
  assert.ok(rows.some((r) => r.text.includes("   7 fn a() {}")), "context keeps its number");
  assert.ok(rows.some((r) => r.text.includes("Edited src/a.rs:7")), "the tool's own output still shows");
});
