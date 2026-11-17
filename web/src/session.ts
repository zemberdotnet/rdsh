// One session as the page shows it: transcript entries built from history and live agent
// events. Ported from SessionView and App::on_agent / load_history in src/tui/app.rs.

import type { AgentEvent, Ask, Message, Snapshot, Usage } from "./proto.ts";

export const COLLAPSED_OUTPUT_LINES = 4;
export const COLLAPSED_REASONING_LINES = 3;
const NOTE_TAG = "<background_jobs>";
const NOTE_END = "</background_jobs>";

export type ToolStatus = "run" | "ok" | "bad";

export interface Tool {
  callId: string;
  name: string;
  summary: string;
  status: ToolStatus;
  output: string;
  children: string[];
  dur: number | null;
}

export type Kind = "user" | "assistant" | "reasoning" | "tool" | "info" | "error";

export interface Entry {
  id: number;
  k: Kind;
  text: string;
  tool?: Tool;
  collapsed: boolean;
  streaming: boolean;
  /** Bumped on every change, so layouts can be cached per entry. */
  v: number;
}

export interface Cursor { row: number; col: number; top: number; follow: boolean; want: number | null }

/** Calls out of the session for state that lives elsewhere (the rail's live subagents). */
export interface Hooks {
  subStarted(id: string, parent: string, title: string): void;
  subState(id: string, state: "done" | "failed"): void;
}

let nextId = 0;

export class SessionView {
  entries: Entry[] = [];
  running = false;
  activity: string | null = null;
  turnStarted = 0;
  usage: Usage = { prompt_tokens: 0, completion_tokens: 0 };
  length: string | null = null;
  readOnly = false;
  hasInput = false;
  draft = "";
  waiting = 0;
  /** An `ask` call waiting for the user. */
  question: { callId: string; ask: Ask } | null = null;
  cur: Cursor = { row: 0, col: 0, top: 0, follow: true, want: null };
  /** Bumped when entries are added or changed. */
  v = 0;
  private streamingText: Entry | null = null;
  private streamingReasoning: Entry | null = null;
  private toolEntries = new Map<string, Entry>();
  /** `task` call id → subagent session id, while in flight. */
  private subagentCalls = new Map<string, string>();

  host: number;
  id: string;
  cwd: string;
  model: string;

  constructor(host: number, id: string, cwd: string, model: string) {
    this.host = host;
    this.id = id;
    this.cwd = cwd;
    this.model = model;
  }

  static fromSnapshot(host: number, s: Snapshot, hooks: Hooks): SessionView {
    const v = new SessionView(host, s.session_id, s.cwd, s.model);
    v.loadHistory(s.history);
    for (const ev of s.turn) v.apply(ev, hooks);
    v.running = s.running;
    v.usage = { ...s.usage };
    return v;
  }

  push(k: Kind, text: string, tool?: Tool): Entry {
    const collapsed = k === "reasoning" || (k === "tool" && !!tool && lineCount(tool.output) > COLLAPSED_OUTPUT_LINES);
    const e: Entry = { id: ++nextId, k, text, tool, collapsed, streaming: false, v: 0 };
    this.entries.push(e);
    this.v++;
    return e;
  }

  touch(e: Entry) {
    e.v++;
    this.v++;
  }

  loadHistory(history: Message[]) {
    for (const m of history) {
      switch (m.role) {
        case "user":
          if (m.content.startsWith(NOTE_TAG)) this.push("info", noteBody(m.content));
          else {
            this.hasInput = true;
            this.push("user", m.content);
          }
          break;
        case "assistant":
          if (m.reasoning) this.push("reasoning", m.reasoning);
          if (m.content) this.push("assistant", m.content);
          for (const c of m.tool_calls ?? []) this.toolEntries.set(c.id, this.push("tool", "", toolEntry(c.id, c.name, c.arguments)));
          break;
        case "tool": {
          const e = this.toolEntries.get(m.call_id);
          if (e) this.finishTool(e, m.content, m.content.startsWith("Error:"), null);
          break;
        }
      }
    }
    if (history.some((m) => m.role === "user")) this.push("info", `session ${this.id}`);
  }

  private finishTool(e: Entry, output: string, failed: boolean, dur: number | null) {
    const t = e.tool!;
    t.status = failed ? "bad" : "ok";
    t.output = output;
    t.dur = dur;
    e.collapsed = lineCount(output) > COLLAPSED_OUTPUT_LINES;
    this.touch(e);
  }

  private closeStreams() {
    for (const e of [this.streamingText, this.streamingReasoning]) {
      if (e) {
        e.streaming = false;
        this.touch(e);
      }
    }
    this.streamingText = this.streamingReasoning = null;
  }

  private streamInto(reasoning: boolean, delta: string) {
    let e = reasoning ? this.streamingReasoning : this.streamingText;
    if (!e) {
      if (!reasoning && this.streamingReasoning) {
        // Reasoning ends when visible text starts.
        this.streamingReasoning.streaming = false;
        this.touch(this.streamingReasoning);
        this.streamingReasoning = null;
      }
      e = this.push(reasoning ? "reasoning" : "assistant", "");
      e.streaming = true;
      if (reasoning) this.streamingReasoning = e;
      else this.streamingText = e;
    }
    e.text += delta;
    this.touch(e);
  }

  apply(ev: AgentEvent, hooks: Hooks) {
    switch (ev.type) {
      case "titled":
        break; // the rail picks titles up from the next `sessions` notice
      case "turn_started": {
        this.closeStreams();
        const text = ev.data.text;
        if (text.startsWith(NOTE_TAG)) this.push("info", noteBody(text));
        else {
          this.hasInput = true;
          this.push("user", text);
          this.waiting = Math.max(0, this.waiting - 1);
        }
        this.running = true;
        this.cur.follow = true;
        this.turnStarted = Date.now();
        this.activity = "waiting for model";
        break;
      }
      case "text_delta":
        this.activity = "writing";
        this.streamInto(false, ev.data);
        break;
      case "reasoning_delta":
        this.activity = "thinking";
        this.streamInto(true, ev.data);
        break;
      case "tool_call_streaming":
        this.activity = `preparing ${ev.data.name}`;
        break;
      case "assistant": {
        const { message, usage } = ev.data;
        if (usage) addUsage(this.usage, usage);
        if (message.role === "assistant") {
          const content = message.content ?? "";
          if (this.streamingText) {
            this.streamingText.text = content;
            this.touch(this.streamingText);
          } else if (content) this.push("assistant", content);
        }
        this.closeStreams();
        this.activity = "waiting for model";
        break;
      }
      case "tool_start": {
        this.closeStreams();
        const c = ev.data.call;
        this.toolEntries.set(c.id, this.push("tool", "", toolEntry(c.id, c.name, c.arguments)));
        this.activity = `running ${c.name}`;
        break;
      }
      case "subagent_started":
        this.subagentCalls.set(ev.data.call_id, ev.data.session_id);
        hooks.subStarted(ev.data.session_id, this.id, ev.data.title);
        break;
      case "tool_end": {
        const { call_id, output, duration_ms } = ev.data;
        const e = this.toolEntries.get(call_id);
        if (e) this.finishTool(e, output.content, output.is_error, duration_ms);
        const sub = this.subagentCalls.get(call_id);
        if (sub) {
          this.subagentCalls.delete(call_id);
          hooks.subState(sub, output.is_error ? "failed" : "done");
        }
        break;
      }
      case "subagent": {
        const { call_id, event } = ev.data;
        const sub = this.subagentCalls.get(call_id);
        if (event.type === "turn_end" && sub) hooks.subState(sub, event.data.cancelled || event.data.error ? "failed" : "done");
        const e = this.toolEntries.get(call_id);
        if (!e) break;
        let line: string | null = null;
        if (event.type === "tool_start") line = `↳ ${event.data.call.name} ${summarize(event.data.call.name, event.data.call.arguments)}`;
        else if (event.type === "tool_end" && event.data.output.is_error) line = `  ✗ ${event.data.output.content.split("\n")[0]}`;
        else if (event.type === "assistant" && event.data.usage) addUsage(this.usage, event.data.usage);
        if (line) {
          e.tool!.children.push(line);
          this.touch(e);
        }
        break;
      }
      case "response_length": {
        const r = ev.data;
        this.length = r.kind === "picked" ? `${r.level} ${r.confidence.toFixed(2)}`
          : r.kind === "unsure" ? `unsure (${r.level} ${r.confidence.toFixed(2)})` : "unrouted";
        break;
      }
      case "jobs_note":
        this.push("info", noteBody(ev.data.text));
        break;
      case "question":
        this.question = { callId: ev.data.call_id, ask: ev.data.ask };
        this.activity = "waiting for your answer";
        break;
      case "answered":
        if (this.question?.callId === ev.data.call_id) this.question = null;
        this.activity = "waiting for model";
        break;
      case "warning":
        this.push("info", ev.data);
        break;
      case "turn_end": {
        this.closeStreams();
        this.question = null;
        this.running = false;
        this.activity = null;
        for (const e of this.toolEntries.values()) if (e.tool!.status === "run") this.finishTool(e, "interrupted", true, null);
        if (ev.data.error) this.push("error", ev.data.error);
        if (ev.data.cancelled) this.push("info", "interrupted");
        // A turn that ended with subagents still in flight was interrupted.
        for (const id of this.subagentCalls.values()) hooks.subState(id, "failed");
        this.subagentCalls.clear();
        break;
      }
    }
    this.v++;
  }
}

function addUsage(a: Usage, b: Usage) {
  a.prompt_tokens += b.prompt_tokens;
  a.completion_tokens += b.completion_tokens;
  a.cached_tokens = (a.cached_tokens ?? 0) + (b.cached_tokens ?? 0);
}

const lineCount = (s: string) => (s ? s.split("\n").length : 0);

function noteBody(text: string): string {
  let t = text.startsWith(NOTE_TAG) ? text.slice(NOTE_TAG.length) : text;
  if (t.endsWith(NOTE_END)) t = t.slice(0, -NOTE_END.length);
  return t.trim();
}

function toolEntry(callId: string, name: string, args: string): Tool {
  return { callId, name, summary: summarize(name, args), status: "run", output: "", children: [], dur: null };
}

/** One-line summary of a tool call's arguments (src/tools/mod.rs `summarize`). */
export function summarize(name: string, args: string): string {
  let a: Record<string, unknown> = {};
  try {
    const v = JSON.parse(args);
    if (v && typeof v === "object") a = v;
  } catch { /* raw text below */ }
  const get = (k: string) => (typeof a[k] === "string" ? (a[k] as string) : "");
  let s: string;
  switch (name) {
    case "bash": s = get("command"); break;
    case "read_file": case "write_file": case "edit_file": s = get("path"); break;
    case "glob": s = get("pattern"); break;
    case "grep": s = get("path") ? `${get("pattern")}  in ${get("path")}` : get("pattern"); break;
    case "task": s = get("description"); break;
    case "ask": {
      const n = Array.isArray(a.forms) ? (a.forms as { fields?: unknown[] }[]).reduce((k, f) => k + (Array.isArray(f?.fields) ? f.fields.length : 0), 0) : 0;
      s = get("title") || `${n} question${n === 1 ? "" : "s"}`;
      break;
    }
    case "web_search": s = get("query"); break;
    case "web_fetch": s = Array.isArray(a.urls) ? (a.urls as unknown[]).filter((u) => typeof u === "string").join(" ") : get("url"); break;
    case "job_output": case "job_kill": s = a.id != null ? `job ${a.id}` : ""; break;
    default: s = args;
  }
  const lines = s.split("\n");
  const first = [...lines[0]];
  let out = first.slice(0, 120).join("");
  if (first.length > 120 || lines.length > 1) out += "…";
  return out;
}
