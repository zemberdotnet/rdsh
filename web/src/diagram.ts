// Diagram frames: each ```d3 block runs in its own iframe, isolated four ways.
//
// - `sandbox="allow-scripts"` without `allow-same-origin`: an opaque origin, so the script
//   cannot read this page, its storage or cookie, navigate the tab, open windows or submit forms.
// - Loaded from its own `<label>.localhost` name: its own site, so the browser gives it its
//   own process and a runaway script freezes neither the page nor the other diagrams. A frame
//   that stops answering pings is removed.
// - Served with a CSP that allows no network (D3 and the bootstrap come from this server).
//   A frame that navigates itself away is removed.
// - Its messages are only suggestions: the page acts on a short list (focus, pick into the
//   prompt, Escape and window keys) and never on keys that could type or send.

/** A fresh URL for one frame: its own `<label>.localhost` name, so its own site and process. */
export function frameUrl(loc: { protocol: string; port: string }): string {
  const label = `d${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
  return `${loc.protocol}//${label}.localhost:${loc.port}/frame`;
}

const PING_EVERY_MS = 2000;
const HUNG_AFTER_MS = 6000;

interface Slot {
  el: HTMLIFrameElement;
  script: string;
  title: string;
  ready: boolean;
  loads: number;
  /** When the unanswered ping went out. */
  pingAt: number | null;
}

export class Frames {
  private slots = new Map<string, Slot>();
  /** Frames removed, with why; their rows show the reason instead. */
  dead = new Map<string, string>();
  /** Script errors reported by frames, not yet passed on. */
  errors = new Map<string, { title: string; message: string }>();
  private layer: HTMLElement;
  private url: () => string;
  private seq = 0;
  /** Called when a frame changes state the page shows (died, errored). */
  onChange: () => void = () => {};
  /** Called when a diagram's script fails. */
  onError: (title: string) => void = () => {};

  constructor(layer: HTMLElement, url: () => string) {
    this.layer = layer;
    this.url = url;
  }

  /** The frame for `key`, created on first use; `null` once it has been removed. */
  get(key: string, script: string, title: string): HTMLIFrameElement | null {
    if (this.dead.has(key)) return null;
    let s = this.slots.get(key);
    if (s && s.script !== script) {
      s.el.remove();
      this.slots.delete(key);
      s = undefined;
    }
    if (!s) {
      const el = document.createElement("iframe");
      el.setAttribute("sandbox", "allow-scripts");
      el.setAttribute("referrerpolicy", "no-referrer");
      el.dataset.key = key;
      const slot: Slot = { el, script, title, ready: false, loads: 0, pingAt: null };
      el.addEventListener("load", () => {
        slot.loads++;
        // Our frame page loads once; a second load means the script navigated it elsewhere.
        if (slot.loads > 1) this.kill(key, "it tried to navigate away");
      });
      el.src = this.url();
      this.slots.set(key, slot);
      this.layer.appendChild(el);
      s = slot;
    }
    return s.el;
  }

  /** Hides frames not drawn this time (kept alive: moving an iframe reloads it). */
  hideExcept(live: Set<string>) {
    for (const [k, s] of this.slots) if (!live.has(k)) s.el.style.display = "none";
  }

  keyOf(source: MessageEventSource | null): string | null {
    for (const [k, s] of this.slots) if (s.el.contentWindow === source) return k;
    return null;
  }

  send(key: string, msg: unknown) {
    this.slots.get(key)?.el.contentWindow?.postMessage(msg, "*");
  }

  /** A message from frame `key`; returns what the page should act on, if anything. */
  receive(key: string, d: Record<string, unknown>): { type: "focus" } | { type: "pick"; label: string } | { type: "key"; key: string; ctrl: boolean; shift: boolean; alt: boolean } | null {
    const s = this.slots.get(key);
    if (!s) return null;
    switch (d.type) {
      case "ready":
        if (!s.ready) {
          s.ready = true;
          this.send(key, { type: "run", script: s.script });
        }
        return null;
      case "pong":
        s.pingAt = null;
        return null;
      case "error":
        if (typeof d.message === "string") {
          this.errors.set(key, { title: s.title, message: d.message.slice(0, 500) });
          this.onError(s.title);
        }
        return null;
      case "focus":
        return { type: "focus" };
      case "pick":
        return typeof d.label === "string" ? { type: "pick", label: d.label.slice(0, 200) } : null;
      case "key":
        return typeof d.key === "string" ? { type: "key", key: d.key, ctrl: !!d.ctrl, shift: !!d.shift, alt: !!d.alt } : null;
    }
    return null;
  }

  /** Pings visible frames; removes those that stopped answering. Call every second or so. */
  watch(now = Date.now()) {
    for (const [k, s] of this.slots) {
      if (!s.ready || s.el.style.display === "none") {
        s.pingAt = null;
        continue;
      }
      if (s.pingAt != null) {
        if (now - s.pingAt > HUNG_AFTER_MS) this.kill(k, "it stopped responding");
      } else if (now - (Number(s.el.dataset.pinged) || 0) >= PING_EVERY_MS) {
        s.pingAt = now;
        s.el.dataset.pinged = String(now);
        this.send(k, { type: "ping", n: ++this.seq });
      }
    }
  }

  kill(key: string, why: string) {
    const s = this.slots.get(key);
    if (s) {
      s.el.remove(); // ends its script and, for an out-of-process frame, its process
      this.slots.delete(key);
    }
    this.dead.set(key, why);
    this.onChange();
  }
}
