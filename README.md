# rdsh

A fast, low-footprint terminal coding agent for DeepSeek models, written in Rust (about 9 MB of resident memory).
It has an agent loop, file, shell and search tools, parallel subagents and JSONL traces. Review is
built in: open any file path mentioned in the chat to see its diff, comment on lines or on selected
chat text, and send all queued comments, quoted, with your next message.

## Run

```sh
direnv allow                     # loads flake.nix (cargo, rustc, clippy) and ./.env
cargo build --release
./target/release/rdsh login      # paste your key; it is verified and saved to ~/.config/rdsh/credentials.json (0600)
./target/release/rdsh            # TUI in the current directory
./target/release/rdsh -C ~/some/project
./target/release/rdsh run "fix the failing test"   # one headless turn
./target/release/rdsh sessions   # list sessions; rdsh --resume last continues one
./target/release/rdsh trace last # readable trace (subagent traces are <id>-subN)
```

Without direnv, use `nix develop`. `DEEPSEEK_API_KEY` (environment or `.env`) overrides the stored login; `rdsh logout` removes it. Options: `-m/--model` (default `deepseek-flash`, env `RDSH_MODEL`), `-C/--cwd`, `-r/--resume`. `DEEPSEEK_BASE_URL` points at any OpenAI-compatible endpoint. rdsh also reads `.env` in its working directory.

Optional `~/.config/rdsh/config.json` (all keys optional):

```json
{ "model": "deepseek-flash", "models": ["deepseek-flash", "deepseek-v4-pro"], "effort": "high", "max_steps": 200, "subagent_max_steps": 50,
  "max_tool_output_chars": 30000, "bash_timeout_secs": 120, "max_retries": 3,
  "stream_idle_timeout_secs": 180, "sessions_dir": "/path/to/sessions",
  "length_routing": true, "decision_url": null, "decision_model": null, "decision_timeout_ms": 2500 }
```

**Response length.** With `OPENROUTER_API_KEY` (or `TYPESAFE_API_KEY`) set, each TUI turn first asks
Jev, TypeSafe's decision model, how long the reply should be (`yesno`, `brief`, `standard`, `detailed`),
judging from your last few prompts, the start of the last reply and the new prompt. A
`<response_length>` note goes after your message; the status line shows the pick (`len brief 0.86`).
A pick below 0.5 confidence, a timeout or an error adds no note. Subagents and `rdsh run` are never
routed. `decision_url` points at another System One endpoint; a localhost one needs no key.

**Web search.** With `KAGI_API_KEY` set (environment or `.env`; keys at https://kagi.com/api/keys), the agent and its subagents get `web_search` (Kagi Search API: query, optional `limit`, `recency` of day/week/month, and `sites`) and `web_fetch` (Kagi Extract API: up to 10 pages as markdown). Without a key the tools are not offered. Both are billed to your Kagi API account per call. `KAGI_BASE_URL` points them at another endpoint.

**Background jobs.** A `bash` command still running after `bash_timeout_secs` (default 120) keeps running as a background job instead of being killed, and the model can start one directly with `run_in_background` (servers, watchers). Job output goes to a log under `$TMPDIR/rdsh-jobs/<session>/`; `job_output` reads what is new and `job_kill` stops a job. Each turn starts with a one-line note of the jobs still running and for how long. When a job finishes, the model is told right away, with the last lines of its output: before its next request mid-turn, or in a turn of its own when the session is idle (shown as a status line, not as your message). A `timeout_secs` the model passes still kills the command. Jobs keep the hub from exiting as idle, and they stop when the hub stops (a restart, or quitting the TUI with `--no-daemon`). `rdsh run` waits for its jobs before exiting. Subagents keep the old behavior: their commands are killed at the limit.

## Keys

The app starts in INSERT mode: type, then press Enter to send (Shift-Enter inserts a newline, in the prompt and in comments; Alt-Enter and Ctrl-J also work in terminals without the kitty keyboard protocol). Esc switches to NORMAL mode.

| Keys | Action |
| --- | --- |
| `h j k l` `w b e` `0 ^ $` `gg G 42G` | move |
| `Ctrl-d/u/f/b`, `Ctrl-e/y`, `H M L`, `zz zt zb` | scroll |
| `{ }` / `[[ ]]` | previous/next message (in a file: hunk; also `]c [c`) |
| `/ ? n N` | search (smartcase) |
| `v` / `V` | charwise / linewise select; `o` swaps ends, `y` yanks via OSC 52 |
| `c` (or Enter in visual) | comment on the selection (or the current line) |
| Ctrl-Enter (any mode), `:now` | send queued comments plus any prompt text right away: a running turn is interrupted so the message starts at once (Enter waits for the turn instead); inside a comment, save it then send |
| Shift-Tab (any mode) | cycle thinking effort: off, low, high, max. The model name in the status line shows it: gray, light blue, blue, deep purple |
| Enter / `gf` / mouse click on a path | open the file's diff view |
| Tab / `za` | expand or collapse tool output and reasoning |
| `Q` | comment queue (`e` edit, `dd` delete, `D` clear) |
| `F` | list changed files (`git status`) |
| `S`, `Ctrl-w h` / `Ctrl-w l` | show or hide the session rail; move focus into it and back (`j k g G`, Enter switches to the session, resuming it if needed; subagent rows open read-only; `y` copies the id) |
| `:new`, `:q`, `:q!` | start another session; quit (refuses while other sessions run), quit and stop them |
| `:model`, `:model <id>` | pick this session's model from a list (`models` in the config; Enter picks, `q` backs out), or name any id; it applies from the next turn and a resumed session keeps it |
| `:host user@box[:/dir]` | add a remote machine to the rail over SSH, start its rdsh hub if needed, and start a session there |
| `:restart` | ask the current host's hub to drain and exit; the next connection starts the latest binary (also happens automatically when a newer client connects) |
| `:e path[:line]`, `:send`, `:help`, `:q` | commands |
| Ctrl-C | interrupt the turn, clear input, or quit (press twice) |

In the file view, the diff against HEAD is shown first; untracked files show as fully added. Code is syntax-highlighted with tree-sitter (Rust, Python, JavaScript, TypeScript and TSX, Go, C, C++, JSON, Bash, TOML, YAML, Nix, Markdown). Deleted lines are highlighted from the HEAD version, and changed lines get a tinted background. Tab switches to the full file with changed lines marked. `v`/`V` then `c`, or Enter, comments on lines, and commented lines show `●`. `r` reloads; `q` goes back.

Queued comments are sent with your next message inside a `<review_comments>` block. Each comment quotes the chat text or diff lines it refers to.

## Layout

- `src/agent.rs`: agent loop; parallel-safe tool calls run concurrently
- `src/llm.rs`: chat types, wire format, scripted provider for tests
- `src/deepseek.rs`: streaming SSE client with retries
- `src/tools/`: `read_file`, `write_file`, `edit_file`, `bash`, `job_output`, `job_kill` (`jobs.rs`), `glob`, `grep`, `web_search`, `web_fetch` (`web.rs`, Kagi), `task` (subagents)
- `src/trace.rs`: JSONL traces; resume rebuilds the exact model-visible history
- `src/comments.rs`, `src/links.rs`, `src/diff.rs`: review features
- `src/tui/`: UI (`app.rs` state and keys, `render.rs`, `transcript.rs` wrapped rows, `motion.rs` vim motions, `fileview.rs`)

`cargo test` runs the unit tests, the agent-loop integration tests (scripted provider) and the TUI behavior tests (key events against a test backend).

## Sessions, the hub, and other machines

Sessions run in a background **hub**, not in the TUI. `rdsh` starts the hub on first use (`rdsh serve`, listening on `$XDG_RUNTIME_DIR/rdsh/hub.sock`, a directory only you can read) and attaches to it. Closing the TUI leaves running sessions running; the next `rdsh` attaches again, and several terminals can watch the same session. The hub exits after `serve_idle_secs` (default 1800) with no clients and no running turns. `--no-daemon` runs sessions inside the TUI process instead.

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

With several hosts the rail groups sessions under a heading per host (`◆ build`; `✗ build (disconnected)` when the connection drops). Enter on a heading starts a session on that host. Everything about a remote session happens on its host: its agent and tools run there, and file links, diffs (`Enter`/`gf`, `r` to reload) and the changed-files list (`F`) are read there. The status line shows `host @ session`.
