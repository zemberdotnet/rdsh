//! Behavior tests driving the App with real key events and rendering to a test backend.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{Terminal, backend::TestBackend};
use tokio::sync::mpsc;

use super::app::{App, Mode, View};
use crate::{
    agent::AgentEvent,
    hub::{Notice, Request, Snapshot},
    llm::Usage,
    llm::{Message, ToolCall},
    tools::ToolOutput,
};

fn snapshot(id: &str) -> Snapshot {
    Snapshot { session_id: id.into(), model: "test-model".into(), history: Vec::new(), turn: Vec::new(), running: false, usage: Usage::default() }
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
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        std::fs::write(root.join("src/lib.rs"), "fn a() {}\nfn b2() {}\nfn c() {}\nfn d() {}\n").unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let mut app = App::new(root.to_path_buf(), tx);
        app.expect_open();
        app.on_notice(Notice::Opened { snapshot: snapshot("s1") });
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
                Ok(Request::Send { session_id, text }) => {
                    self.notice(Notice::Event { session_id, event: AgentEvent::TurnStarted { text: text.clone() } });
                    return text;
                }
                Ok(_) => continue,
                Err(e) => panic!("no turn sent: {e}"),
            }
        }
    }

    fn notice(&mut self, n: Notice) {
        self.app.on_notice(n);
        self.draw();
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
    // The hub queues turns, so the second message goes out right away and waits there.
    h.keys("second");
    h.key(KeyCode::Enter);
    assert!(h.screen().contains("message waiting"));
    h.agent(AgentEvent::TurnEnd { cancelled: false, error: None });
    assert!(h.screen().contains("message waiting"), "still waiting until the hub starts it");
    assert_eq!(h.sent(), "second");
    assert!(!h.screen().contains("message waiting"));
    assert!(h.screen().contains("› second"));
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
fn file_view_highlights_added_and_deleted_lines() {
    let mut h = Harness::new();
    let path = h.app.cur.transcript.cwd().join("src/lib.rs");
    h.app.open_file(path, None);
    h.draw();
    let View::File(fv) = &h.app.view else { panic!() };
    let row_of = |text: &str| fv.lines.iter().position(|l| l.text == text).unwrap();
    let (added, deleted) = (row_of("fn b2() {}"), row_of("fn b() {}"));
    // `fn` is a keyword on both sides: the added line uses the current file, the deleted line HEAD.
    for row in [added, deleted] {
        let first = fv.syntax(row).first().copied().expect("highlighted");
        assert_eq!((first.start, first.end, first.style.fg), (0, 2, Some(super::theme::RED)));
    }
    // Rendered: the keyword keeps its syntax color on the tinted background.
    let screen = h.screen();
    let y = screen.lines().position(|l| l.contains("+ fn b2() {}")).unwrap() as u16;
    let x = screen.lines().nth(y as usize).unwrap().chars().position(|c| c == 'f').unwrap() as u16;
    let cell = &h.term.backend().buffer()[(x, y)];
    assert_eq!(cell.fg, super::theme::RED);
    assert_eq!(cell.bg, super::theme::ADD_BG);
    assert_eq!(h.term.backend().buffer()[(79, y)].bg, super::theme::ADD_BG, "tint fills the row");
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
    assert_eq!(h.request(), Request::New);
    h.notice(Notice::Opened { snapshot: snapshot("s2") });
    assert_eq!(h.app.cur.session_id, "s2");
    assert_eq!(h.app.open_sessions(), vec!["s2", "s1"], "read-only traces are not listed as resumable");
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
    Notice::Trace { session_id: id.into(), history: vec![Message::user("older work"), Message::assistant(answer, vec![])] }
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
fn hovering_previews_and_open_sessions_preview_live() {
    let mut h = harness_with_session();
    let screen = h.screen();
    let y = screen.lines().position(|l| l.contains("older work")).unwrap() as u16;
    let mv = |row: u16, col: u16| Event::Mouse(MouseEvent { kind: MouseEventKind::Moved, column: col, row, modifiers: KeyModifiers::NONE });
    h.app.on_event(mv(y, 4));
    h.draw();
    assert!(!h.app.sidebar_focus, "hovering does not take focus");
    assert_eq!(h.request(), Request::ReadTrace { session_id: "s0".into() });
    h.notice(trace_of("s0", "hover answer"));
    assert!(h.screen().contains("hover answer"));
    h.app.on_event(mv(5, 60));
    h.draw();
    assert!(!h.screen().contains("PREVIEW"), "moving off the rail ends the preview");

    // A session open in this TUI previews from its live view, including background output.
    h.notice(Notice::Opened { snapshot: snapshot("s0") });
    h.notice(Notice::Event { session_id: "s0".into(), event: AgentEvent::TurnStarted { text: "live job".into() } });
    h.notice(Notice::Event { session_id: "s0".into(), event: AgentEvent::TextDelta("streaming now".into()) });
    h.app.on_event(mv(y, 4));
    h.draw();
    assert!(h.rx.try_recv().is_err(), "no trace request for an open session");
    let screen = h.screen();
    assert!(screen.contains("streaming now"), "{screen}");
    assert_eq!(h.app.cur.session_id, "s1");
}
