//! Behavior tests driving the App with real key events and rendering to a test backend.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{Terminal, backend::TestBackend};
use tokio::sync::mpsc;

use super::app::{App, Host, HostEvent, Mode, View};
use crate::{
    agent::AgentEvent,
    hub::{Notice, QueuedTurn, Request, Snapshot},
    llm::{Effort, Usage},
    llm::{Message, ToolCall},
    tools::ToolOutput,
};

fn snapshot(id: &str) -> Snapshot {
    Snapshot { session_id: id.into(), model: "test-model".into(), cwd: String::new(), history: Vec::new(), turn: Vec::new(), running: false, usage: Usage::default(), queued: Vec::new() }
}

fn queued(turns: &[(u64, &str)]) -> Vec<QueuedTurn> {
    turns.iter().map(|&(id, text)| QueuedTurn { id, text: text.into() }).collect()
}

fn hello() -> Notice {
    Notice::Hello { version: env!("CARGO_PKG_VERSION").into() }
}

struct Harness {
    app: App,
    term: Terminal<TestBackend>,
    rx: mpsc::UnboundedReceiver<Request>,
    _dir: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git = |args: &[&str]| {
            std::process::Command::new("git").arg("-C").arg(root).args(args).output().unwrap();
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        // Don't sign synthetic commits: a global `commit.gpgsign` with `gpg.format=ssh`
        // would otherwise prompt the SSH-agent (e.g. 1Password) during tests.
        git(&["config", "commit.gpgsign", "false"]);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        std::fs::write(root.join("src/lib.rs"), "fn a() {}\nfn b2() {}\nfn c() {}\nfn d() {}\n").unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = App::new(root.to_path_buf(), vec![Host::new("local", true, None, tx)]);
        app.expect_open(0);
        app.on_notice(0, hello());
        assert_eq!(rx.try_recv().unwrap(), Request::WatchQueue);
        app.on_notice(0, Notice::Opened { snapshot: snapshot("s1") });
        let term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut h = Self { app, term, rx, _dir: dir };
        h.draw();
        h
    }

    fn draw(&mut self) {
        let app = &mut self.app;
        self.term.draw(|f| app.draw(f)).unwrap();
    }

    fn key(&mut self, code: KeyCode) {
        self.app.on_event(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
        self.draw();
    }

    fn keys(&mut self, s: &str) {
        for c in s.chars() {
            self.key(KeyCode::Char(c));
        }
    }

    fn agent(&mut self, ev: AgentEvent) {
        self.app.on_agent(ev);
        self.draw();
    }

    fn screen(&self) -> String {
        let buf = self.term.backend().buffer();
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }

    /// The next `Send` request; acts as the hub by starting that turn right away.
    fn sent(&mut self) -> String {
        loop {
            match self.rx.try_recv() {
                Ok(Request::Send { session_id, text, .. }) => {
                    self.notice(Notice::Event { session_id, event: AgentEvent::TurnStarted { text: text.clone() } });
                    return text;
                }
                Ok(_) => continue,
                Err(e) => panic!("no turn sent: {e}"),
            }
        }
    }

    fn notice(&mut self, n: Notice) {
        self.app.on_notice(0, n);
        self.draw();
    }

    fn queue(&mut self, session_id: &str, turns: &[(u64, &str)]) {
        self.notice(Notice::Queue { session_id: session_id.into(), queued: queued(turns) });
    }

    fn edit_ack(&mut self, session_id: &str, turn_id: u64, updated: bool) {
        self.notice(Notice::QueueEdited { session_id: session_id.into(), turn_id, updated });
    }

    /// Feeds the rail the sessions recorded in `dir`, as the hub would.
    fn sessions(&mut self, dir: &std::path::Path, running: &[&str]) {
        let recorded = crate::trace::list_sessions(dir);
        let open = recorded.iter().map(|s| s.id.clone()).collect();
        let running = running.iter().map(|s| s.to_string()).collect();
        self.notice(Notice::Sessions { recorded, open, running });
    }

    /// Simulates a finished turn whose answer mentions a file and ran one tool.
    fn complete_turn(&mut self) {
        self.agent(AgentEvent::TextDelta("I changed src/lib.rs:2 and ".into()));
        self.agent(AgentEvent::TextDelta("renamed b to b2.\nSecond line here.".into()));
        let msg = Message::Assistant {
            content: "I changed src/lib.rs:2 and renamed b to b2.\nSecond line here.".into(),
            reasoning: None,
            tool_calls: vec![ToolCall { id: "c1".into(), name: "bash".into(), arguments: r#"{"command":"cargo test"}"#.into() }],
            replay: vec![],
        };
        self.agent(AgentEvent::Assistant { message: msg, usage: None });
        self.agent(AgentEvent::ToolStart {
            call: ToolCall { id: "c1".into(), name: "bash".into(), arguments: r#"{"command":"cargo test"}"#.into() },
        });
        self.agent(AgentEvent::ToolEnd {
            call_id: "c1".into(),
            output: ToolOutput::ok("test result: ok\nsrc/lib.rs:4: fn d"),
            duration_ms: 1200,
        });
        self.agent(AgentEvent::TurnEnd { cancelled: false, error: None });
    }
}

#[test]
fn sending_a_message_starts_a_turn_and_streams_the_answer() {
    let mut h = Harness::new();
    assert_eq!(h.app.mode, Mode::Insert);
    h.keys("hello");
    h.key(KeyCode::Enter);
    assert_eq!(h.sent(), "hello");
    assert!(h.app.cur.running);
    h.complete_turn();
    assert!(!h.app.cur.running);
    let screen = h.screen();
    assert!(screen.contains("› hello"), "{screen}");
    assert!(screen.contains("● I changed src/lib.rs:2"), "{screen}");
    assert!(screen.contains("✓ bash cargo test  (1.2s)"), "{screen}");
}

#[test]
fn visual_selection_comments_are_queued_and_sent_with_quotes() {
    let mut h = Harness::new();
    h.keys("start");
    h.key(KeyCode::Enter);
    h.sent();
    h.complete_turn();
    h.key(KeyCode::Esc);
    assert_eq!(h.app.mode, Mode::Normal);
    // Jump to the assistant message: gg, then } to the next entry.
    h.keys("gg}");
    // Select "renamed b to b2" charwise: search for it, then v and four word-ends.
    h.keys("/renamed");
    h.key(KeyCode::Enter);
    h.keys("v4e");
    h.key(KeyCode::Char('c'));
    assert_eq!(h.app.mode, Mode::Comment);
    assert!(h.screen().contains("Comment on your response"));
    h.keys("why rename?");
    h.key(KeyCode::Enter);
    assert_eq!(h.app.cur.queue.len(), 1);
    assert!(h.screen().contains("1 comment queued"));
    // Linewise comment on the second line.
    h.keys("jV");
    h.key(KeyCode::Char('c'));
    h.keys("drop this");
    h.key(KeyCode::Enter);
    assert_eq!(h.app.cur.queue.len(), 2);
    // Send: i, text, Enter.
    h.keys("i");
    h.keys("and run tests");
    h.key(KeyCode::Enter);
    let sent = h.sent();
    assert!(sent.contains("[1] On your response:\n> renamed b to b2\nComment: why rename?"), "{sent}");
    assert!(sent.contains("[2] On your response:\n> Second line here.\nComment: drop this"), "{sent}");
    assert!(sent.ends_with("</review_comments>\n\nand run tests"));
    assert!(h.app.cur.queue.is_empty());
}

#[test]
fn file_links_open_diff_view_and_line_comments_queue() {
    let mut h = Harness::new();
    h.keys("start");
    h.key(KeyCode::Enter);
    h.sent();
    h.complete_turn();
    h.key(KeyCode::Esc);
    h.keys("gg}");
    // Cursor on "I changed src/lib.rs:2 …": move onto the path and press Enter.
    h.keys("2w");
    h.key(KeyCode::Enter);
    let View::File(fv) = &h.app.view else { panic!("expected file view") };
    assert_eq!(fv.file.rel, "src/lib.rs");
    assert!(!fv.full);
    // Link line 2 → cursor on the added `fn b2() {}` line.
    assert_eq!(fv.lines[fv.cursor.pos.row].text, "fn b2() {}");
    let screen = h.screen();
    assert!(screen.contains("src/lib.rs — +2 -1 — diff view"), "{screen}");
    assert!(screen.contains("+ fn b2() {}"), "{screen}");
    // Enter in the file view comments on the current line.
    h.key(KeyCode::Enter);
    h.keys("use a better name");
    h.key(KeyCode::Enter);
    assert!(h.screen().contains("●+ fn b2() {}"), "{}", h.screen());
    // Tab switches to the full file.
    h.key(KeyCode::Tab);
    let View::File(fv) = &h.app.view else { panic!() };
    assert!(fv.full);
    assert_eq!(fv.lines.len(), 4);
    h.key(KeyCode::Char('q'));
    assert!(matches!(h.app.view, View::Chat));
    h.keys("i");
    h.key(KeyCode::Enter);
    let sent = h.sent();
    assert!(sent.contains("[1] On src/lib.rs:2 (current diff):\n```diff\n+fn b2() {}\n```\nComment: use a better name"), "{sent}");
}

#[test]
fn clicking_a_path_in_tool_output_opens_it() {
    let mut h = Harness::new();
    h.keys("start");
    h.key(KeyCode::Enter);
    h.sent();
    h.complete_turn();
    let screen = h.screen();
    let (y, line) = screen.lines().enumerate().find(|(_, l)| l.contains("│ src/lib.rs:4")).expect("tool output row");
    let x = line.chars().position(|c| c == 's').unwrap() as u16 + 1;
    h.app.on_event(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: x,
        row: y as u16,
        modifiers: KeyModifiers::NONE,
    }));
    h.draw();
    let View::File(fv) = &h.app.view else { panic!("click did not open file:\n{screen}") };
    assert_eq!(fv.lines[fv.cursor.pos.row].text, "fn d() {}");
}

#[test]
fn queue_view_edits_and_deletes() {
    let mut h = Harness::new();
    h.keys("start");
    h.key(KeyCode::Enter);
    h.sent();
    h.complete_turn();
    h.key(KeyCode::Esc);
    h.keys("gg}");
    for body in ["one", "two"] {
        h.key(KeyCode::Char('c'));
        h.keys(body);
        h.key(KeyCode::Enter);
    }
    h.key(KeyCode::Char('Q'));
    assert!(h.screen().contains("Comment queue — 2"));
    h.keys("j");
    h.key(KeyCode::Char('e'));
    h.app.on_event(Event::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)));
    h.keys("TWO");
    h.key(KeyCode::Enter);
    assert_eq!(h.app.cur.queue[1].body, "TWO");
    h.keys("ggdd");
    assert_eq!(h.app.cur.queue.len(), 1);
    assert_eq!(h.app.cur.queue[0].body, "TWO");
}

#[test]
fn messages_typed_during_a_turn_wait_for_it() {
    let mut h = Harness::new();
    h.keys("first");
    h.key(KeyCode::Enter);
    h.sent();
    // The message goes out right away, but the server alone reports what is pending.
    h.keys("second");
    h.key(KeyCode::Enter);
    assert!(!h.screen().contains("1 queued"));
    assert!(matches!(h.request(), Request::Send { text, .. } if text == "second"));
    h.queue("s1", &[(7, "second"), (8, "from another client")]);
    let screen = h.screen();
    assert!(screen.contains("2 queued · ↑ recall · Ctrl-Enter sends now"), "{screen}");
    h.agent(AgentEvent::TurnEnd { cancelled: false, error: None });
    assert!(h.screen().contains("2 queued"), "still pending until a Queue notice");
    h.queue("s1", &[(8, "from another client")]);
    h.agent(AgentEvent::TurnStarted { text: "second".into() });
    assert!(h.screen().contains("1 queued"), "the running turn is not counted");
    assert!(h.screen().contains("› second"));
    h.queue("s1", &[]);
    assert!(!h.screen().contains("1 queued"));
}

#[test]
fn ctrl_enter_during_a_turn_interrupts_it_for_the_new_message() {
    let ctrl_enter = || Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL));
    let mut h = Harness::new();
    h.keys("first");
    h.key(KeyCode::Enter);
    h.sent();
    // Ctrl-Enter sends the prompt, then merges the queue and interrupts the running turn so it starts at once.
    h.keys("second");
    h.app.on_event(ctrl_enter());
    assert!(matches!(h.rx.try_recv(), Ok(Request::Send { text, .. }) if text == "second"));
    assert!(matches!(h.rx.try_recv(), Ok(Request::RunQueuedNow { session_id }) if session_id == "s1"));
    h.agent(AgentEvent::TurnEnd { cancelled: true, error: None });
    h.agent(AgentEvent::TurnStarted { text: "second".into() });
    // A message sent with Enter waits; Ctrl-Enter with an empty prompt (or :now) hurries it.
    h.keys("third");
    h.key(KeyCode::Enter);
    assert!(matches!(h.rx.try_recv(), Ok(Request::Send { text, .. }) if text == "third"));
    assert!(h.rx.try_recv().is_err());
    h.queue("s1", &[(3, "third")]);
    h.key(KeyCode::Esc);
    h.keys(":now");
    h.key(KeyCode::Enter);
    assert!(matches!(h.rx.try_recv(), Ok(Request::RunQueuedNow { .. })));
    h.agent(AgentEvent::TurnEnd { cancelled: true, error: None });
    h.queue("s1", &[]);
    h.agent(AgentEvent::TurnStarted { text: "third".into() });
    // Nothing typed or waiting: nothing is sent or interrupted.
    h.app.on_event(ctrl_enter());
    assert!(h.rx.try_recv().is_err());
}

#[test]
fn up_recalls_queued_turns_before_history_without_overwriting_drafts() {
    let mut h = Harness::new();
    let mut snap = snapshot("s1");
    snap.history.push(Message::user("history"));
    snap.queued = queued(&[(1, "oldest"), (2, "middle"), (3, "latest\nsecond line")]);
    h.notice(Notice::Opened { snapshot: snap });
    h.keys("independent draft");
    h.key(KeyCode::Up);
    assert_eq!(h.app.cur.prompt.text, "independent draft");
    h.ctrl('u');

    h.key(KeyCode::Up);
    assert_eq!(h.app.cur.prompt.text, "latest\nsecond line");
    h.key(KeyCode::Up);
    assert_eq!(h.app.cur.prompt.cursor, "latest".len(), "within a multiline recall, Up moves the cursor first");
    h.key(KeyCode::Down);
    assert_eq!(h.app.cur.prompt.cursor, "latest\nsecond".len());
    h.key(KeyCode::Up);
    h.key(KeyCode::Up);
    assert_eq!(h.app.cur.prompt.text, "middle");
    h.key(KeyCode::Up);
    assert_eq!(h.app.cur.prompt.text, "oldest");
    h.key(KeyCode::Up);
    assert_eq!(h.app.cur.prompt.text, "oldest");
    h.key(KeyCode::Down);
    assert_eq!(h.app.cur.prompt.text, "middle");
    h.key(KeyCode::Down);
    assert_eq!(h.app.cur.prompt.text, "latest\nsecond line");
    h.key(KeyCode::Down);
    assert!(h.app.cur.prompt.text.is_empty());
    h.queue("s1", &[]);
    h.key(KeyCode::Up);
    assert_eq!(h.app.cur.prompt.text, "history");
    assert!(h.rx.try_recv().is_err());
}

#[test]
fn a_queued_edit_submits_the_same_id_not_an_ask_answer_or_review_comments() {
    let mut h = Harness::new();
    h.keys("first");
    h.key(KeyCode::Enter);
    h.sent();
    h.agent(AgentEvent::TextDelta("reviewable reply".into()));
    h.key(KeyCode::Esc);
    h.keys("gg}c");
    assert_eq!(h.app.mode, Mode::Comment, "{}", h.screen());
    h.keys("review this later");
    h.key(KeyCode::Enter);
    assert_eq!(h.app.cur.queue.len(), 1);
    h.queue("s1", &[(9, "older"), (10, "pending")]);
    h.keys("i");
    h.key(KeyCode::Up);
    assert!(h.screen().contains("Editing queued turn · Enter save · Esc discard"));
    h.keys(" edited");
    h.key(KeyCode::Up);
    h.key(KeyCode::Down);
    assert_eq!(h.app.cur.prompt.text, "pending edited", "arrows do not abandon a changed recall");
    h.agent(AgentEvent::Question { call_id: "ask1".into(), ask: crate::tools::ask::Ask { title: None, forms: vec![] } });
    h.key(KeyCode::Enter);
    assert_eq!(h.request(), Request::EditQueued { session_id: "s1".into(), turn_id: 10, text: "pending edited".into() });
    assert_eq!(h.app.cur.prompt.text, "pending edited");
    assert!(h.screen().contains("Saving edit"));
    h.key(KeyCode::Enter);
    assert!(h.rx.try_recv().is_err(), "do not submit again until ack");
    h.edit_ack("s1", 9, true);
    assert_eq!(h.app.cur.prompt.text, "pending edited", "ignore an ack for another turn");
    h.edit_ack("s1", 10, true);
    assert!(h.app.cur.prompt.text.is_empty());
    assert_eq!(h.app.cur.question.as_deref(), Some("ask1"));
    assert_eq!(h.app.cur.queue[0].body, "review this later");
    h.queue("s1", &[(9, "older"), (10, "pending edited")]);
    h.key(KeyCode::Up);
    assert_eq!(h.app.cur.prompt.text, "pending edited");
}

#[test]
fn escape_discards_a_queued_edit_without_sending_it() {
    let mut h = Harness::new();
    h.queue("s1", &[(30, "original")]);
    h.key(KeyCode::Up);
    h.keys(" changed");
    h.key(KeyCode::Esc);
    assert_eq!(h.app.mode, Mode::Normal);
    assert!(h.app.cur.prompt.text.is_empty());
    assert!(h.rx.try_recv().is_err());
    h.keys("i");
    h.key(KeyCode::Up);
    assert_eq!(h.app.cur.prompt.text, "original");
}

#[test]
fn queued_edit_ack_success_keeps_new_typing_and_failure_keeps_an_unsent_draft() {
    let mut h = Harness::new();
    h.queue("s1", &[(12, "original")]);
    h.key(KeyCode::Up);
    h.keys(" changed");
    h.key(KeyCode::Enter);
    assert!(matches!(h.request(), Request::EditQueued { turn_id: 12, .. }));
    h.key(KeyCode::Esc);
    assert_eq!(h.app.cur.prompt.text, "original changed", "Esc cannot retract a pending edit");
    h.keys("i again");
    h.edit_ack("s1", 12, true);
    assert_eq!(h.app.cur.prompt.text, "original changed again");
    h.queue("s1", &[]);
    h.key(KeyCode::Enter);
    assert_eq!(h.sent(), "original changed again");

    h.queue("s1", &[(13, "next")]);
    h.key(KeyCode::Up);
    h.keys(" changed");
    // A queue update can arrive before Enter. It must not turn the edit into a Send.
    h.queue("s1", &[]);
    h.key(KeyCode::Enter);
    assert_eq!(h.request(), Request::EditQueued { session_id: "s1".into(), turn_id: 13, text: "next changed".into() });
    h.edit_ack("s1", 13, false);
    assert_eq!(h.app.cur.prompt.text, "next changed");
    assert!(h.screen().contains("already started — edit kept as an unsent draft"));
    assert!(h.rx.try_recv().is_err(), "do not resend automatically");
    h.key(KeyCode::Enter);
    assert_eq!(h.sent(), "next changed");
}

#[test]
fn ctrl_enter_saves_a_queued_edit_before_interrupting_the_running_turn() {
    let mut h = Harness::new();
    h.agent(AgentEvent::TurnStarted { text: "running".into() });
    h.queue("s1", &[(4, "pending")]);
    h.key(KeyCode::Up);
    h.keys(" edited");
    h.app.on_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL)));
    assert_eq!(h.request(), Request::EditQueued { session_id: "s1".into(), turn_id: 4, text: "pending edited".into() });
    assert!(h.rx.try_recv().is_err(), "wait for the edit to land before interrupting");
    h.edit_ack("s1", 4, true);
    assert_eq!(h.request(), Request::RunQueuedNow { session_id: "s1".into() });
    assert!(h.app.cur.prompt.text.is_empty());

    h.queue("s1", &[(4, "pending edited"), (5, "following")]);
    h.key(KeyCode::Up);
    h.keys(" edited");
    h.app.on_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL)));
    assert!(matches!(h.request(), Request::EditQueued { turn_id: 5, .. }));
    h.agent(AgentEvent::TurnEnd { cancelled: true, error: None });
    h.queue("s1", &[(5, "following")]);
    h.agent(AgentEvent::TurnStarted { text: "pending edited".into() });
    h.edit_ack("s1", 5, true);
    assert!(h.rx.try_recv().is_err(), "do not interrupt a different running turn after the ack");
}

#[test]
fn search_and_message_jumps() {
    let mut h = Harness::new();
    h.keys("start");
    h.key(KeyCode::Enter);
    h.sent();
    h.complete_turn();
    h.key(KeyCode::Esc);
    h.keys("gg/second");
    h.key(KeyCode::Enter);
    let row = h.app.cur.chat_cursor.pos.row;
    assert_eq!(h.app.cur.transcript.row(row).text, "Second line here.");
    h.keys("{");
    assert_eq!(h.app.cur.transcript.entry_at(h.app.cur.chat_cursor.pos.row), 1);
    h.keys("G");
    assert!(h.app.cur.follow);
}

#[test]
fn shift_enter_adds_a_newline_in_comments() {
    let mut h = Harness::new();
    h.keys("start");
    h.key(KeyCode::Enter);
    h.sent();
    h.complete_turn();
    h.key(KeyCode::Esc);
    h.keys("gg}c");
    h.keys("first");
    h.app.on_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)));
    h.draw();
    assert_eq!(h.app.mode, Mode::Comment);
    assert!(h.screen().contains("Shift-Enter newline"));
    h.keys("second");
    h.key(KeyCode::Enter);
    assert_eq!(h.app.cur.queue[0].body, "first\nsecond");
}

#[test]
fn ctrl_enter_sends_from_any_mode() {
    let ctrl_enter = || Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL));
    let mut h = Harness::new();
    h.keys("start");
    h.key(KeyCode::Enter);
    h.sent();
    h.complete_turn();
    h.key(KeyCode::Esc);
    // Nothing queued or typed: nothing is sent.
    h.app.on_event(ctrl_enter());
    assert!(h.rx.try_recv().is_err());
    // Normal mode: queue a comment, then Ctrl-Enter sends it without entering insert mode.
    h.keys("gg}c");
    h.keys("one");
    h.key(KeyCode::Enter);
    h.app.on_event(ctrl_enter());
    let sent = h.sent();
    assert!(sent.contains("Comment: one") && sent.ends_with("</review_comments>"), "{sent}");
    assert_eq!(h.app.mode, Mode::Normal);
    h.complete_turn();
    // Inside the comment editor: Ctrl-Enter saves the comment and sends.
    h.keys("gg}c");
    h.keys("two");
    h.app.on_event(ctrl_enter());
    assert!(h.sent().contains("Comment: two"));
    assert!(h.app.cur.queue.is_empty());
}

#[test]
fn shift_tab_cycles_thinking_and_colors_the_model_name() {
    use super::theme::{GRAY, THINK_HIGH, THINK_LOW, THINK_MAX};
    let mut h = Harness::new();
    let model_fg = |h: &Harness| {
        let buf = h.term.backend().buffer();
        let y = buf.area.height - 1;
        let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
        let x = row.find("test-model").map(|b| row[..b].chars().count()).expect("model in the status line");
        buf[(x as u16, y)].fg
    };
    assert_eq!((h.app.effort, model_fg(&h)), (Effort::High, THINK_HIGH), "thinking starts on");
    let mut seen = Vec::new();
    for _ in 0..4 {
        h.key(KeyCode::BackTab);
        seen.push((h.app.effort, model_fg(&h)));
    }
    assert_eq!(
        seen,
        [(Effort::Max, THINK_MAX), (Effort::Off, GRAY), (Effort::Low, THINK_LOW), (Effort::High, THINK_HIGH)]
    );
    // Works from normal mode too, and the turn carries the level picked.
    h.key(KeyCode::Esc);
    h.key(KeyCode::BackTab);
    h.keys("ihi");
    h.key(KeyCode::Enter);
    let effort = loop {
        match h.rx.try_recv() {
            Ok(Request::Send { effort, .. }) => break effort,
            Ok(_) => continue,
            Err(e) => panic!("no turn sent: {e}"),
        }
    };
    assert_eq!(effort, Effort::Max);
}

#[test]
fn model_command_lists_models_and_the_pick_goes_with_the_next_turn() {
    let mut h = Harness::new();
    let next_model = |h: &mut Harness| loop {
        match h.rx.try_recv() {
            Ok(Request::Send { model, .. }) => break model,
            Ok(_) => continue,
            Err(e) => panic!("no turn sent: {e}"),
        }
    };
    h.key(KeyCode::Esc);
    h.keys(":model");
    h.key(KeyCode::Enter);
    assert!(matches!(h.app.view, View::Models { sel: 0 }));
    let screen = h.screen();
    assert!(screen.contains("deepseek-flash") && screen.contains("deepseek-v4-pro"), "{screen}");
    h.keys("j");
    h.key(KeyCode::Enter);
    assert!(matches!(h.app.view, View::Chat));
    assert!(h.screen().contains("deepseek-v4-pro · ↑"), "the status line names the pick");
    h.keys("ihi");
    h.key(KeyCode::Enter);
    assert_eq!(next_model(&mut h).as_deref(), Some("deepseek-v4-pro"));
    // The list opens on the current model; any id can be typed after :model.
    h.key(KeyCode::Esc);
    h.keys(":model");
    h.key(KeyCode::Enter);
    assert!(matches!(h.app.view, View::Models { sel: 1 }));
    h.keys("q:model my-endpoint-model");
    h.key(KeyCode::Enter);
    h.keys("iagain");
    h.key(KeyCode::Enter);
    assert_eq!(next_model(&mut h).as_deref(), Some("my-endpoint-model"));
}

#[test]
fn user_messages_have_a_full_width_background() {
    use super::theme::USER_BG;
    let mut h = Harness::new();
    h.keys("hello there");
    h.key(KeyCode::Enter);
    h.sent();
    h.complete_turn();
    let screen = h.screen();
    let y = screen.lines().position(|l| l.starts_with("› hello there")).unwrap() as u16;
    let buf = h.term.backend().buffer();
    for x in [0, 5, 79] {
        assert_eq!(buf[(x, y)].bg, USER_BG, "cell {x},{y}");
    }
    // Assistant rows keep the terminal background.
    let a = screen.lines().position(|l| l.starts_with("● I changed")).unwrap() as u16;
    assert_eq!(buf[(5, a)].bg, super::theme::BG, "everything else gets the theme background");
}

#[test]
fn assistant_markdown_renders_and_quotes_keep_the_source() {
    use ratatui::style::Modifier;
    let mut h = Harness::new();
    h.keys("explain");
    h.key(KeyCode::Enter);
    h.sent();
    let md = "**What it does:** loads `src/lib.rs:2` config\n- `Config` struct, *strict*";
    h.agent(AgentEvent::TextDelta(md.into()));
    h.agent(AgentEvent::Assistant { message: Message::assistant(md, vec![]), usage: None });
    h.agent(AgentEvent::TurnEnd { cancelled: false, error: None });
    let screen = h.screen();
    assert!(screen.contains("● What it does: loads src/lib.rs:2 config"), "{screen}");
    assert!(screen.contains("  • Config struct, strict"), "{screen}");
    assert!(!screen.contains("**") && !screen.contains('`'), "{screen}");
    let y = screen.lines().position(|l| l.starts_with("● What")).unwrap() as u16;
    let buf = h.term.backend().buffer();
    assert!(buf[(2, y)].modifier.contains(Modifier::BOLD), "bold run is drawn bold");
    // The path inside the code span is still a link, and the code keeps its color under it.
    let x = screen.lines().nth(y as usize).unwrap().find("src/lib.rs").unwrap() as u16;
    assert!(buf[(x, y)].modifier.contains(Modifier::UNDERLINED));

    // Select the whole first line linewise and comment: the quote is the source markdown.
    h.key(KeyCode::Esc);
    h.keys("gg}V");
    h.key(KeyCode::Char('c'));
    h.keys("ok");
    h.key(KeyCode::Enter);
    match &h.app.cur.queue[0].anchor {
        crate::comments::Anchor::Chat { quote, .. } => {
            assert_eq!(quote, "**What it does:** loads `src/lib.rs:2` config")
        }
        a => panic!("{a:?}"),
    }
    // Charwise selection of just the path inside the code span quotes it with its backticks.
    h.keys("0/src");
    h.key(KeyCode::Enter);
    h.keys("v");
    for _ in 0.."src/lib.rs:2".len() - 1 {
        h.key(KeyCode::Char('l'));
    }
    h.key(KeyCode::Char('c'));
    h.keys("path?");
    h.key(KeyCode::Enter);
    match &h.app.cur.queue[1].anchor {
        crate::comments::Anchor::Chat { quote, .. } => assert_eq!(quote, "`src/lib.rs:2`"),
        a => panic!("{a:?}"),
    }
}

#[test]
fn user_messages_are_not_rendered_as_markdown() {
    let mut h = Harness::new();
    h.keys("keep **this** and `that`");
    h.key(KeyCode::Enter);
    h.sent();
    assert!(h.screen().contains("› keep **this** and `that`"));
}

#[test]
fn assistant_tables_render_aligned_and_quote_rows() {
    use ratatui::style::Modifier;
    let mut h = Harness::new();
    h.keys("table");
    h.key(KeyCode::Enter);
    h.sent();
    let md = "| name | value |\n|:-----|------:|\n| **a** | 1 |\n| bb | 22 |";
    h.agent(AgentEvent::TextDelta(md.into()));
    h.agent(AgentEvent::Assistant { message: Message::assistant(md, vec![]), usage: None });
    h.agent(AgentEvent::TurnEnd { cancelled: false, error: None });
    let screen = h.screen();
    assert!(screen.contains("● name │ value"), "{screen}");
    assert!(screen.contains("─────┼──────"), "{screen}");
    assert!(screen.contains("a    │     1"), "{screen}");
    assert!(screen.contains("bb   │    22"), "{screen}");
    assert!(!screen.contains("| name |"), "{screen}");
    // The header row is bold.
    let y = screen.lines().position(|l| l.starts_with("● name")).unwrap() as u16;
    assert!(h.term.backend().buffer()[(2, y)].modifier.contains(Modifier::BOLD));
    // Selecting the header linewise and commenting quotes the exact source markdown.
    h.key(KeyCode::Esc);
    h.keys("gg}V");
    h.key(KeyCode::Char('c'));
    h.keys("ok");
    h.key(KeyCode::Enter);
    match &h.app.cur.queue[0].anchor {
        crate::comments::Anchor::Chat { quote, .. } => assert_eq!(quote, "| name | value |"),
        a => panic!("{a:?}"),
    }
}

#[test]
fn sidebar_lists_sessions_with_subagents_nested() {
    let mut h = Harness::new();
    let dir = h._dir.path().join("sessions");
    std::fs::create_dir_all(&dir).unwrap();
    let mut parent = crate::trace::Trace::create(&dir, "20260101-000000-aaaa", "m", h.app.cur.transcript.cwd(), None)
        .unwrap();
    parent.record(crate::trace::TraceEvent::Message { message: Message::user("refactor the parser") }).unwrap();
    crate::trace::Trace::create(&dir, "20260101-000000-aaaa-sub1", "m", h.app.cur.transcript.cwd(), Some("20260101-000000-aaaa"))
        .unwrap()
        .record(crate::trace::TraceEvent::Message { message: Message::user("check the lexer") })
        .unwrap();
    h.app.sidebar_open = true;
    h.sessions(&dir, &[]);
    h.draw();

    let screen = h.screen();
    let root = screen.lines().position(|l| l.contains("refactor the parser")).unwrap_or_else(|| panic!("root row in:\n{screen}"));
    let sub = screen.lines().position(|l| l.contains("check the lexer")).unwrap_or_else(|| panic!("subagent row in:\n{screen}"));
    assert!(root < sub, "subagent is nested beneath its parent");
    // The rail takes the left column and the prompt keeps the full width.
    let buf = h.term.backend().buffer();
    assert_eq!(buf[(0, root as u16)].bg, super::theme::PANEL);
    assert_eq!(buf[(super::render::SIDEBAR_WIDTH, root as u16)].bg, super::theme::BG);
}

#[test]
fn sidebar_toggles_with_s_and_hides_on_narrow_terminals() {
    let mut h = Harness::new();
    h.key(KeyCode::Esc);
    assert!(!h.app.sidebar_open);
    h.key(KeyCode::Char('S'));
    assert!(h.app.sidebar_open);
    assert!(h.app.sidebar_area.is_some());
    h.key(KeyCode::Char('S'));
    assert!(!h.app.sidebar_open);
    assert!(h.app.sidebar_area.is_none());

    let mut narrow = Terminal::new(TestBackend::new(60, 24)).unwrap();
    h.app.sidebar_open = true;
    let app = &mut h.app;
    narrow.draw(|f| app.draw(f)).unwrap();
    assert!(h.app.sidebar_area.is_none(), "rail yields to the chat below the width floor");
}

/// A harness whose session `s1` is recorded on disk, so the rail lists it.
fn harness_with_session() -> Harness {
    let mut h = Harness::new();
    let dir = h._dir.path().join("sessions");
    let cwd = h.app.cur.transcript.cwd().to_path_buf();
    crate::trace::Trace::create(&dir, "s0", "m", &cwd, None)
        .unwrap()
        .record(crate::trace::TraceEvent::Message { message: Message::user("older work") })
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    crate::trace::Trace::create(&dir, "s1", "m", &cwd, None)
        .unwrap()
        .record(crate::trace::TraceEvent::Message { message: Message::user("current work") })
        .unwrap();
    h.app.sidebar_open = true;
    h.sessions(&dir, &[]);
    h.draw();
    h
}

fn task_call(id: &str) -> ToolCall {
    ToolCall { id: id.into(), name: "task".into(), arguments: r#"{"description":"d","prompt":"p"}"#.into() }
}

#[test]
fn running_subagents_appear_immediately_and_settle() {
    let mut h = harness_with_session();
    h.keys("go");
    h.key(KeyCode::Enter);
    h.sent();
    h.agent(AgentEvent::ToolStart { call: task_call("t1") });
    h.agent(AgentEvent::SubagentStarted { call_id: "t1".into(), session_id: "s1-sub1".into(), title: "scan lexer".into() });
    let screen = h.screen();
    let row = screen.lines().find(|l| l.contains("scan lexer")).unwrap_or_else(|| panic!("{screen}"));
    assert!(super::render::SPINNER.iter().any(|s| row.contains(s)), "running row shows the spinner: {row:?}");
    let cur = screen.lines().position(|l| l.contains("current work")).unwrap();
    let sub = screen.lines().position(|l| l.contains("scan lexer")).unwrap();
    assert_eq!(sub, cur + 1, "nested under the current session");

    h.agent(AgentEvent::Subagent { call_id: "t1".into(), event: Box::new(AgentEvent::TurnEnd { cancelled: false, error: None }) });
    h.agent(AgentEvent::ToolEnd { call_id: "t1".into(), output: ToolOutput::ok("x"), duration_ms: 5 });
    assert!(h.screen().lines().any(|l| l.contains("● scan lexer")));
    h.agent(AgentEvent::TurnEnd { cancelled: false, error: None });

    // A later turn: the reported id is used as is, and a failure shows ✗.
    h.keys("again");
    h.key(KeyCode::Enter);
    h.sent();
    h.agent(AgentEvent::ToolStart { call: task_call("t2") });
    h.agent(AgentEvent::SubagentStarted { call_id: "t2".into(), session_id: "s1-sub2".into(), title: "check docs".into() });
    h.agent(AgentEvent::ToolEnd { call_id: "t2".into(), output: ToolOutput::err("boom"), duration_ms: 5 });
    let screen = h.screen();
    assert!(screen.lines().any(|l| l.contains("✗ check docs")), "{screen}");
    assert!(screen.lines().any(|l| l.contains("● scan lexer")), "{screen}");
}

impl Harness {
    /// Next request other than `List`, without acting on it.
    fn request(&mut self) -> Request {
        loop {
            match self.rx.try_recv() {
                Ok(Request::List) => continue,
                Ok(r) => return r,
                Err(e) => panic!("no request: {e}"),
            }
        }
    }

    fn ctrl(&mut self, c: char) {
        self.app.on_event(Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)));
        self.draw();
    }

    fn rail_id(&self) -> &str {
        &self.app.sidebar.rows()[self.app.sidebar_sel].id
    }
}

#[test]
fn rail_focus_selection_and_mouse() {
    let mut h = harness_with_session();
    h.key(KeyCode::Esc);
    h.ctrl('w');
    h.key(KeyCode::Char('h'));
    assert!(h.app.sidebar_focus);
    assert!(h.screen().contains(" sessions "), "status shows the focused view");
    // The selection starts on the current session; j moves to the older one.
    assert_eq!(h.rail_id(), "s1");
    h.key(KeyCode::Char('j'));
    assert_eq!(h.rail_id(), "s0");
    h.key(KeyCode::Char('y'));
    assert!(h.app.osc_out.as_deref().is_some_and(|s| s.starts_with("\x1b]52;c;")));
    // Navigation keys no longer reach the chat while the rail has focus.
    h.key(KeyCode::Char('k'));
    assert_eq!(h.rail_id(), "s1");
    h.key(KeyCode::Esc);
    assert!(!h.app.sidebar_focus);
    // Clicking a row focuses the rail and selects it; clicking the chat returns focus.
    let screen = h.screen();
    let y = screen.lines().position(|l| l.contains("older work")).unwrap() as u16;
    let click = |row: u16, col: u16| {
        Event::Mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: col, row, modifiers: KeyModifiers::NONE })
    };
    h.app.on_event(click(y, 3));
    assert!(h.app.sidebar_focus);
    assert_eq!(h.rail_id(), "s0");
    h.app.on_event(click(1, 50));
    assert!(!h.app.sidebar_focus);
}

#[test]
fn enter_on_a_session_switches_and_others_keep_updating() {
    let mut h = harness_with_session();
    h.keys("draft for s1");
    h.key(KeyCode::Esc);
    h.ctrl('w');
    h.key(KeyCode::Char('h'));
    h.key(KeyCode::Char('j'));
    assert_eq!(h.request(), Request::ReadTrace { session_id: "s0".into() }, "selecting it previews it");
    h.key(KeyCode::Enter);
    // Not open in this TUI yet: ask the hub to open (resume) it.
    assert_eq!(h.request(), Request::Open { session_id: "s0".into() });
    assert!(h.screen().contains("opening s0"));
    let mut s0 = snapshot("s0");
    s0.history = vec![Message::user("older work"), Message::assistant("old answer", vec![])];
    h.notice(Notice::Opened { snapshot: s0 });
    assert_eq!(h.app.cur.session_id, "s0");
    assert!(h.screen().contains("old answer"));
    assert!(h.screen().contains("▸ older work"), "rail marks the session on screen");

    // s1 runs in the background while s0 is on screen.
    let ev = |e| Notice::Event { session_id: "s1".into(), event: e };
    h.notice(ev(AgentEvent::TurnStarted { text: "background job".into() }));
    h.notice(ev(AgentEvent::TextDelta("finished in the background".into())));
    assert!(!h.screen().contains("finished in the background"));
    h.notice(Notice::Sessions { recorded: crate::trace::list_sessions(&h._dir.path().join("sessions")), open: vec!["s0".into(), "s1".into()], running: vec!["s1".into()] });
    // :q refuses while another session is running.
    h.keys(":q");
    h.key(KeyCode::Enter);
    assert!(!h.app.quit);
    assert!(h.screen().contains("still running"));

    // Back to s1: already open here, so no request; its draft and background output are there.
    h.ctrl('w');
    h.key(KeyCode::Char('h'));
    h.key(KeyCode::Char('g'));
    assert_eq!(h.rail_id(), "s1");
    h.key(KeyCode::Enter);
    assert!(h.rx.try_recv().is_err());
    assert_eq!(h.app.cur.session_id, "s1");
    assert_eq!(h.app.cur.prompt.text, "draft for s1");
    assert!(h.screen().contains("› background job"));
    assert!(h.screen().contains("finished in the background"));
    assert!(h.app.cur.running);
}

#[test]
fn subagent_rows_open_read_only_traces_and_new_starts_a_session() {
    let mut h = harness_with_session();
    h.notice(Notice::Event {
        session_id: "s1".into(),
        event: AgentEvent::SubagentStarted { call_id: "t1".into(), session_id: "s1-sub1".into(), title: "scan lexer".into() },
    });
    h.key(KeyCode::Esc);
    h.ctrl('w');
    h.key(KeyCode::Char('h'));
    h.key(KeyCode::Char('j'));
    assert_eq!(h.rail_id(), "s1-sub1");
    h.key(KeyCode::Enter);
    assert_eq!(h.request(), Request::ReadTrace { session_id: "s1-sub1".into() });
    h.notice(Notice::Trace {
        session_id: "s1-sub1".into(),
        cwd: String::new(),
        history: vec![Message::user("scan the lexer"), Message::assistant("lexer is fine", vec![])],
    });
    assert_eq!(h.app.cur.session_id, "s1-sub1");
    assert!(h.app.cur.read_only);
    assert!(h.screen().contains("lexer is fine"));
    h.keys("ihello");
    h.key(KeyCode::Enter);
    assert!(h.screen().contains("read-only"), "{}", h.screen());
    assert!(h.rx.try_recv().is_err(), "nothing is sent from a read-only view");

    h.key(KeyCode::Esc);
    h.keys(":new");
    h.key(KeyCode::Enter);
    assert!(matches!(h.request(), Request::New { cwd: Some(_) }), "new sessions start in this directory");
    // s1 was used (the rail lists it), so it stays resumable; the fresh s2 has no input yet.
    if let Some(s1) = h.app.others.iter_mut().find(|v| v.session_id == "s1") {
        s1.has_input = true;
    }
    h.notice(Notice::Opened { snapshot: snapshot("s2") });
    assert_eq!(h.app.cur.session_id, "s2");
    let open: Vec<String> = h.app.open_sessions().into_iter().map(|(_, id)| id).collect();
    assert_eq!(open, ["s1"], "read-only traces and empty sessions are not listed as resumable");
}

#[test]
fn focusing_the_rail_starts_on_the_session_on_screen() {
    let mut h = harness_with_session();
    h.key(KeyCode::Esc);
    h.ctrl('w');
    h.key(KeyCode::Char('h'));
    h.key(KeyCode::Char('j'));
    assert_eq!(h.rail_id(), "s0");
    h.key(KeyCode::Esc);
    // Leave the selection elsewhere, then come back: it starts on the current session again.
    h.ctrl('w');
    h.key(KeyCode::Char('h'));
    assert_eq!(h.rail_id(), "s1");
}

fn trace_of(id: &str, answer: &str) -> Notice {
    Notice::Trace { session_id: id.into(), cwd: String::new(), history: vec![Message::user("older work"), Message::assistant(answer, vec![])] }
}

#[test]
fn moving_through_the_rail_previews_sessions_without_switching() {
    let mut h = harness_with_session();
    h.key(KeyCode::Esc);
    h.ctrl('w');
    h.key(KeyCode::Char('h'));
    assert!(!h.screen().contains("PREVIEW"), "the current session needs no preview");
    h.key(KeyCode::Char('j'));
    assert_eq!(h.request(), Request::ReadTrace { session_id: "s0".into() });
    assert!(h.screen().contains("loading…"));
    h.notice(trace_of("s0", "the old answer"));
    let screen = h.screen();
    assert!(screen.contains("PREVIEW") && screen.contains("the old answer"), "{screen}");
    assert_eq!(h.app.cur.session_id, "s1", "previewing does not switch");
    assert!(!h.app.history_contains("older work"), "preview messages stay out of the prompt history");
    // Back and forth: the preview is cached, so no second request.
    h.key(KeyCode::Char('k'));
    assert!(!h.screen().contains("PREVIEW"));
    h.key(KeyCode::Char('j'));
    assert!(h.rx.try_recv().is_err());
    assert!(h.screen().contains("the old answer"));
    // Leaving the rail ends the preview.
    h.key(KeyCode::Esc);
    assert!(!h.screen().contains("PREVIEW"));
    // A changed trace invalidates the cached preview.
    let dir = h._dir.path().join("sessions");
    std::thread::sleep(std::time::Duration::from_millis(20));
    let mut t = crate::trace::Trace::open_append(&dir.join("s0.jsonl"), "s0").unwrap();
    t.record(crate::trace::TraceEvent::Message { message: Message::user("more") }).unwrap();
    h.sessions(&dir, &[]);
    h.ctrl('w');
    h.key(KeyCode::Char('h'));
    // s0 changed last, so it is listed first now.
    h.key(KeyCode::Char('k'));
    assert_eq!(h.rail_id(), "s0");
    assert_eq!(h.request(), Request::ReadTrace { session_id: "s0".into() });
}

#[test]
fn hovering_does_not_preview_but_clicking_and_keys_do() {
    let mut h = harness_with_session();
    let screen = h.screen();
    let y = screen.lines().position(|l| l.contains("older work")).unwrap() as u16;
    let mv = |row: u16, col: u16| Event::Mouse(MouseEvent { kind: MouseEventKind::Moved, column: col, row, modifiers: KeyModifiers::NONE });
    let click = |row: u16, col: u16| {
        Event::Mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: col, row, modifiers: KeyModifiers::NONE })
    };
    // Moving over a row changes nothing: no selection, no focus, no trace request.
    h.app.on_event(mv(y, 4));
    h.draw();
    assert!(!h.app.sidebar_focus, "hovering does not take focus");
    assert_eq!(h.rail_id(), "s1", "hovering does not move the selection");
    assert!(h.rx.try_recv().is_err(), "hovering does not preview");
    assert!(!h.screen().contains("PREVIEW"));

    // Clicking the row selects it and previews it.
    h.app.on_event(click(y, 4));
    h.draw();
    assert!(h.app.sidebar_focus);
    assert_eq!(h.rail_id(), "s0");
    assert_eq!(h.request(), Request::ReadTrace { session_id: "s0".into() });
    h.notice(trace_of("s0", "clicked answer"));
    assert!(h.screen().contains("clicked answer"));
    // Moving the mouse away leaves that preview up: only keys and clicks change it.
    h.app.on_event(mv(5, 60));
    h.draw();
    assert!(h.screen().contains("clicked answer"));

    // A session open in this TUI previews from its live view, including background output.
    h.app.on_event(mv(y, 4));
    h.draw();
    h.notice(Notice::Opened { snapshot: snapshot("s0") });
    h.notice(Notice::Event { session_id: "s0".into(), event: AgentEvent::TurnStarted { text: "live job".into() } });
    h.notice(Notice::Event { session_id: "s0".into(), event: AgentEvent::TextDelta("streaming now".into()) });
    h.app.on_event(click(y, 4));
    h.draw();
    assert!(h.rx.try_recv().is_err(), "no trace request for an open session");
    let screen = h.screen();
    assert!(screen.contains("streaming now"), "{screen}");
    assert_eq!(h.app.cur.session_id, "s1");
}

/// A local host plus a remote `box` whose sessions live in `/srv/app`.
struct TwoHosts {
    h: Harness,
    remote: mpsc::UnboundedReceiver<Request>,
}

fn two_hosts() -> TwoHosts {
    let mut h = harness_with_session();
    let (tx, remote) = mpsc::unbounded_channel();
    h.app.hosts.push(Host::new("box", false, Some(std::path::PathBuf::from("/srv/app")), tx));
    let info = |id: &str, title: &str| crate::trace::SessionInfo {
        id: id.into(),
        path: format!("/srv/sessions/{id}.jsonl").into(),
        modified: std::time::SystemTime::now(),
        parent: None,
        cwd: "/srv/app".into(),
        has_input: true,
        title: title.into(),
        slug: None,
    };
    h.app.on_notice(1, Notice::Sessions { recorded: vec![info("r1", "remote build fix")], open: vec![], running: vec![] });
    h.draw();
    TwoHosts { h, remote }
}

impl TwoHosts {
    fn remote_request(&mut self) -> Request {
        loop {
            match self.remote.try_recv() {
                Ok(Request::List) => continue,
                Ok(r) => return r,
                Err(e) => panic!("no remote request: {e}"),
            }
        }
    }
}

#[test]
fn several_hosts_share_the_rail_and_requests_go_to_the_sessions_host() {
    let mut t = two_hosts();
    let screen = t.h.screen();
    let local = screen.lines().position(|l| l.contains("◆ local")).unwrap_or_else(|| panic!("{screen}"));
    let remote = screen.lines().position(|l| l.contains("◆ box")).unwrap();
    let row = screen.lines().position(|l| l.contains("remote build fix")).unwrap();
    assert!(local < remote && remote < row, "{screen}");

    // Open the remote session from the rail: the request goes to box, not to the local hub.
    t.h.key(KeyCode::Esc);
    t.h.ctrl('w');
    t.h.key(KeyCode::Char('h'));
    t.h.key(KeyCode::Char('G'));
    assert_eq!(t.h.rail_id(), "r1");
    assert_eq!(t.remote_request(), Request::ReadTrace { session_id: "r1".into() }, "preview reads it on box");
    t.h.key(KeyCode::Enter);
    assert_eq!(t.remote_request(), Request::Open { session_id: "r1".into() });
    assert!(t.h.rx.try_recv().is_err(), "nothing went to the local hub");
    let mut snap = snapshot("r1");
    snap.cwd = "/srv/app".into();
    snap.history = vec![Message::user("fix the build"), Message::assistant("edited src/build.rs:7 for you", vec![])];
    t.h.app.on_notice(1, Notice::Opened { snapshot: snap });
    t.h.draw();
    assert_eq!((t.h.app.cur.host, t.h.app.cur.session_id.as_str()), (1, "r1"));
    assert!(t.h.screen().contains("box @ r1"), "status names the host");

    // Sending goes to box.
    t.h.keys("ithanks");
    t.h.key(KeyCode::Enter);
    assert_eq!(t.remote_request(), Request::Send { session_id: "r1".into(), text: "thanks".into(), effort: Effort::High, model: Some("test-model".into()), diagrams: false });

    // A file link in a remote session is read on box (no local file exists).
    t.h.key(KeyCode::Esc);
    t.h.keys("gg}");
    t.h.keys("w");
    t.h.key(KeyCode::Enter);
    assert_eq!(t.remote_request(), Request::ReadFile { cwd: "/srv/app".into(), path: "/srv/app/src/build.rs".into(), base: crate::diff::Base::Head });
    let file = crate::diff::FileDiff {
        path: "/srv/app/src/build.rs".into(),
        rel: "src/build.rs".into(),
        status: crate::diff::Status::Modified,
        diff: crate::diff::parse_unified("@@ -7,1 +7,1 @@\n-old()\n+new()\n"),
        full: vec![],
        added: 1,
        removed: 1,
        error: None,
        head_source: None,
        base: None,
    };
    t.h.app.on_notice(1, Notice::File { file: Box::new(file.clone()) });
    t.h.draw();
    let View::File(fv) = &t.h.app.view else { panic!("file view") };
    assert_eq!(fv.lines[fv.cursor.pos.row].text, "new()", "cursor on the linked line");
    assert!(t.h.screen().contains("src/build.rs"));
    // Reloading asks box again.
    t.h.key(KeyCode::Char('r'));
    assert_eq!(t.remote_request(), Request::ReadFile { cwd: "/srv/app".into(), path: "/srv/app/src/build.rs".into(), base: crate::diff::Base::Head });
    t.h.key(KeyCode::Char('q'));

    // Changed files come from box too.
    t.h.key(KeyCode::Char('F'));
    assert_eq!(t.remote_request(), Request::ChangedFiles { cwd: "/srv/app".into() });
    t.h.app.on_notice(1, Notice::ChangedFiles { cwd: "/srv/app".into(), files: vec![("M".into(), "src/build.rs".into())] });
    t.h.draw();
    assert!(t.h.screen().contains("Changed files"));
    t.h.key(KeyCode::Enter);
    assert_eq!(t.remote_request(), Request::ReadFile { cwd: "/srv/app".into(), path: "/srv/app/src/build.rs".into(), base: crate::diff::Base::Head });
}

#[test]
fn a_host_heading_starts_a_session_there_and_disconnects_show() {
    let mut t = two_hosts();
    t.h.key(KeyCode::Esc);
    t.h.ctrl('w');
    t.h.key(KeyCode::Char('h'));
    // Move up to the box heading (the row above its session).
    t.h.key(KeyCode::Char('G'));
    t.h.key(KeyCode::Char('k'));
    assert!(t.h.app.sidebar.rows()[t.h.app.sidebar_sel].header);
    let _ = t.remote.try_recv();
    t.h.key(KeyCode::Enter);
    assert_eq!(t.remote_request(), Request::New { cwd: Some("/srv/app".into()) });
    let mut snap = snapshot("r2");
    snap.cwd = "/srv/app".into();
    t.h.app.on_notice(1, Notice::Opened { snapshot: snap });
    t.h.draw();
    assert_eq!((t.h.app.cur.host, t.h.app.cur.session_id.as_str()), (1, "r2"));

    t.h.app.on_disconnect(1);
    t.h.draw();
    let screen = t.h.screen();
    assert!(screen.contains("✗ box (disconnected)") && screen.contains("lost the connection to box"), "{screen}");
    t.h.keys("ihello");
    t.h.key(KeyCode::Enter);
    assert!(t.h.screen().contains("not connected to box"));
    // The local host is unaffected.
    assert!(t.h.screen().contains("◆ local"));
}

#[test]
fn job_notes_show_as_status_lines_not_user_messages() {
    let mut h = Harness::new();
    h.keys("build");
    h.key(KeyCode::Enter);
    h.sent();
    h.complete_turn();
    h.agent(AgentEvent::TurnStarted {
        text: "<background_jobs>\njob 1 `cargo build` finished: exit 0 after 3m12s. No output.\n</background_jobs>".into(),
    });
    let screen = h.screen();
    assert!(screen.contains("job 1 `cargo build` finished: exit 0 after 3m12s"), "{screen}");
    assert!(!screen.contains("<background_jobs>") && !screen.contains("› job 1"), "{screen}");
}

#[test]
fn rail_takes_counts_and_zh_shows_old_subagents() {
    use crate::tui::sidebar::{Live, LiveSub, STALE_SUBAGENT};
    let mut h = harness_with_session();
    let old = std::time::SystemTime::now() - STALE_SUBAGENT * 2;
    for (id, finished) in [("s1-sub1", Some(old)), ("s1-sub2", None), ("s1-sub3", Some(old))] {
        let state = if finished.is_some() { Live::Done } else { Live::Running };
        h.app.hosts[0].live.insert(id.into(), LiveSub { parent: "s1".into(), title: id.into(), state, finished });
    }
    h.app.refresh_sidebar();
    h.key(KeyCode::Esc);
    h.ctrl('w');
    h.key(KeyCode::Char('h'));
    // The finished pair is hidden; the header says so.
    let ids = |h: &Harness| h.app.sidebar.rows().iter().map(|r| r.id.clone()).collect::<Vec<_>>();
    assert_eq!(ids(&h), ["s1", "s1-sub2", "s0"]);
    assert!(h.screen().contains("+2 old"));
    h.keys("2j");
    assert_eq!(h.rail_id(), "s0");
    h.keys("2k");
    assert_eq!(h.rail_id(), "s1");
    h.keys("zh");
    assert_eq!(ids(&h), ["s1", "s1-sub1", "s1-sub2", "s1-sub3", "s0"]);
    assert_eq!(h.rail_id(), "s1");
    h.keys("3j");
    assert_eq!(h.rail_id(), "s1-sub3");
    h.keys("1G");
    assert_eq!(h.rail_id(), "s1");
    h.keys("zh");
    assert_eq!(ids(&h), ["s1", "s1-sub2", "s0"]);
}

#[test]
fn shift_tab_in_a_file_diffs_against_the_default_branch() {
    let mut h = Harness::new();
    let root = h._dir.path().to_path_buf();
    let git = |args: &[&str]| {
        std::process::Command::new("git").arg("-C").arg(&root).args(args).output().unwrap();
    };
    let effort = h.app.effort;
    h.key(KeyCode::Esc);

    // No remote yet: Shift-Tab stays on HEAD and says why.
    h.app.open_file(root.join("src/lib.rs"), None);
    h.draw();
    h.key(KeyCode::BackTab);
    let View::File(fv) = &h.app.view else { panic!("file view") };
    assert!(fv.base.is_head());
    assert!(h.app.flash.as_deref().is_some_and(|f| f.contains("no default branch")), "{:?}", h.app.flash);
    h.key(KeyCode::Char('q'));

    // origin/main at the first commit; the branch commits the change, so HEAD is clean.
    git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    git(&["commit", "-qam", "change"]);
    h.app.open_file(root.join("src/lib.rs"), None);
    h.draw();
    assert!(h.screen().contains("src/lib.rs — no changes — file view"), "{}", h.screen());
    h.key(KeyCode::BackTab);
    let View::File(fv) = &h.app.view else { panic!("file view") };
    assert!(!fv.base.is_head());
    assert_eq!(fv.file.base.as_deref(), Some("origin/main"));
    assert!(h.screen().contains("src/lib.rs — +2 -1 vs origin/main"), "{}", h.screen());
    assert_eq!(h.app.flash.as_deref(), Some("diff against origin/main"));
    // Reloading keeps the base; Shift-Tab goes back to HEAD. Thinking effort never moved.
    h.key(KeyCode::Char('r'));
    assert!(h.screen().contains("vs origin/main"), "{}", h.screen());
    h.key(KeyCode::BackTab);
    assert!(h.screen().contains("src/lib.rs — no changes"), "{}", h.screen());
    assert_eq!(h.app.effort, effort);
}

#[test]
fn queue_edits_and_notices_follow_the_session_through_switches_and_refreshes() {
    let mut h = harness_with_session();
    h.queue("s1", &[(21, "s1 pending")]);
    h.key(KeyCode::Up);
    h.keys(" edited");
    h.key(KeyCode::Enter);
    assert_eq!(h.request(), Request::EditQueued { session_id: "s1".into(), turn_id: 21, text: "s1 pending edited".into() });
    h.key(KeyCode::Esc);
    h.app.expect_open(0);
    h.notice(Notice::Opened { snapshot: snapshot("s0") });
    h.queue("s0", &[(21, "s0 pending")]);
    h.keys("i");
    h.key(KeyCode::Up);
    assert_eq!(h.app.cur.prompt.text, "s0 pending");
    h.keys(" draft");
    let mut refreshed = snapshot("s1");
    refreshed.queued = queued(&[(21, "s1 pending")]);
    h.notice(Notice::Opened { snapshot: refreshed });
    assert_eq!(h.app.others.iter().find(|v| v.session_id == "s1").unwrap().prompt.text, "s1 pending edited");
    h.edit_ack("s1", 21, true);
    h.queue("s1", &[(21, "s1 pending edited"), (22, "s1 latest")]);
    assert_eq!(h.app.cur.prompt.text, "s0 pending draft", "background ack must not touch the current edit");
    assert!(h.screen().contains("1 queued"));
    h.key(KeyCode::Esc);
    h.ctrl('w');
    h.keys("hg");
    h.key(KeyCode::Enter);
    assert_eq!(h.app.cur.session_id, "s1");
    assert!(h.app.cur.prompt.text.is_empty(), "the ack cleared s1's draft off screen");
    assert!(h.screen().contains("2 queued"));
    h.keys("i");
    h.key(KeyCode::Up);
    assert_eq!(h.app.cur.prompt.text, "s1 latest");
}

#[test]
fn a_reconnected_hub_gets_queue_opt_in_before_open_and_keeps_drafts_not_edit_ids() {
    let mut h = Harness::new();
    let id = h.app.cur.session_id.clone();
    h.queue("s1", &[(1, "old")]);
    h.key(KeyCode::Up);
    h.keys(" draft");
    h.key(KeyCode::Enter);
    assert!(matches!(h.request(), Request::EditQueued { turn_id: 1, .. }));
    h.app.on_host_event(0, HostEvent::Closed);
    assert!(!h.app.hosts[0].connected);
    assert!(h.app.flash.as_deref().is_some_and(|f| f.contains("lost the connection")));
    assert!(h.app.cur.queued.is_empty());
    assert!(h.app.cur.queue_edit.is_none());
    h.key(KeyCode::Enter);
    assert_eq!(h.app.cur.prompt.text, "old draft", "submitting while disconnected preserves the draft");
    let (tx, mut rx) = mpsc::unbounded_channel();
    h.app.on_host_event(0, HostEvent::Linked(tx));
    assert!(h.app.hosts[0].connected);
    assert!(rx.try_recv().is_err());
    h.notice(hello());
    let sent: Vec<Request> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert_eq!(sent, vec![Request::WatchQueue, Request::List, Request::Open { session_id: id.clone() }]);
    assert_eq!(h.app.flash.as_deref(), Some("reconnected to local"));
    let mut fresh = snapshot(&id);
    fresh.queued = queued(&[(1, "new turn with a reused id")]);
    h.notice(Notice::Opened { snapshot: fresh });
    assert_eq!(h.app.cur.session_id, id);
    assert_eq!(h.app.cur.prompt.text, "old draft");
    h.key(KeyCode::Enter);
    assert!(matches!(rx.try_recv(), Ok(Request::Send { text, .. }) if text == "old draft"));
}

#[test]
fn waiting_messages_are_listed_above_the_prompt() {
    let mut h = Harness::new();
    h.keys("first");
    h.key(KeyCode::Enter);
    h.sent();
    h.queue("s1", &[(2, "second message"), (3, "third\nwith two lines")]);
    let screen = h.screen();
    let lines: Vec<&str> = screen.lines().collect();
    let prompt = lines.iter().rposition(|l| l.starts_with('›')).unwrap_or_else(|| panic!("{screen}"));
    assert!(lines[prompt - 3].contains("○ 1. second message"), "{screen}");
    assert!(lines[prompt - 2].contains("○ 2. third …"), "{screen}");
    assert!(lines[prompt - 1].contains("2 queued"), "the separator sits between list and prompt:\n{screen}");
    // Recalling one for editing marks it.
    h.key(KeyCode::Up);
    assert!(h.screen().contains("✎ 2. third"), "{}", h.screen());
    // Six waiting: four rows, the last counting the rest.
    h.queue("s1", &[(2, "a"), (3, "b"), (4, "c"), (5, "d"), (6, "e"), (7, "f")]);
    let screen = h.screen();
    assert!(screen.contains("○ 3. c") && !screen.contains("○ 4. d"), "{screen}");
    assert!(screen.contains("+3 more waiting"), "{screen}");
    // Once the queue drains, the list goes away.
    h.queue("s1", &[]);
    assert!(!h.screen().contains("○ 1."), "{}", h.screen());
}

#[test]
fn tab_completes_paths_on_the_sessions_host_and_cycles_matches() {
    let mut h = Harness::new();
    let completions = |h: &mut Harness| loop {
        match h.rx.try_recv() {
            Ok(Request::CompletePath { request_id, prefix, .. }) => break (request_id, prefix),
            Ok(_) => continue,
            Err(e) => panic!("no completion request: {e}"),
        }
    };
    h.keys("look at src/m");
    h.key(KeyCode::Tab);
    let (id, prefix) = completions(&mut h);
    assert_eq!(prefix, "src/m");
    h.notice(Notice::PathCompletions { request_id: id, paths: vec!["src/main.rs".into()] });
    assert_eq!(h.app.cur.prompt.text, "look at src/main.rs");

    // Several matches: the shared part goes in, then Tab cycles through them.
    h.keys(" and (sr");
    h.key(KeyCode::Tab);
    let (id, prefix) = completions(&mut h);
    assert_eq!(prefix, "sr", "the path starts after the parenthesis");
    h.notice(Notice::PathCompletions { request_id: id, paths: vec!["src/tui/app.rs".into(), "src/tui/mod.rs".into()] });
    assert_eq!(h.app.cur.prompt.text, "look at src/main.rs and (src/tui/");
    assert!(h.app.flash.as_deref().is_some_and(|f| f.starts_with("2 matches")), "{:?}", h.app.flash);
    h.key(KeyCode::Tab);
    assert_eq!(h.app.cur.prompt.text, "look at src/main.rs and (src/tui/app.rs");
    h.key(KeyCode::Tab);
    assert_eq!(h.app.cur.prompt.text, "look at src/main.rs and (src/tui/mod.rs");
    assert!(h.rx.try_recv().is_err(), "cycling asks the hub nothing");

    // An answer that arrives after more typing changes nothing.
    h.keys(" x/");
    h.key(KeyCode::Tab);
    let (id, _) = completions(&mut h);
    h.keys("y");
    h.notice(Notice::PathCompletions { request_id: id, paths: vec!["x/zzz".into()] });
    assert!(h.app.cur.prompt.text.ends_with(" x/y"), "{}", h.app.cur.prompt.text);
}

#[test]
fn gx_opens_the_url_under_the_cursor_even_when_it_wraps() {
    let mut h = Harness::new();
    let opened = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
    let sink = opened.clone();
    h.app.open_url = Box::new(move |u| sink.borrow_mut().push(u.to_owned()));
    let long = format!("https://example.com/{}", "segment/".repeat(12));
    h.keys("start");
    h.key(KeyCode::Enter);
    h.sent();
    h.agent(AgentEvent::TextDelta(format!("Docs: {long} and src/lib.rs")));
    h.agent(AgentEvent::TurnEnd { cancelled: false, error: None });
    h.key(KeyCode::Esc);
    // On the answer's second row (inside the wrapped URL).
    h.keys("gg}j");
    h.keys("gx");
    assert_eq!(opened.borrow().as_slice(), std::slice::from_ref(&long), "{:?}", h.app.flash);
    // At the line start, off any link, gx falls back to the line's first URL.
    h.keys("k0gx");
    assert_eq!(opened.borrow().len(), 2);
    // On the file link, it opens the file itself.
    h.keys("/lib.rs");
    h.key(KeyCode::Enter);
    h.keys("gx");
    assert!(opened.borrow()[2].ends_with("src/lib.rs"), "{:?}", opened.borrow());
}
