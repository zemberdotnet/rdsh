//! Left rail: this directory's recorded sessions with their subagents nested beneath them.
//!
//! The model is rebuilt from `trace::SessionInfo` rows plus the subagents this process has seen
//! start (`AgentEvent::SubagentStarted`), so a subagent appears as soon as it starts even
//! before its trace has a title on disk.

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, SystemTime},
};

use unicode_width::UnicodeWidthChar;

use crate::trace::SessionInfo;

/// Finished subagents older than this are hidden from the rail until `zh` shows them.
pub const STALE_SUBAGENT: Duration = Duration::from_secs(5 * 60);

/// How a subagent is doing right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Live {
    /// A `task` call is in flight. Only known for this process.
    Running,
    Done,
    Failed,
}

/// A subagent this process watched start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveSub {
    pub parent: String,
    pub title: String,
    pub state: Live,
    /// When it stopped running, once it has.
    pub finished: Option<SystemTime>,
}

/// One row of the rail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub id: String,
    /// Index of the host the session lives on.
    pub host: usize,
    /// A host heading (shown when several hosts are attached); `id` is empty.
    pub header: bool,
    /// Indent level: 0 for a session, 1 for its subagents.
    pub depth: u8,
    pub title: String,
    /// `Running` for a session mid-turn in the hub; for subagents, the outcome seen, if any.
    pub live: Option<Live>,
    /// For a subagent that is not running: when it finished (or its trace was last written).
    pub finished: Option<SystemTime>,
}

/// The rail's rows in display order: sessions newest first, each followed by its subagents
/// in the order they started.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sidebar {
    rows: Vec<Row>,
}

/// `N` of a `<parent>-sub<N>` id, for ordering subagents by start.
fn sub_number(id: &str) -> usize {
    id.rsplit_once("-sub").and_then(|(_, n)| n.parse().ok()).unwrap_or(usize::MAX)
}

impl Sidebar {
    /// Builds the rail.
    ///
    /// @param infos sessions from `trace::list_sessions` (newest first).
    /// @param host index stored in every row.
    /// @param cwd when set, only sessions recorded in this working directory are listed.
    /// @param live subagents seen starting, keyed by session id.
    /// @param running sessions whose agent is mid-turn in the hub.
    pub fn build(
        host: usize,
        infos: &[SessionInfo],
        cwd: Option<&str>,
        live: &HashMap<String, LiveSub>,
        running: &HashSet<String>,
    ) -> Self {
        let mut subs: HashMap<&str, Vec<Row>> = HashMap::new();
        for s in infos {
            if let Some(p) = &s.parent {
                let l = live.get(&s.id);
                // The `task` description is the short label; the trace title is the full prompt.
                let title = l.map(|l| l.title.clone()).filter(|t| !t.is_empty()).unwrap_or_else(|| s.title.clone());
                let finished = match l {
                    Some(l) if l.state == Live::Running => None,
                    Some(l) => l.finished.or(Some(s.modified)),
                    None => Some(s.modified),
                };
                subs.entry(p.as_str()).or_default().push(Row {
                    id: s.id.clone(),
                    host,
                    header: false,
                    depth: 1,
                    title,
                    live: l.map(|l| l.state),
                    finished,
                });
            }
        }
        // Subagents whose trace is not listed yet (just started, or no trace directory).
        for (id, l) in live {
            let known = subs.get(l.parent.as_str()).is_some_and(|v| v.iter().any(|r| r.id == *id));
            if !known {
                subs.entry(l.parent.as_str()).or_default().push(Row {
                    id: id.clone(),
                    host,
                    header: false,
                    depth: 1,
                    title: l.title.clone(),
                    live: Some(l.state),
                    finished: l.finished,
                });
            }
        }
        let mut rows = Vec::new();
        for s in infos.iter().filter(|s| s.parent.is_none() && cwd.is_none_or(|c| s.cwd == c)) {
            let live = running.contains(&s.id).then_some(Live::Running);
            let title = s.slug.clone().unwrap_or_else(|| s.title.clone());
            rows.push(Row { id: s.id.clone(), host, header: false, depth: 0, title, live, finished: None });
            if let Some(mut children) = subs.remove(s.id.as_str()) {
                children.sort_by_key(|r| sub_number(&r.id));
                rows.extend(children);
            }
        }
        Self { rows }
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// Index of the row for session `id` on `host`, if listed.
    pub fn index_of(&self, host: usize, id: &str) -> Option<usize> {
        self.rows.iter().position(|r| !r.header && r.host == host && r.id == id)
    }

    /// Drops subagent rows that finished before `cutoff`, except those `keep` names and the
    /// subagents of a session still mid-turn (their traces can go quiet during a long tool call).
    ///
    /// @return how many rows were dropped.
    pub fn hide_finished_before(&mut self, cutoff: SystemTime, keep: impl Fn(&Row) -> bool) -> usize {
        let before = self.rows.len();
        let mut parent_running = false;
        self.rows.retain(|r| {
            if r.depth == 0 {
                parent_running = r.live == Some(Live::Running);
                return true;
            }
            parent_running || keep(r) || r.finished.is_none_or(|t| t >= cutoff)
        });
        before - self.rows.len()
    }

    /// Joins per-host rails, each under a heading row titled with the host's name.
    pub fn grouped(parts: Vec<(String, Sidebar)>) -> Self {
        let mut rows = Vec::new();
        for (i, (name, part)) in parts.into_iter().enumerate() {
            rows.push(Row { id: String::new(), host: i, header: true, depth: 0, title: name, live: None, finished: None });
            rows.extend(part.rows);
        }
        Self { rows }
    }

    /// Shortens `label` (whitespace collapsed) to at most `width` display columns, marking a cut.
    pub fn fit(label: &str, width: usize) -> String {
        let label = label.split_whitespace().collect::<Vec<_>>().join(" ");
        let total: usize = label.chars().map(|c| c.width().unwrap_or(0)).sum();
        if total <= width {
            return label;
        }
        let mut out = String::new();
        let mut w = 0;
        for c in label.chars() {
            let cw = c.width().unwrap_or(0);
            if w + cw + 1 > width {
                break;
            }
            out.push(c);
            w += cw;
        }
        if width > 0 {
            out.push('…');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::SystemTime};

    use super::*;

    fn info(id: &str, title: &str, parent: Option<&str>, cwd: &str) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            path: PathBuf::from(format!("{id}.jsonl")),
            modified: SystemTime::now(),
            parent: parent.map(str::to_owned),
            cwd: cwd.into(),
            has_input: true,
            title: title.into(),
            slug: None,
        }
    }

    fn ids(sb: &Sidebar) -> Vec<(&str, u8)> {
        sb.rows().iter().map(|r| (r.id.as_str(), r.depth)).collect()
    }

    #[test]
    fn nests_subagents_in_start_order_and_filters_by_directory() {
        let infos = vec![
            info("b-sub10", "tenth", Some("b"), "/p"),
            info("b-sub2", "second", Some("b"), "/p"),
            info("b", "refactor the parser", None, "/p"),
            info("other", "elsewhere", None, "/q"),
            info("a", "fix the bug", None, "/p"),
        ];
        let sb = Sidebar::build(0, &infos, Some("/p"), &HashMap::new(), &HashSet::new());
        assert_eq!(ids(&sb), [("b", 0), ("b-sub2", 1), ("b-sub10", 1), ("a", 0)]);
        assert_eq!(sb.index_of(0, "a"), Some(3));
        assert_eq!(sb.index_of(0, "other"), None);
    }

    #[test]
    fn live_subagents_show_before_their_trace_has_a_title() {
        let infos = vec![info("b-sub1", "", Some("b"), "/p"), info("b", "root", None, "/p")];
        let mut live = HashMap::new();
        live.insert("b-sub1".into(), LiveSub { parent: "b".into(), title: "scan lexer".into(), state: Live::Done, finished: None });
        live.insert("b-sub2".into(), LiveSub { parent: "b".into(), title: "check docs".into(), state: Live::Running, finished: None });
        let sb = Sidebar::build(0, &infos, Some("/p"), &live, &HashSet::new());
        let rows = sb.rows();
        assert_eq!(ids(&sb), [("b", 0), ("b-sub1", 1), ("b-sub2", 1)]);
        assert_eq!((rows[1].title.as_str(), rows[1].live), ("scan lexer", Some(Live::Done)));
        assert_eq!((rows[2].title.as_str(), rows[2].live), ("check docs", Some(Live::Running)));
    }

    #[test]
    fn live_task_label_wins_over_the_trace_prompt() {
        let infos = vec![info("b-sub1", "read Cargo.toml and report", Some("b"), "/p"), info("b", "root", None, "/p")];
        let mut live = HashMap::new();
        live.insert("b-sub1".into(), LiveSub { parent: "b".into(), title: "inspect manifest".into(), state: Live::Done, finished: None });
        assert_eq!(Sidebar::build(0, &infos, Some("/p"), &live, &HashSet::new()).rows()[1].title, "inspect manifest");
        assert_eq!(Sidebar::build(0, &infos, Some("/p"), &HashMap::new(), &HashSet::new()).rows()[1].title, "read Cargo.toml and report");
    }

    #[test]
    fn finished_subagents_hide_after_a_while() {
        let old = SystemTime::now() - STALE_SUBAGENT * 2;
        let mut stale = info("b-sub1", "old", Some("b"), "/p");
        stale.modified = old;
        let mut kept = info("b-sub3", "old but open", Some("b"), "/p");
        kept.modified = old;
        let mut busy = info("c-sub1", "quiet but parent busy", Some("c"), "/p");
        busy.modified = old;
        let infos = vec![
            stale,
            info("b-sub2", "recent", Some("b"), "/p"),
            kept,
            info("b", "root", None, "/p"),
            busy,
            info("c", "running", None, "/p"),
        ];
        let mut live = HashMap::new();
        // Running in this process: never hidden, however old its trace.
        live.insert("b-sub4".into(), LiveSub { parent: "b".into(), title: "live".into(), state: Live::Running, finished: None });
        // Finished here long ago: its own finish time wins over a fresh trace.
        live.insert("b-sub2".into(), LiveSub { parent: "b".into(), title: "recent".into(), state: Live::Done, finished: Some(old) });
        let running = HashSet::from(["c".to_owned()]);
        let mut sb = Sidebar::build(0, &infos, Some("/p"), &live, &running);
        let hidden = sb.hide_finished_before(SystemTime::now() - STALE_SUBAGENT, |r| r.id == "b-sub3");
        assert_eq!(hidden, 2);
        assert_eq!(ids(&sb), [("b", 0), ("b-sub3", 1), ("b-sub4", 1), ("c", 0), ("c-sub1", 1)]);
    }

    #[test]
    fn sessions_show_their_short_title_when_they_have_one() {
        let mut named = info("b", "please refactor the whole parser module so that it", None, "/p");
        named.slug = Some("refactor parser".into());
        let infos = vec![named, info("a", "fix the bug", None, "/p")];
        let sb = Sidebar::build(0, &infos, Some("/p"), &HashMap::new(), &HashSet::new());
        let titles: Vec<_> = sb.rows().iter().map(|r| r.title.as_str()).collect();
        assert_eq!(titles, ["refactor parser", "fix the bug"]);
    }

    #[test]
    fn fit_uses_display_width() {
        assert_eq!(Sidebar::fit("hello", 10), "hello");
        assert_eq!(Sidebar::fit("a very long session title", 10), "a very lo…");
        assert_eq!(Sidebar::fit("multi\nline   title", 40), "multi line title");
        // Each CJK char is two columns wide.
        let cjk = Sidebar::fit("修复解析器错误", 7);
        assert_eq!(cjk, "修复解…");
        assert_eq!(Sidebar::fit("x", 0), "");
    }
}
