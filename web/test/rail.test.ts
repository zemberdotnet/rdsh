import assert from "node:assert/strict";
import { test } from "node:test";
import type { SessionInfo } from "../src/proto.ts";
import { rail, STALE_SUBAGENT_MS, type HostRail, type LiveSub } from "../src/rail.ts";

const now = Date.now();
const info = (id: string, parent: string | null, ageMs = 0, cwd = "/p"): SessionInfo => ({
  id, path: id, parent, cwd, has_input: true, title: id,
  modified: { secs_since_epoch: Math.floor((now - ageMs) / 1000), nanos_since_epoch: 0 },
});
const host = (recorded: SessionInfo[], live: [string, LiveSub][] = [], running: string[] = []): HostRail => ({
  name: "local", connected: true, recorded, running: new Set(running), live: new Map(live), filter: "/p",
});

test("subagents nest in start order and old finished ones hide until shown", () => {
  const old = STALE_SUBAGENT_MS * 2;
  const h = host([info("b-sub10", "b"), info("b-sub2", "b", old), info("b", null), info("x", null, 0, "/q"), info("a", null)]);
  const ids = (r: { id: string }[]) => r.map((x) => x.id);
  const hidden = rail([h], false, now, () => false);
  assert.deepEqual(ids(hidden.rows), ["b", "b-sub10", "a"]);
  assert.equal(hidden.hidden, 1);
  assert.deepEqual(ids(rail([h], true, now, () => false).rows), ["b", "b-sub2", "b-sub10", "a"]);
  // The selected or current row stays.
  assert.deepEqual(ids(rail([h], false, now, (r) => r.id === "b-sub2").rows), ["b", "b-sub2", "b-sub10", "a"]);
});

test("running subagents and those of a running session never hide; live ones show before their trace", () => {
  const old = STALE_SUBAGENT_MS * 2;
  const h = host([info("b-sub1", "b", old), info("b", null)], [["b-sub2", { parent: "b", title: "scan", state: "running", finished: null }]], ["b"]);
  const r = rail([h], false, now, () => false).rows;
  assert.deepEqual(r.map((x) => [x.id, x.live]), [["b", "running"], ["b-sub1", null], ["b-sub2", "running"]]);
});

test("several hosts get headings", () => {
  const r = rail([host([info("a", null)]), { ...host([info("z", null)]), name: "box", filter: null, connected: false }], false, now, () => false).rows;
  assert.deepEqual(r.map((x) => (x.header ? `#${x.title}` : x.id)), ["#local", "a", "#box (disconnected)", "z"]);
});
