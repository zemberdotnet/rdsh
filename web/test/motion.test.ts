import assert from "node:assert/strict";
import { test } from "node:test";
import { entryJump, find, wordBack, wordEnd, wordForward } from "../src/motion.ts";

const lines = ["foo bar.baz", "", "  qux"];

test("w b e move by words across lines", () => {
  assert.deepEqual(wordForward(lines, { row: 0, col: 0 }), { row: 0, col: 4 });
  assert.deepEqual(wordForward(lines, { row: 0, col: 4 }), { row: 0, col: 7 });
  assert.deepEqual(wordForward(lines, { row: 0, col: 8 }), { row: 1, col: 0 });
  assert.deepEqual(wordForward(lines, { row: 1, col: 0 }), { row: 2, col: 2 });
  assert.deepEqual(wordBack(lines, { row: 2, col: 2 }), { row: 1, col: 0 });
  assert.deepEqual(wordBack(lines, { row: 0, col: 8 }), { row: 0, col: 7 });
  assert.deepEqual(wordEnd(lines, { row: 0, col: 0 }), { row: 0, col: 2 });
  assert.deepEqual(wordEnd(lines, { row: 0, col: 2 }), { row: 0, col: 6 });
});

test("{ } jump between entries", () => {
  const owners = ["a", "a", "b", "b", "b", "c"];
  assert.equal(entryJump(owners, 0, 1), 2);
  assert.equal(entryJump(owners, 3, 1), 5);
  assert.equal(entryJump(owners, 4, -1), 2);
  assert.equal(entryJump(owners, 2, -1), 0);
});

test("search is smart-case and wraps", () => {
  const t = ["Hub one", "the hub", "HUB"];
  assert.deepEqual(find(t, "hub", { row: 0, col: 0 }, true), { pos: { row: 1, col: 4 }, wrapped: false });
  assert.deepEqual(find(t, "hub", { row: 2, col: 0 }, true), { pos: { row: 0, col: 0 }, wrapped: true });
  assert.deepEqual(find(t, "HUB", { row: 0, col: 0 }, true), { pos: { row: 2, col: 0 }, wrapped: false });
  assert.equal(find(t, "zzz", { row: 0, col: 0 }, true), null);
});
