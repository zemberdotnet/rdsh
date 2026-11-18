# rdsh

A fast, low-footprint coding agent for DeepSeek and OpenAI models (API key or ChatGPT subscription), written in Rust, with a browser UI.
It has an agent loop, file, shell and search tools, parallel subagents and JSONL traces. Review is
built in: open any file path mentioned in the chat to see its diff, comment on lines or on selected
chat text, and send all queued comments, quoted, with your next message.

## Run

```sh
direnv allow                     # loads flake.nix (cargo, rustc, clippy) and ./.env
cargo build --release
./target/release/rdsh login      # ChatGPT subscription, OpenAI or DeepSeek API key (see below)
./target/release/rdsh            # opens the browser UI; new sessions start in the current directory
./target/release/rdsh -C ~/some/project --no-open   # just print the URL
./target/release/rdsh run "fix the failing test"   # one headless turn
./target/release/rdsh sessions   # list sessions; rdsh --resume last opens one
./target/release/rdsh trace last # readable trace (subagent traces are <id>-subN)
```

Without direnv, use `nix develop`. Options: `-m/--model` (env `RDSH_MODEL`), `-C/--cwd`, `-r/--resume`. rdsh also reads `.env` in its working directory.

**Providers and login.** `rdsh login` asks which provider to use; `rdsh login <provider>` skips the question:

- `chatgpt`: your ChatGPT subscription, through the same sign-in the Codex CLI uses. A browser opens and the redirect comes back to `localhost:1455`. If the browser runs on another machine, or the Codex CLI holds the port, paste the final redirect URL at the prompt. `--device` shows a code to enter at auth.openai.com instead, which suits SSH sessions. Requests go to `chatgpt.com/backend-api/codex` and the token refreshes on its own.
- `openai`: an OpenAI API key, used with the Responses API (`OPENAI_BASE_URL`, default `https://api.openai.com/v1`).
- `deepseek`: a DeepSeek API key (`DEEPSEEK_BASE_URL` points at any OpenAI-compatible endpoint).

Credentials live in `~/.config/rdsh/credentials.json` (0600). There is one DeepSeek key and one OpenAI login; a new ChatGPT or OpenAI login replaces the previous one. `DEEPSEEK_API_KEY` and `OPENAI_API_KEY` (environment or `.env`) override them. `rdsh logout [provider]` removes one login, or all of them. A hub that is already running picks up a new OpenAI login on its next request.

**Default model.** New sessions use `model` from the config (or `RDSH_MODEL`, or `-m`). Without one, they use the model of your last login (`gpt-6.2-sol` for ChatGPT and OpenAI, `deepseek-flash` for DeepSeek), as long as that provider is still logged in. `:model` offers the models of every provider you are logged into. `gpt-…`, `o3`-style and `codex-…` ids go to OpenAI, plus any listed in `openai_models`; everything else goes to the DeepSeek endpoint. Thinking effort maps to OpenAI's levels: off is `none` where the model allows it and `low` otherwise, and max is `max` on gpt-5.6 and gpt-6, `xhigh` on older models.

Optional `~/.config/rdsh/config.json` (all keys optional):

```json
{ "model": "deepseek-flash", "models": ["deepseek-flash", "gpt-6.2-sol"], "effort": "high", "max_steps": 200, "subagent_max_steps": 0,
  "openai_base_url": "https://api.openai.com/v1", "chatgpt_base_url": "https://chatgpt.com/backend-api", "openai_models": [],
  "max_tool_output_chars": 30000, "bash_timeout_secs": 120, "max_retries": 3,
  "stream_idle_timeout_secs": 180, "sessions_dir": "/path/to/sessions",
  "length_routing": true, "decision_url": null, "decision_model": null, "decision_timeout_ms": 2500 }
```

`max_steps` caps model requests per turn and `subagent_max_steps` per subagent task; 0 means no limit, which is the default for subagents.

**Response length.** With `OPENROUTER_API_KEY` (or `TYPESAFE_API_KEY`) set, each turn first asks
Jev, TypeSafe's decision model, how long the reply should be (`yesno`, `brief`, `standard`, `detailed`),
judging from your last few prompts, the start of the last reply and the new prompt. A
`<response_length>` note goes after your message; the status line shows the pick (`len brief 0.86`).
A pick below 0.5 confidence, a timeout or an error adds no note. Subagents and `rdsh run` are never
routed. `decision_url` points at another System One endpoint; a localhost one needs no key.

**Web search.** With `KAGI_API_KEY` set (environment or `.env`; keys at https://kagi.com/api/keys), the agent and its subagents get `web_search` (Kagi Search API: query, optional `limit`, `recency` of day/week/month, and `sites`) and `web_fetch` (Kagi Extract API: up to 10 pages as markdown). Without a key the tools are not offered. Both are billed to your Kagi API account per call. `KAGI_BASE_URL` points them at another endpoint.

**Background jobs.** A `bash` command still running after `bash_timeout_secs` (default 120) keeps running as a background job instead of being killed, and the model can start one directly with `run_in_background` (servers, watchers). Job output goes to a log under `$TMPDIR/rdsh-jobs/<session>/`; `job_output` reads what is new and `job_kill` stops a job. Each turn starts with a one-line note of the jobs still running and for how long. When a job finishes, the model is told right away, with the last lines of its output: before its next request mid-turn, or in a turn of its own when the session is idle (shown as a status line, not as your message). A `timeout_secs` the model passes still kills the command. Jobs keep the hub from exiting as idle, and they stop when the hub stops (a restart). `rdsh run` waits for its jobs before exiting. Subagents keep the old behavior: their commands are killed at the limit.

## Keys

The page is a monospace grid with vim keys. It starts in NORMAL mode: `i` (or a click in the prompt) types, Enter sends, Shift-Enter or Ctrl-j inserts a newline, Esc leaves. `:help` lists everything.

Messages sent during a turn wait in order, listed above the prompt (up to four, then a "+N more" row); the count on the separator excludes the running turn. Ctrl-Enter (or `:now`) starts them all at once: the hub joins the waiting messages into one, in order, and interrupts the running turn. A message that answers an `ask` form is never hurried this way. Press ↑ in an empty prompt to recall the newest queued message; ↑ and ↓ move between unchanged recalls. Enter saves an edit without changing its place in line, Ctrl-Enter saves it and then starts the queue, and Esc discards it. The queue keeps running while you edit; if the message starts before you save, your changes stay in the prompt as an unsent draft.

**Path completion.** Tab in the prompt completes the path before the cursor, relative to the session's directory on its host. Files git ignores are left out, and hidden ones show only once you type the dot. With several matches their shared part goes in first and a menu opens (Tab, ↑ ↓ or Ctrl-n/p move, Enter picks, Esc closes).

| Keys | Action |
| --- | --- |
| `h j k l` `w b e` `0 ^ $` `gg G 42G` | move |
| `Ctrl-d/u/f/b`, `Ctrl-e/y`, `H M L`, `zz zt zb` | scroll |
| `{ }` | previous/next message |
| `/ ? n N` | search (smartcase) |
| `v` / `V` | charwise / linewise select; `y` copies |
| `c` (or Enter in visual) | comment on the selection (or the current line) |
| Ctrl-Enter (any mode), `:now` | send queued comments plus any prompt text right away: a running turn is interrupted and every waiting message starts at once, joined into one (Enter waits for the turn instead) |
| Shift-Tab (any mode) | cycle thinking effort: off, low, high, max (in an open file it switches the diff base instead); the model name's color shows it |
| Enter / `gf` / click on a path | open the file view |
| `gx` | open the link under the cursor (web links in a new tab); off any link, the line's first one |
| Enter on a `task` | open the subagent's session; `Ctrl-o`, or `q` in it, comes back |
| Tab / `za` / Enter | expand or collapse tool output and reasoning |
| `Q` | comment queue (`e` edit, `dd` delete, `D` clear) |
| `F`, `:files` | files with uncommitted changes (`git status` on the session's host); Enter opens one, `q` in it comes back to the list |
| `S`, `Ctrl-Space h` / `Ctrl-Space l` | show or hide the session rail; move focus into it and back (`j k g G` with counts like `3j`, `zh` shows subagents that finished over 5 minutes ago, Enter switches to the session, resuming it if needed; subagent rows open read-only; `y` copies the id) |
| `:new` | start another session |
| `:model`, `:model <id>` | pick this session's model from a list (`models` in the config, default: every logged-in provider's; the current one is marked, Enter picks, `q` backs out), or name any id; it applies from the next turn and a resumed session keeps it |
| `:host user@box[:/dir]` | add a remote machine to the rail over SSH, start its rdsh hub if needed, and start a session there |
| `:restart` | ask the current host's hub to drain and exit; the page reconnects to a new hub on the latest binary (also happens automatically when a newer client connects) |
| `:e path[:line]`, `:help` | open a file; list the keys |
| Ctrl-C | interrupt the turn |

**The file view** shows the diff against HEAD first; untracked files show as fully added. `Tab` switches to the whole file with changed lines marked. `Shift-Tab` compares against the default branch instead (`origin/HEAD`, else `origin/main` or `origin/master`, from where the branch left it, as a pull request shows it) and back to HEAD. `]`/`[` move between changes, `r` reloads, `q` or `Esc` goes back. Removed and added lines are tinted, and within a changed line the words that changed get a stronger tint.

**Review comments.** `c` comments on the line under the cursor, `v`/`V` then `c` on a selection, Enter on lines in the file view. Each quotes what it is about (chat text, or file lines with their numbers and diff signs), and commented file lines show `●`. They go out with your next message (or alone, with an empty prompt) inside a `<review_comments>` block.

## Layout

- `src/agent.rs`: agent loop; parallel-safe tool calls run concurrently
- `src/llm.rs`: chat types, wire format, scripted provider for tests
- `src/providers.rs`, `src/deepseek.rs`, `src/openai.rs`, `src/chatgpt.rs`: model routing, the two streaming clients, ChatGPT sign-in
- `src/tools/`: `read_file`, `write_file`, `edit_file`, `bash`, `job_output`, `job_kill` (`jobs.rs`), `glob`, `grep`, `web_search`, `web_fetch` (`web.rs`, Kagi), `ask`, `task` (subagents)
- `src/trace.rs`: JSONL traces; resume rebuilds the exact model-visible history
- `src/hub/`: the session hub, its protocol, path checks and completion; `src/diff.rs`: file diffs
- `src/webview.rs`: the web server (HTTP, login, the WebSocket relay to the hubs); `web/src/`: the browser client (`app.ts` state, `keys.ts`, `view.ts`, `layout.ts` grid and markdown, `session.ts` agent events, `fileview.ts`, `comments.ts`, `worddiff.ts`, `rail.ts`, `diagram.ts`)

`cargo test` runs the unit tests and the agent-loop, hub and web integration tests (scripted provider); `cd web && npm test` runs the browser client's tests.

## Sessions, the hub, and other machines

Sessions run in a background **hub**, not in the page. `rdsh` starts the hub on first use (`rdsh serve`, listening on `$XDG_RUNTIME_DIR/rdsh/hub.sock`, a directory only you can read) and attaches to it. Closing the tab leaves running sessions running; the next visit attaches again, and several tabs can watch the same session. The hub exits after `serve_idle_secs` (default 1800) with no clients and no running turns.

A new session joins the rail only when you send its first message. Sessions you never type into stay out of the rail, and their empty traces are deleted when the hub shuts down.

Other machines are reached over SSH. The remote machine needs rdsh installed and logged in (`rdsh login` there; its own key is used):

```sh
rdsh --host me@build-box                    # attach to build-box's hub too
rdsh --host me@build-box:/srv/app           # new sessions there start in /srv/app; the rail lists only those
```

`--host` runs `ssh -T me@build-box rdsh connect`, which relays the hub protocol (newline-delimited JSON) over the SSH connection; SSH does all authentication and encryption, and no hub ever listens on the network. Hosts can also be configured once:

```json
{ "hosts": [ { "name": "build", "ssh": "me@build-box", "dir": "/srv/app" } ],
  "ssh_program": "ssh", "remote_command": ["rdsh", "connect"] }
```

If rdsh is not on the remote `PATH` for non-interactive shells, set `remote_command` to its full path, e.g. `["/home/me/.cargo/bin/rdsh", "connect"]`.

With several hosts the rail groups sessions under a heading per host (`◆ build`; `✗ build (disconnected)` when the connection drops). Enter on a heading starts a session on that host. Everything about a remote session happens on its host: its agent and tools run there, and file links and diffs (`Enter`/`gf`, `r` to reload) are checked and read there. The status line shows `host @ session`.

## The web server

`rdsh` (or `rdsh web`) serves the page on 127.0.0.1:7357 (another free port when taken) and opens it; `--no-open` only prints the URL, `--port N` picks the port, and `--host` attaches remote machines as above. For each open page it connects to the local hub and to every configured host (over SSH), and relays them over one WebSocket. Hubs still never listen on the network. The browser keeps `Ctrl-w`, so window commands use `Ctrl-Space`. The URL names the session on screen, so a reload or bookmark comes back to it, and `rdsh -r ID` opens that session.

Access: the server binds `127.0.0.1` only. The URL it prints carries a token from `$XDG_RUNTIME_DIR/rdsh/web.token` (made once, so tabs survive restarts); the first visit swaps it for an HttpOnly, SameSite=Strict cookie. Requests must name `127.0.0.1` or `localhost` as their host, and the WebSocket must come from the page's own origin with the cookie, so other sites open in the same browser can't drive your agents. To use it from another machine, forward the port: `ssh -L 7357:127.0.0.1:7357 box` and open the printed URL locally.

**Diagrams.** Turns sent from the browser tell the model, once, that it can draw: a ```` ```d3 ```` block in its reply becomes an interactive diagram. While the block streams, the browser shows a “Generating diagram…” spinner instead of the source. The script gets `d3` (v7), `svg`, `g` (pan/zoom layer), `W`, `H`, `C` (palette), `CH` (character width) and `pick(label)`, which puts a clicked item into your prompt as context. `// rows: N` sets the height, `// title: …` the caption. `Enter` on a diagram gives it the keys (`hjkl` pan, `+`/`-` zoom, `0` reset, `Esc` leaves). A script that throws shows its error, and the error goes to the model with your next message so it can fix it.

Diagram scripts are model-written, so each runs isolated: in a sandboxed iframe with an opaque origin (no access to the page, its cookie, storage or the WebSocket; no popups, forms or tab navigation), loaded from its own `<random>.localhost` name so the browser gives it its own process (a hung script is removed after a few seconds without freezing the page or other diagrams), under a CSP that allows no network (D3 is served by `rdsh web`), and removed if it navigates itself away. Its messages to the page are limited to focusing itself, adding a pick to the prompt (never sending it), and `Esc`/`Ctrl-Space` to leave: a script cannot type, send or run commands.

**Files.** Paths in the chat and in tool output become links only when the session's host confirms they are files: the page asks the hub about path-like words (relative to the session's directory, on the machine the session runs on) and caches the answers, so `something/else` stays plain text. Each `edit_file` call shows as a diff of what it replaced, numbered from where it landed, with changed words marked as in the file view; edits over 40 lines start folded (`Tab` unfolds).

**Questions.** In sessions a client is attached to, the agent has an `ask` tool: it puts one or more forms in front of you (text, paragraph, single or multiple choice with an optional "other", yes/no, number; defaults it proposes; optional fields) and waits. In the browser the form docks above the prompt: Tab moves between fields, Enter goes to the next, Ctrl-Enter to the next form or submits, Esc returns to the chat with the form still waiting, `i` comes back to it. You can also answer in your own words (`o`, or "reply in words"): the next message you send is the answer. Every client watching the session sees the form, and the first answer wins. `rdsh run` and subagents don't get the tool.

The page uses DejaVu Sans Mono, served by `rdsh web` (license in `web/src/fonts/LICENSE`), so it looks the same on every machine and loads nothing from other sites.

The client is TypeScript in `web/src`, bundled by esbuild into `web/dist`, which is committed and compiled into the binary, so building rdsh does not need Node. To work on it (Node comes with the dev shell):

```sh
cd web && npm install
npm run watch                               # rebuilds web/dist on change
RDSH_WEB_DIR=web/dist cargo run            # serve from disk: reload the page to see changes
npm run check && npm test                   # types, unit tests
```
