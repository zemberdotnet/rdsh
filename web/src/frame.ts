// Runs inside a diagram frame: sandboxed (opaque origin), on another site than the page (its own
// process), under a CSP that allows no network. The page sends the model's script once with
// `run`; this sets up the canvas, runs it, and talks back only through postMessage.

declare const d3: any;

const C = {
  bg: "#282828", fg: "#ebdbb2", fgdim: "#a89984", gray: "#928374", panel: "#3c3836", sel: "#504945",
  red: "#fb4934", green: "#b8bb26", yellow: "#fabd2f", blue: "#83a598", purple: "#d3869b", aqua: "#8ec07c", orange: "#fe8019",
};
const CH = 7.2;
const post = (m: unknown) => parent.postMessage(m, "*");

const W = innerWidth;
const H = innerHeight;
const svg = d3.select("#s").attr("width", W).attr("height", H);
const g = svg.append("g");
const zoom = d3.zoom().scaleExtent([0.2, 10]).on("zoom", (e: any) => g.attr("transform", e.transform));
svg.call(zoom).on("dblclick.zoom", null);

function pick(label: unknown) {
  post({ type: "pick", label: String(label).slice(0, 200) });
}

function report(message: string) {
  let el = document.getElementById("err");
  if (!el) {
    el = document.createElement("pre");
    el.id = "err";
    document.body.appendChild(el);
  }
  el.textContent = message;
  post({ type: "error", message: message.slice(0, 500) });
}
window.onerror = (m) => report(String(m));
window.addEventListener("unhandledrejection", (e) => report(String(e.reason)));

/** Pan and zoom: from keys typed here, or from the page while it has the focus. */
function act(k: string): boolean {
  const s = 60;
  const t = svg.transition().duration(140);
  if (k === "h" || k === "ArrowLeft") t.call(zoom.translateBy, s, 0);
  else if (k === "l" || k === "ArrowRight") t.call(zoom.translateBy, -s, 0);
  else if (k === "k" || k === "ArrowUp") t.call(zoom.translateBy, 0, s);
  else if (k === "j" || k === "ArrowDown") t.call(zoom.translateBy, 0, -s);
  else if (k === "+" || k === "=") t.call(zoom.scaleBy, 1.25);
  else if (k === "-") t.call(zoom.scaleBy, 0.8);
  else if (k === "0") t.call(zoom.transform, d3.zoomIdentity);
  else return false;
  return true;
}

addEventListener("keydown", (e) => {
  e.preventDefault();
  if (!e.ctrlKey && act(e.key)) return;
  // The page takes only Escape and window commands from here.
  post({ type: "key", key: e.key, ctrl: e.ctrlKey, shift: e.shiftKey, alt: e.altKey });
});
addEventListener("pointerdown", () => post({ type: "focus" }));

let ran = false;
addEventListener("message", (e) => {
  if (e.source !== parent) return;
  const d = e.data;
  if (!d || typeof d !== "object") return;
  if (d.type === "ping") post({ type: "pong", n: d.n });
  else if (d.type === "act" && typeof d.key === "string") act(d.key);
  else if (d.type === "run" && typeof d.script === "string" && !ran) {
    ran = true;
    try {
      new Function("d3", "svg", "g", "W", "H", "C", "CH", "pick", d.script)(d3, svg, g, W, H, C, CH, pick);
    } catch (err) {
      report(String(err));
    }
  }
});

post({ type: "ready" });
