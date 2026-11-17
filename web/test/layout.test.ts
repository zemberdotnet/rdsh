import assert from "node:assert/strict";
import { test } from "node:test";
import { markdown, segText, table, wrap } from "../src/layout.ts";

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

test("markdown: a closed d3 block reserves rows for a diagram; an open one shows as code", () => {
  const closed = markdown("Look:\n```d3\n// rows: 6\n// title: deps\nsvg.append('g')\n```", 40, "e1");
  const dia = closed.filter((l) => l.dia);
  assert.equal(dia.length, 6);
  assert.equal(dia[0].dia!.title, "deps");
  assert.equal(dia[0].dia!.key, "e1#0");
  const open = markdown("Look:\n```d3\nsvg.append('g')", 40, "e1");
  assert.ok(!open.some((l) => l.dia));
  assert.ok(texts(open).includes(" svg.append('g')"));
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
