import assert from "node:assert/strict";
import { test } from "node:test";
import { layout, markdown, segText, table, wrap } from "../src/layout.ts";
import { SessionView } from "../src/session.ts";

const texts = (lines: { segs: [string, string][] }[]) => lines.map((l) => segText(l.segs));

test("wrap breaks at words, hangs the indent, and splits long words", () => {
  assert.deepEqual(texts(wrap([["the quick brown fox jumps", ""]], 10)), ["the quick", "brown fox", "jumps"]);
  assert.deepEqual(texts(wrap([["alpha beta gamma", ""]], 10, "• ")), ["• alpha", "  beta", "  gamma"]);
  assert.deepEqual(texts(wrap([["abcdefghijkl", ""]], 5)), ["abcde", "fghij", "kl"]);
  assert.deepEqual(texts(wrap([["", ""]], 10)), [""]);
});

test("markdown: headings, lists, code, inline styles", () => {
  const lines = markdown("# Title\n\nSome `code` and **bold**.\n\n- one\n- two\n\n```rust\nlet x = 1;\n```", 40);
  assert.deepEqual(texts(lines), ["Title", "", "Some code and bold.", "", "• one", "• two", "", " rust ", " let x = 1;"]);
  const para = lines[2].segs;
  assert.ok(para.some(([t, c]) => t === "code" && c.includes("ic")));
  assert.ok(para.some(([t, c]) => t === "bold" && c.includes("b")));
});

test("a streaming d3 block hides its source and animates until its fence closes", () => {
  const v = new SessionView(0, "s1", "/p", "m");
  const hooks = { subStarted() {}, subState() {} };
  const src = "// rows: 6\n// title: deps\nsvg.append('g')";
  v.apply({ type: "text_delta", data: `Look:\n\n\`\`\`d3\n${src}` }, hooks);
  const first = layout(v.entries, 40, 0);
  assert.deepEqual(texts(first), ["Look:", "", "◴ Generating diagram…"]);
  assert.ok(!first.some((l) => l.dia));
  assert.deepEqual(texts(layout(v.entries, 40, 1)), ["Look:", "", "◷ Generating diagram…"]);

  v.apply({ type: "text_delta", data: "\n```\n\nDone." }, hooks);
  assert.ok(v.entries[0].streaming);
  const closed = layout(v.entries, 40, 2);
  const dia = closed.filter((l) => l.dia);
  assert.equal(dia.length, 6);
  assert.equal(dia[0].dia!.title, "deps");
  assert.equal(dia[0].dia!.key, `${v.entries[0].id}#0`);
  assert.equal(dia[0].dia!.script, src);
  assert.deepEqual(texts(closed).slice(-1), ["Done."]);
  assert.ok(!texts(closed).some((text) => text.includes("Generating diagram")));
});

test("an interrupted diagram stops animating without showing its source", () => {
  const v = new SessionView(0, "s1", "/p", "m");
  const hooks = { subStarted() {}, subState() {} };
  v.apply({ type: "text_delta", data: "```d3\nsvg.append('g')" }, hooks);
  assert.deepEqual(texts(layout(v.entries, 40, 0)), ["◴ Generating diagram…"]);
  v.apply({ type: "turn_end", data: { cancelled: true, error: null } }, hooks);
  assert.deepEqual(texts(layout(v.entries, 40, 0)), ["Diagram incomplete", "", "interrupted"]);
});

test("ordinary code still shows as it streams", () => {
  const open = markdown("```js\nconst value = 42;", 40, "e1", 0);
  assert.deepEqual(texts(open), [" js ", " const value = 42;"]);
});

test("tables line up and shrink to fit", () => {
  const t = texts(table(["| a | long header |", "|---|---|", "| 1 | x |"], 80));
  assert.deepEqual(t, ["┌───┬─────────────┐", "│ a │ long header │", "├───┼─────────────┤", "│ 1 │ x           │", "└───┴─────────────┘"]);
  for (const l of texts(table(["| " + "x".repeat(50) + " | y |"], 20))) assert.ok(l.length <= 20, l);
});

test("code and links inside bold or italic still render as code and links", () => {
  const [line] = markdown("1. **`web/src/keys.ts:52`** — see *the [docs](http://x)* and **plain**", 80);
  const segs = line.segs.filter(([t]) => t.trim());
  assert.deepEqual(segs.slice(0, 2), [["1. ", "aqua"], ["web/src/keys.ts:52", "b ic"]]);
  assert.ok(segs.some(([t, c]) => t === "docs" && c === "i link"));
  assert.ok(segs.some(([t, c]) => t === "plain" && c === "b"));
  assert.ok(!segText(line.segs).includes("`"), "no stray backticks");
});
