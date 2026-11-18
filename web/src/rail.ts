// The session rail: recorded sessions newest first, their subagents nested beneath, one group
// per host.

import { millis, type SessionInfo } from "./proto.ts";

export type Live = "running" | "done" | "failed";

/** A subagent this page watched start. */
export interface LiveSub { parent: string; title: string; state: Live; finished: number | null }

export interface RailRow {
  id: string;
  host: number;
  /** A host heading (shown when several hosts are attached); `id` is empty. */
  header: boolean;
  depth: 0 | 1;
  title: string;
  /** `running` for a session mid-turn in the hub; for subagents, the outcome seen, if any. */
  live: Live | null;
  /** For a subagent that is not running: when it finished (ms), or its trace was last written. */
  finished: number | null;
}

/** Finished subagents older than this are hidden until `zh` shows them. */
export const STALE_SUBAGENT_MS = 5 * 60 * 1000;

const subNumber = (id: string) => {
  const i = id.lastIndexOf("-sub");
  const n = i < 0 ? NaN : Number(id.slice(i + 4));
  return Number.isFinite(n) ? n : Number.MAX_SAFE_INTEGER;
};

export interface HostRail {
  name: string;
  connected: boolean;
  recorded: SessionInfo[];
  running: Set<string>;
  live: Map<string, LiveSub>;
  /** Only sessions recorded in this directory are listed. */
  filter: string | null;
}

export function build(host: number, h: HostRail): RailRow[] {
  const subs = new Map<string, RailRow[]>();
  const add = (parent: string, r: RailRow) => {
    const v = subs.get(parent) ?? [];
    v.push(r);
    subs.set(parent, v);
  };
  for (const s of h.recorded) {
    if (!s.parent) continue;
    const l = h.live.get(s.id);
    const finished = l ? (l.state === "running" ? null : (l.finished ?? millis(s.modified))) : millis(s.modified);
    // The `task` description is the short label; the trace title is the full prompt.
    add(s.parent, { id: s.id, host, header: false, depth: 1, title: l?.title || s.title, live: l?.state ?? null, finished });
  }
  // Subagents whose trace is not listed yet.
  for (const [id, l] of h.live) {
    if (!(subs.get(l.parent) ?? []).some((r) => r.id === id)) {
      add(l.parent, { id, host, header: false, depth: 1, title: l.title, live: l.state, finished: l.finished });
    }
  }
  const rows: RailRow[] = [];
  for (const s of h.recorded) {
    if (s.parent || (h.filter != null && s.cwd !== h.filter)) continue;
    const title = s.slug || s.title;
    rows.push({ id: s.id, host, header: false, depth: 0, title, live: h.running.has(s.id) ? "running" : null, finished: null });
    const children = subs.get(s.id);
    if (children) rows.push(...children.sort((a, b) => subNumber(a.id) - subNumber(b.id)));
  }
  return rows;
}

/**
 * Drops subagent rows that finished before `cutoff`, except those `keep` names and the
 * subagents of a session still mid-turn.
 */
export function hideStale(rows: RailRow[], cutoff: number, keep: (r: RailRow) => boolean): { rows: RailRow[]; hidden: number } {
  let parentRunning = false;
  const out = rows.filter((r) => {
    if (r.depth === 0) {
      parentRunning = r.live === "running";
      return true;
    }
    return parentRunning || keep(r) || r.finished == null || r.finished >= cutoff;
  });
  return { rows: out, hidden: rows.length - out.length };
}

/** The whole rail; host headings only when there is more than one host. */
export function rail(hosts: HostRail[], showStale: boolean, now: number, keep: (r: RailRow) => boolean): { rows: RailRow[]; hidden: number } {
  const all: RailRow[] = [];
  let hidden = 0;
  hosts.forEach((h, i) => {
    let part = build(i, h);
    if (!showStale) {
      const r = hideStale(part, now - STALE_SUBAGENT_MS, keep);
      part = r.rows;
      hidden += r.hidden;
    }
    if (hosts.length > 1) {
      all.push({ id: "", host: i, header: true, depth: 0, title: h.connected ? h.name : `${h.name} (disconnected)`, live: null, finished: null });
    }
    all.push(...part);
  });
  return { rows: all, hidden };
}
