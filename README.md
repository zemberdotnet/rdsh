# rdsh

A fast, low-footprint terminal coding agent for DeepSeek models, written in Rust (about 9 MB of resident memory).
It has an agent loop, file, shell and search tools, parallel subagents and JSONL traces. Review is
built in: open any file path mentioned in the chat to see its diff, comment on lines or on selected
chat text, and send all queued comments, quoted, with your next message.

## Run

```sh
direnv allow                     # loads flake.nix (cargo, rustc, clippy) and ./.env
echo 'DEEPSEEK_API_KEY=sk-...' > .env
cargo build --release
./target/release/rdsh            # TUI in the current directory
./target/release/rdsh -C ~/some/project
./target/release/rdsh run "fix the failing test"   # one headless turn
./target/release/rdsh sessions   # list sessions; rdsh --resume last continues one
./target/release/rdsh trace last # readable trace (subagent traces are <id>-subN)
```

Without direnv, use `nix develop`. Options: `-m/--model` (default `deepseek-chat`, env `RDSH_MODEL`), `-C/--cwd`, `-r/--resume`. `DEEPSEEK_BASE_URL` points at any OpenAI-compatible endpoint. rdsh also reads `.env` in its working directory.

Optional `~/.config/rdsh/config.json` (all keys optional):

```json
{ "model": "deepseek-chat", "max_steps": 100, "subagent_max_steps": 50,
  "max_tool_output_chars": 30000, "bash_timeout_secs": 120, "max_retries": 3,
  "stream_idle_timeout_secs": 180, "sessions_dir": "/path/to/sessions" }
```

## Keys

The app starts in INSERT mode: type, then press Enter to send (Alt-Enter or Ctrl-J inserts a newline). Esc switches to NORMAL mode.

| Keys | Action |
| --- | --- |
| `h j k l` `w b e` `0 ^ $` `gg G 42G` | move |
| `Ctrl-d/u/f/b`, `Ctrl-e/y`, `H M L`, `zz zt zb` | scroll |
| `{ }` / `[[ ]]` | previous/next message (in a file: hunk; also `]c [c`) |
| `/ ? n N` | search (smartcase) |
| `v` / `V` | charwise / linewise select; `o` swaps ends, `y` yanks via OSC 52 |
| `c` (or Enter in visual) | comment on the selection (or the current line) |
| Enter / `gf` / mouse click on a path | open the file's diff view |
| Tab / `za` | expand or collapse tool output and reasoning |
| `Q` | comment queue (`e` edit, `dd` delete, `D` clear) |
| `F` | list changed files (`git status`) |
| `:e path[:line]`, `:send`, `:help`, `:q` | commands |
| Ctrl-C | interrupt the turn, clear input, or quit (press twice) |

In the file view, the diff against HEAD is shown first; untracked files show as fully added. Tab switches to the full file with changed lines marked. `v`/`V` then `c`, or Enter, comments on lines, and commented lines show `●`. `r` reloads; `q` goes back.

Queued comments are sent with your next message inside a `<review_comments>` block. Each comment quotes the chat text or diff lines it refers to.

## Layout

- `src/agent.rs`: agent loop; parallel-safe tool calls run concurrently
- `src/llm.rs`: chat types, wire format, scripted provider for tests
- `src/deepseek.rs`: streaming SSE client with retries
- `src/tools/`: `read_file`, `write_file`, `edit_file`, `bash`, `glob`, `grep`, `task` (subagents)
- `src/trace.rs`: JSONL traces; resume rebuilds the exact model-visible history
- `src/comments.rs`, `src/links.rs`, `src/diff.rs`: review features
- `src/tui/`: UI (`app.rs` state and keys, `render.rs`, `transcript.rs` wrapped rows, `motion.rs` vim motions, `fileview.rs`)

`cargo test` runs the unit tests, the agent-loop integration tests (scripted provider) and the TUI behavior tests (key events against a test backend).
