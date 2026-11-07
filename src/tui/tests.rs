//! Behavior tests driving the App with real key events and rendering to a test backend.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{Terminal, backend::TestBackend};
use tokio::sync::mpsc;

use super::app::{AgentCmd, App, Mode, View};
use crate::{
    agent::AgentEvent,
    llm::{Message, ToolCall},
    tools::ToolOutput,
};

struct Harness {
    app: App,
    term: Terminal<TestBackend>,
    rx: mpsc::UnboundedReceiver<AgentCmd>,
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
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        std::fs::write(root.join("src/lib.rs"), "fn a() {}\nfn b2() {}\nfn c() {}\nfn d() {}\n").unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let app = App::new(root.to_path_buf(), "test-model".into(), "s1".into(), tx);
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

    fn sent(&mut self) -> String {
        match self.rx.try_recv() {
            Ok(AgentCmd::Turn { text, .. }) => text,
            Err(e) => panic!("no turn sent: {e}"),
        }
    }

    /// Simulates a finished turn whose answer mentions a file and ran one tool.
    fn complete_turn(&mut self) {
        self.agent(AgentEvent::TextDelta("I changed src/lib.rs:2 and ".into()));
        self.agent(AgentEvent::TextDelta("renamed b to b2.\nSecond line here.".into()));
        let msg = Message::Assistant {
            content: "I changed src/lib.rs:2 and renamed b to b2.\nSecond line here.".into(),
            reasoning: None,
            tool_calls: vec![ToolCall { id: "c1".into(), name: "bash".into(), arguments: r#"{"command":"cargo test"}"#.into() }],
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
    assert!(h.app.running);
    h.complete_turn();
    assert!(!h.app.running);
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
    assert_eq!(h.app.queue.len(), 1);
    assert!(h.screen().contains("1 comment queued"));
    // Linewise comment on the second line.
    h.keys("jV");
    h.key(KeyCode::Char('c'));
    h.keys("drop this");
    h.key(KeyCode::Enter);
    assert_eq!(h.app.queue.len(), 2);
    // Send: i, text, Enter.
    h.keys("i");
    h.keys("and run tests");
    h.key(KeyCode::Enter);
    let sent = h.sent();
    assert!(sent.contains("[1] On your response:\n> renamed b to b2\nComment: why rename?"), "{sent}");
    assert!(sent.contains("[2] On your response:\n> Second line here.\nComment: drop this"), "{sent}");
    assert!(sent.ends_with("</review_comments>\n\nand run tests"));
    assert!(h.app.queue.is_empty());
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
    assert_eq!(h.app.queue[1].body, "TWO");
    h.keys("ggdd");
    assert_eq!(h.app.queue.len(), 1);
    assert_eq!(h.app.queue[0].body, "TWO");
}

#[test]
fn messages_typed_during_a_turn_wait_for_it() {
    let mut h = Harness::new();
    h.keys("first");
    h.key(KeyCode::Enter);
    h.sent();
    h.keys("second");
    h.key(KeyCode::Enter);
    assert!(h.rx.try_recv().is_err());
    assert!(h.screen().contains("message waiting"));
    h.agent(AgentEvent::TurnEnd { cancelled: false, error: None });
    assert_eq!(h.sent(), "second");
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
    let row = h.app.chat_cursor.pos.row;
    assert_eq!(h.app.transcript.row(row).text, "Second line here.");
    h.keys("{");
    assert_eq!(h.app.transcript.entry_at(h.app.chat_cursor.pos.row), 1);
    h.keys("G");
    assert!(h.app.follow);
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
    assert_eq!(h.app.queue[0].body, "first\nsecond");
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
    assert!(h.app.queue.is_empty());
}
