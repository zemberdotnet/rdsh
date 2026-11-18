// Review comments queued by the user and the message that carries them. A port of
// src/comments.rs: the browser composes exactly what the TUI sends.

/** What a comment is attached to. */
export type Anchor =
  /** Text selected in the conversation; `source` names where it came from ("your response"). */
  | { kind: "chat"; source: string; quote: string }
  /** Lines of a file as shown in its diff view; `snippet` keeps the `+`/`-`/` ` prefixes. */
  | { kind: "file"; path: string; lines: [number, number] | null; snippet: string };

export interface Comment { anchor: Anchor; body: string }

/** Short label for lists, e.g. `src/main.rs:10-12` or `your response`. */
export function label(c: Comment): string {
  const a = c.anchor;
  if (a.kind === "chat") return a.source;
  if (!a.lines) return a.path;
  return a.lines[0] === a.lines[1] ? `${a.path}:${a.lines[0]}` : `${a.path}:${a.lines[0]}-${a.lines[1]}`;
}

/** The quoted text or snippet. */
export function excerpt(c: Comment): string {
  return c.anchor.kind === "chat" ? c.anchor.quote : c.anchor.snippet;
}

/** A backtick fence longer than any backtick run inside `s`. */
function fenceFor(s: string): string {
  const longest = Math.max(0, ...(s.match(/`+/g) ?? []).map((r) => r.length));
  return "`".repeat(Math.max(3, longest + 1));
}

/** The user message for a turn: the review block (if any comments) followed by `text`. */
export function composeMessage(comments: Comment[], text: string): string {
  text = text.trim();
  if (!comments.length) return text;
  let out = "<review_comments>\nI left comments on specific parts of the conversation and the working tree. "
    + "Each quotes what it refers to. Address every comment.\n";
  comments.forEach((c, i) => {
    out += "\n";
    const a = c.anchor;
    if (a.kind === "chat") {
      out += `[${i + 1}] On ${a.source}:\n`;
      for (const l of a.quote.split("\n")) out += `> ${l}\n`;
    } else {
      out += `[${i + 1}] On ${label(c)} (current diff):\n`;
      const fence = fenceFor(a.snippet);
      out += `${fence}diff\n${a.snippet}\n${fence}\n`;
    }
    out += `Comment: ${c.body.trim()}\n`;
  });
  out += "</review_comments>";
  if (text) out += `\n\n${text}`;
  return out;
}
