// The hub protocol as JSON, mirroring src/hub/proto.rs, src/agent.rs and src/llm.rs, plus the
// envelope `rdsh web` wraps it in (src/webview.rs).

export type Effort = "off" | "low" | "high" | "max";
export const EFFORTS: Effort[] = ["off", "low", "high", "max"];

export interface SystemTime { secs_since_epoch: number; nanos_since_epoch: number }
export const millis = (t: SystemTime) => t.secs_since_epoch * 1000 + Math.floor(t.nanos_since_epoch / 1e6);

export interface SessionInfo {
  id: string;
  path: string;
  modified: SystemTime;
  parent: string | null;
  cwd: string;
  has_input: boolean;
  slug?: string | null;
  title: string;
}

export interface ToolCall { id: string; name: string; arguments: string }

export type Message =
  | { role: "system"; content: string }
  | { role: "user"; content: string }
  | { role: "context"; content: string }
  | { role: "directive"; content: string }
  | { role: "assistant"; content?: string; reasoning?: string | null; tool_calls?: ToolCall[] }
  | { role: "tool"; call_id: string; name: string; content: string };

export interface Usage { prompt_tokens: number; completion_tokens: number; cached_tokens?: number }

export type Routing =
  | { kind: "picked"; level: string; confidence: number }
  | { kind: "unsure"; level: string; confidence: number }
  | { kind: "unavailable"; reason: string };

export type AgentEvent =
  | { type: "turn_started"; data: { text: string } }
  | { type: "titled"; data: { title: string } }
  | { type: "text_delta"; data: string }
  | { type: "reasoning_delta"; data: string }
  | { type: "tool_call_streaming"; data: { name: string } }
  | { type: "assistant"; data: { message: Message; usage: Usage | null } }
  | { type: "tool_start"; data: { call: ToolCall } }
  | { type: "tool_end"; data: { call_id: string; output: { content: string; is_error: boolean }; duration_ms: number } }
  | { type: "subagent_started"; data: { call_id: string; session_id: string; title: string } }
  | { type: "subagent"; data: { call_id: string; event: AgentEvent } }
  | { type: "response_length"; data: Routing }
  | { type: "jobs_note"; data: { text: string } }
  | { type: "question"; data: { call_id: string; ask: Ask } }
  | { type: "answered"; data: { call_id: string; answer: Answer } }
  | { type: "warning"; data: string }
  | { type: "turn_end"; data: { cancelled: boolean; error: string | null } };

export interface QueuedTurn { id: number; text: string }

export interface Snapshot {
  session_id: string;
  model: string;
  cwd: string;
  history: Message[];
  turn: AgentEvent[];
  running: boolean;
  usage: Usage;
  queued?: QueuedTurn[];
}

export type Notice =
  | { type: "hello"; version: string }
  | { type: "sessions"; recorded: SessionInfo[]; open: string[]; running: string[] }
  | { type: "opened"; snapshot: Snapshot }
  | { type: "queue"; session_id: string; queued: QueuedTurn[] }
  | { type: "queue_edited"; session_id: string; turn_id: number; updated: boolean }
  | { type: "event"; session_id: string; event: AgentEvent }
  | { type: "trace"; session_id: string; cwd: string; history: Message[] }
  | { type: "file"; file: FileDiff }
  | { type: "changed_files"; cwd: string; files: [string, string][] }
  | { type: "path_completions"; request_id: number; paths: string[] }
  | { type: "resolved_paths"; request_id: number; cwd: string; found: [string, string][] }
  | { type: "restarting" }
  | { type: "error"; session_id: string | null; message: string };

export type Request =
  | { type: "list" }
  | { type: "new"; cwd?: string | null }
  | { type: "open"; session_id: string }
  | { type: "send"; session_id: string; text: string; effort: Effort; model?: string; diagrams?: boolean }
  | { type: "watch_queue" }
  | { type: "edit_queued"; session_id: string; turn_id: number; text: string }
  | { type: "answer"; session_id: string; question_id: string; answer: Answer }
  | { type: "cancel"; session_id: string }
  | { type: "run_queued_now"; session_id: string }
  | { type: "complete_path"; session_id: string; request_id: number; prefix: string }
  | { type: "close"; session_id: string }
  | { type: "read_file"; cwd: string; path: string; base?: Base }
  | { type: "resolve_paths"; cwd: string; request_id: number; paths: string[] }
  | { type: "changed_files"; cwd: string }
  | { type: "read_trace"; session_id: string }
  | { type: "restart" };

export interface HostSpec { name: string; local: boolean; dir: string | null }

/** `rdsh web` → page. */
export type ServerMsg =
  | { type: "welcome"; version: string; cwd: string; effort: Effort; models: string[]; hosts: HostSpec[] }
  | { type: "host_added"; host: HostSpec }
  | { type: "notice"; host: number; notice: Notice }
  | { type: "disconnected"; host: number; reason: string | null }
  | { type: "error"; message: string };

/** Page → `rdsh web`. */
export type ClientMsg =
  | { type: "request"; host: number; request: Request }
  | { type: "add_host"; spec: string };

// `ask` (src/tools/ask.rs): forms the agent puts in front of the user.

export interface Ask { title?: string; forms: Form[] }
export interface Form { title?: string; description?: string; fields: Field[] }
export interface Choice { label: string; description?: string }
export type Field = { id: string; label: string; help?: string; required?: boolean } & (
  | { type: "text"; placeholder?: string; default?: string; multiline?: boolean }
  | { type: "choice"; options: Choice[]; multiple?: boolean; other?: boolean; default?: string[] }
  | { type: "confirm"; default?: boolean }
  | { type: "number"; min?: number; max?: number; default?: number }
);
export type Value = string | number | boolean | string[];
export type Answer =
  | { type: "form"; values: Record<string, Value> }
  | { type: "text"; text: string }
  | { type: "dismissed" };

// Files (src/diff.rs): a file with its diff against HEAD or the default branch.

export type Base = "head" | "default";
export type DiffKind = "Meta" | "Hunk" | "Add" | "Del" | "Ctx";
export interface DiffLine { kind: DiffKind; text: string; old: number | null; new: number | null }
export interface FileDiff {
  path: string;
  /** Path relative to the session's directory when inside it. */
  rel: string;
  status: "Modified" | "Untracked" | "Clean" | "NoRepo";
  diff: DiffLine[];
  /** The whole file; lines added relative to the base are `Add`. */
  full: DiffLine[];
  added: number;
  removed: number;
  error: string | null;
  head_source: string | null;
  /** The default branch compared against (`origin/main`); absent means HEAD. */
  base?: string | null;
}
