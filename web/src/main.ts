// Boot: the WebSocket to `rdsh web`, DOM events, timers.

import { App } from "./app.ts";
import { keyName, Keys } from "./keys.ts";
import type { ClientMsg, ServerMsg } from "./proto.ts";
import { GUTTER, View } from "./view.ts";

const input = document.querySelector("#input") as HTMLTextAreaElement;
const main = document.querySelector("#main") as HTMLElement;
const railEl = document.querySelector("#rail") as HTMLElement;

let ws: WebSocket | null = null;
let frame = 0;
const redraw = () => {
  if (!frame) frame = requestAnimationFrame(() => {
    frame = 0;
    view.draw();
  });
};
const app = new App((m: ClientMsg) => ws?.readyState === WebSocket.OPEN && ws.send(JSON.stringify(m)), redraw, {
  get: () => input.value,
  set: (s) => { input.value = s; },
});
const view = new View(app);
const keys = new Keys(app, view, input);

// ------------------------------------------------------------------ connection

let backoff = 500;
function connect() {
  const sock = new WebSocket(`ws://${location.host}/ws`);
  ws = sock;
  sock.onopen = () => { backoff = 500; };
  sock.onmessage = (ev) => {
    let m: ServerMsg;
    try {
      m = JSON.parse(ev.data);
    } catch {
      return;
    }
    app.onServer(m);
  };
  sock.onclose = () => {
    if (ws !== sock) return;
    app.connected = false;
    for (const h of app.hosts) h.connected = false;
    redraw();
    setTimeout(connect, backoff);
    backoff = Math.min(backoff * 2, 8000);
  };
}

// ------------------------------------------------------------------ keys

document.addEventListener("keydown", (e) => {
  if (document.activeElement === input && app.mode === "insert") return;
  if (keys.handle(e)) {
    e.preventDefault();
    redraw();
  }
});

input.addEventListener("keydown", (e) => {
  if (app.mode !== "insert") return;
  if (keys.insertKey(e)) {
    e.preventDefault();
    redraw();
  }
});
input.addEventListener("input", () => { app.histPos = null; view.drawPrompt(); });
input.addEventListener("focus", () => {
  if (app.mode === "insert") return;
  if (app.previewing() || app.cur.readOnly) {
    input.blur();
    return;
  }
  app.focusRail(false);
  app.mode = "insert";
  redraw();
});
input.addEventListener("blur", () => {
  if (app.mode === "insert") {
    app.mode = "normal";
    redraw();
  }
});

// Messages from diagram frames. Only our frames count, and they only ever get to focus their
// diagram, put a picked item in the prompt (never send it), or leave with Escape and the window
// keys. A script can post anything, so nothing here may type, send or run a command.
const FRAME_KEYS = /^(Escape|C-Space|C-w)$/;
window.addEventListener("message", (ev) => {
  const k = view.frames.keyOf(ev.source);
  if (!k || !ev.data || typeof ev.data !== "object") return;
  const m = view.frames.receive(k, ev.data);
  if (!m) return redraw();
  if (m.type === "focus") {
    if (app.diagram?.key !== k) {
      app.focusRail(false);
      if (document.activeElement === input) input.blur();
      keys.enterDiagram(k, false);
    }
  } else if (m.type === "pick") {
    const dia = app.rows.find((r) => r.dia?.key === k)?.dia;
    app.setFlash(`picked "${m.label.slice(0, 80)}" — it's in the prompt`);
    if (!app.previewing() && !app.cur.readOnly) {
      input.value += (input.value && !/\s$/.test(input.value) ? " " : "") + `[${dia?.title ?? "diagram"}: ${m.label}] `;
    }
  } else {
    const name = keyName({ key: m.key, ctrlKey: m.ctrl, shiftKey: m.shift, altKey: m.alt });
    const windowKey = app.pending === "W" && /^(h|l|C-h|C-l)$/.test(name);
    if (FRAME_KEYS.test(name) || windowKey) {
      // Take the focus back so the next keys come to the page, not through the frame.
      if (document.activeElement instanceof HTMLIFrameElement) document.activeElement.blur();
      keys.handle({ key: m.key, ctrlKey: m.ctrl, shiftKey: m.shift, altKey: m.alt });
    }
  }
  redraw();
});

view.frames.onChange = redraw;
app.diagramErrors = () => {
  const out = [...view.frames.errors.values()];
  view.frames.errors.clear();
  return out;
};

// ------------------------------------------------------------------ mouse

railEl.addEventListener("mousedown", (ev) => {
  const row = (ev.target as HTMLElement).closest(".row[data-i]") as HTMLElement | null;
  if (!row) return;
  ev.preventDefault();
  if (document.activeElement === input) input.blur();
  app.focusRail(true);
  app.selectRail(Number(row.dataset.i));
  redraw();
});
railEl.addEventListener("dblclick", () => {
  app.openSelected();
  redraw();
});

main.addEventListener("mousedown", (ev) => {
  if ((ev.target as HTMLElement).tagName === "IFRAME") return;
  ev.preventDefault();
  if (app.railFocus) app.focusRail(false);
  if (document.activeElement === input) input.blur();
  if (app.mode === "diagram") keys.leaveDiagram();
  if (app.mode !== "visual" && app.mode !== "vline") app.mode = "normal";
  app.relayout(view.cols());
  const rect = main.getBoundingClientRect();
  const c = app.shown().cur;
  c.row = Math.min(app.rows.length - 1, c.top + Math.floor((ev.clientY - rect.top) / view.lh));
  const raw = Math.floor((ev.clientX - rect.left) / view.cw) - GUTTER;
  c.col = Math.max(0, raw);
  c.want = c.col;
  c.follow = false;
  // A click on a gutter marker folds or unfolds, like Tab.
  if (raw < 0 && app.rows[c.row]?.g[0].trim()) keys.fold();
  redraw();
});

main.addEventListener("wheel", (ev) => {
  ev.preventDefault();
  const c = app.shown().cur;
  const vis = view.visibleRows();
  c.top = Math.max(0, Math.min(app.rows.length - vis, c.top + Math.sign(ev.deltaY) * 3));
  if (c.row < c.top) c.row = c.top;
  if (c.row >= c.top + vis) c.row = c.top + vis - 1;
  c.follow = false;
  redraw();
}, { passive: false });

// ------------------------------------------------------------------ timers and boot

// Watchdog for diagram frames.
setInterval(() => view.frames.watch(), 1000);

setInterval(() => {
  if (!app.busy()) return;
  app.spin++;
  redraw();
}, 100);
// Lets finished subagents age out of the rail.
setInterval(redraw, 30_000);

window.addEventListener("resize", () => { view.measure(); redraw(); });
document.fonts.ready.then(() => { view.measure(); redraw(); });
view.measure();
redraw();
connect();
view.frames.onError = (title) => {
  app.setFlash(`diagram "${title}" failed — the error goes to the model with your next message`, true);
  redraw();
};

// ------------------------------------------------------------------ forms (`ask`)

const form = view.form;
form.onAnswer = (a) => { app.answer(a); redraw(); };
form.onFlash = (s, err) => { app.setFlash(s, err); redraw(); };
form.onLeave = (words) => {
  app.mode = "normal";
  if (words) keys.startInsert();
  redraw();
};
// The mode follows the focus: in the panel it is FORM, anywhere else it is not.
const formEl = document.querySelector("#form") as HTMLElement;
formEl.addEventListener("focusin", () => {
  if (app.mode !== "form") {
    app.focusRail(false);
    app.mode = "form";
    redraw();
  }
});
formEl.addEventListener("focusout", (e) => {
  if (!form.has(e.relatedTarget as Node | null) && app.mode === "form") {
    app.mode = "normal";
    redraw();
  }
});
// A new question takes the focus unless you are typing or busy elsewhere.
view.onQuestion = () => {
  if (app.mode === "normal" && !app.railFocus) setTimeout(() => form.focus(), 0);
  else app.setFlash("the agent asks something — Esc, then i to answer");
};
