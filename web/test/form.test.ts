import assert from "node:assert/strict";
import { test } from "node:test";
import { collect, initialValues, problem } from "../src/form.ts";
import type { Ask } from "../src/proto.ts";
import { SessionView } from "../src/session.ts";

const ask: Ask = {
  title: "Open a PR",
  forms: [
    { fields: [
      { id: "title", label: "PR title", type: "text", default: "Fix rail" },
      { id: "body", label: "Description", type: "text", multiline: true, required: false },
    ] },
    { fields: [
      { id: "reviewers", label: "Reviewers", type: "choice", multiple: true, other: true, options: [{ label: "alice" }, { label: "bob" }], default: ["alice"] },
      { id: "draft", label: "Draft?", type: "confirm" },
      { id: "n", label: "Count", type: "number", min: 1, max: 5, required: false },
    ] },
  ],
};

test("defaults fill in, required fields are checked, empty optional ones are left out", () => {
  const v = initialValues(ask);
  assert.deepEqual(v, { title: "Fix rail", body: undefined, reviewers: ["alice"], draft: undefined, n: undefined });
  const fields = ask.forms.flatMap((f) => f.fields);
  const by = (id: string) => fields.find((f) => f.id === id)!;
  assert.equal(problem(by("draft"), undefined), "needs an answer");
  assert.equal(problem(by("body"), ""), null);
  assert.equal(problem(by("reviewers"), []), "needs an answer");
  assert.equal(problem(by("n"), "9"), "at most 5");
  assert.equal(problem(by("n"), "x"), "needs a number");
  v.draft = false;
  v.n = "3";
  v.title = "  Fix rail counts ";
  assert.deepEqual(collect(ask, v), { type: "form", values: { title: "Fix rail counts", reviewers: ["alice"], draft: false, n: 3 } });
});

test("a question waits in the session until answered or the turn ends", () => {
  const s = new SessionView(0, "s1", "/p", "m");
  const h = { subStarted() {}, subState() {} };
  s.apply({ type: "turn_started", data: { text: "open a PR" } }, h);
  s.apply({ type: "tool_start", data: { call: { id: "q1", name: "ask", arguments: JSON.stringify(ask) } } }, h);
  s.apply({ type: "question", data: { call_id: "q1", ask } }, h);
  assert.equal(s.question?.callId, "q1");
  assert.equal(s.entries[1].tool!.summary, "Open a PR");
  s.apply({ type: "answered", data: { call_id: "q1", answer: { type: "dismissed" } } }, h);
  assert.equal(s.question, null);
  s.apply({ type: "question", data: { call_id: "q2", ask } }, h);
  s.apply({ type: "turn_end", data: { cancelled: true, error: null } }, h);
  assert.equal(s.question, null);
});
