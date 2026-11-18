import assert from "node:assert/strict";
import { test } from "node:test";
import { ChoiceMenu, choiceMenuAction } from "../src/choice-menu.ts";

type Key = Parameters<typeof choiceMenuAction>[0];
const key = (key: string, props: Partial<Key> = {}): Key => ({
  key, keyCode: 0, isComposing: false, defaultPrevented: false,
  ctrlKey: false, shiftKey: false, altKey: false, metaKey: false,
  ...props,
});

test("choice menu maps Tab, Up/Down and Ctrl-n/p to cycling", () => {
  for (const e of [key("Tab"), key("ArrowDown"), key("n", { ctrlKey: true })]) {
    assert.equal(choiceMenuAction(e), "next");
  }
  for (const e of [key("Tab", { shiftKey: true }), key("ArrowUp"), key("p", { ctrlKey: true })]) {
    assert.equal(choiceMenuAction(e), "previous");
  }
  assert.equal(choiceMenuAction(key("Enter")), "accept");
  assert.equal(choiceMenuAction(key("Escape")), "dismiss");
});

test("choice menu leaves composition, typing and unsupported shortcuts alone", () => {
  for (const e of [
    key("Enter", { isComposing: true }), key("Escape", { keyCode: 229 }), key("Process"),
    key("Enter", { defaultPrevented: true }), key("Enter", { shiftKey: true }), key("Enter", { ctrlKey: true }),
    key("ArrowUp", { shiftKey: true }), key("ArrowDown", { altKey: true }), key("ArrowLeft"), key("ArrowRight"),
    key("Tab", { ctrlKey: true }), key("Tab", { metaKey: true }), key("n"), key("p"),
    key("N", { ctrlKey: true, shiftKey: true }), key("p", { ctrlKey: true, altKey: true }),
  ]) {
    assert.equal(choiceMenuAction(e), null, JSON.stringify(e));
  }
});

test("a closed choice menu doesn't consume keys or claim event targets", () => {
  const menu = new ChoiceMenu<string>("Suggestions");
  assert.equal(menu.open, false);
  menu.position();
  menu.close();
  assert.equal(menu.handle(key("Enter") as KeyboardEvent), false);
  assert.equal(menu.has(new EventTarget()), false);
  assert.equal(menu.has(null), false);
});
