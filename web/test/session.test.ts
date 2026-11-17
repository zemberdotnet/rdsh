import assert from "node:assert/strict";
import { test } from "node:test";
import type { AgentEvent } from "../src/proto.ts";
import { SessionView, summarize } from "../src/session.ts";

const hooks = () => {
  const calls: string[] = [];
  return { calls, h: { subStarted: (id: string, p: string, t: string) => calls.push(`start ${id} ${p} ${t}`), subState: (id: string, s: string) => calls.push(`${s} ${id}`) } };
};

test("a turn streams reasoning, text and a tool, then ends", () => {
  const v = new SessionView(0, "s1", "/p", "m");
  const { h } = hooks();
  const evs: AgentEvent[] = [
    { type: "turn_started", data: { text: "hi" } },
    { type: "reasoning_delta", data: "think" },
    { type: "text_delta", data: "Hel" },
    { type: "text_delta", data: "lo" },
    { type: "assistant", data: { message: { role: "assistant", content: "Hello", tool_calls: [] }, usage: { prompt_tokens: 10, completion_tokens: 2 } } },
    { type: "tool_start", data: { call: { id: "c1", name: "bash", arguments: '{"command":"ls -la"}' } } },
    { type: "tool_end", data: { call_id: "c1", output: { content: "a\nb\nc\nd\ne", is_error: false }, duration_ms: 12 } },
    { type: "turn_end", data: { cancelled: false, error: null } },
  ];
  for (const e of evs) v.apply(e, h);
  assert.deepEqual(v.entries.map((e) => e.k), ["user", "reasoning", "assistant", "tool"]);
  assert.equal(v.entries[2].text, "Hello");
  assert.ok(!v.entries[1].streaming && !v.entries[2].streaming);
  const t = v.entries[3];
  assert.equal(t.tool!.summary, "ls -la");
  assert.equal(t.tool!.status, "ok");
  assert.ok(t.collapsed, "5 lines of output fold");
  assert.equal(v.usage.prompt_tokens, 10);
  assert.ok(!v.running);
});

test("subagent state flows to the hooks; an interrupted turn fails running tools", () => {
  const v = new SessionView(0, "s1", "/p", "m");
  const { calls, h } = hooks();
  v.apply({ type: "turn_started", data: { text: "go" } }, h);
  v.apply({ type: "tool_start", data: { call: { id: "t1", name: "task", arguments: '{"description":"scan"}' } } }, h);
  v.apply({ type: "subagent_started", data: { call_id: "t1", session_id: "s1-sub1", title: "scan" } }, h);
  v.apply({ type: "subagent", data: { call_id: "t1", event: { type: "tool_start", data: { call: { id: "x", name: "read_file", arguments: '{"path":"a.rs"}' } } } } }, h);
  v.apply({ type: "turn_end", data: { cancelled: true, error: null } }, h);
  assert.deepEqual(calls, ["start s1-sub1 s1 scan", "failed s1-sub1"]);
  assert.deepEqual(v.entries[1].tool!.children, ["↳ read_file a.rs"]);
  assert.equal(v.entries[1].tool!.status, "bad");
  assert.equal(v.entries[v.entries.length - 1].text, "interrupted");
});

test("history rebuilds the transcript and job notes show as info", () => {
  const v = new SessionView(0, "s1", "/p", "m");
  v.loadHistory([
    { role: "system", content: "sys" },
    { role: "user", content: "q" },
    { role: "assistant", content: "", tool_calls: [{ id: "c", name: "grep", arguments: '{"pattern":"x","path":"src"}' }] },
    { role: "tool", call_id: "c", name: "grep", content: "Error: nope" },
    { role: "user", content: "<background_jobs>job 1 done</background_jobs>" },
  ]);
  assert.deepEqual(v.entries.map((e) => [e.k, e.k === "tool" ? e.tool!.status : e.text]), [["user", "q"], ["tool", "bad"], ["info", "job 1 done"], ["info", "session s1"]]);
  assert.equal(summarize("grep", '{"pattern":"x","path":"src"}'), "x  in src");
});
