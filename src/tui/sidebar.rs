//! Left rail: recorded sessions with their subagents nested beneath them.
//!
//! The model is built purely from `trace::SessionInfo` rows plus the live subagent calls the
//! agent has reported so far, so it can be rebuilt at any time without touching the agent.

use std::collections::HashMap;

use crate::trace::SessionInfo;

/// How a subagent is doing right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Live {
    /// A `task` call is in flight. Only visible for this process.
    Running,
    Done,
    Failed,
}

/// One row of the rail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub id: String,
    /// Indent level: 0 for a session, 1 for its subagents.
    pub depth: u8,
    pub title: String,
    /// `None` for sessions; `Some` only when this process knows the outcome.
    pub live: Option<Live>,
}

/// A top-level session and the subagents recorded beneath it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Root {
    pub id: String,
    pub title: String,
    pub subs: Vec<Row>,
}

/// The rail's contents.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sidebar {
    /// Newest first, as `trace::list_sessions` returns them.
    pub roots: Vec<Root>,
}

impl Sidebar {
    /// Groups recorded sessions by parent, newest first within each group.
    ///
    /// @param infos sessions from `trace::list_sessions` (newest first); subagent rows are the
    ///              ones whose `parent` names another session in the list.
    /// @param live  subagent id → outcome, for subagents this process watched run.
    pub fn build(infos: &[SessionInfo], live: &HashMap<String, Live>) -> Self {
        let mut roots: Vec<Root> = Vec::new();
        let mut sub_of: HashMap<&str, Vec<Row>> = HashMap::new();
        for s in infos {
            match &s.parent {
                Some(p) => sub_of.entry(p.as_str()).or_default().push(Row {
                    id: s.id.clone(),
                    depth: 1,
                    title: s.title.clone(),
                    live: live.get(&s.id).copied(),
                }),
                None => roots.push(Root { id: s.id.clone(), title: s.title.clone(), subs: Vec::new() }),
            }
        }
        for root in &mut roots {
            if let Some(mut subs) = sub_of.remove(root.id.as_str()) {
                // `infos` is newest first; subagents read better oldest first.
                subs.reverse();
                root.subs = subs;
            }
        }
        Self { roots }
    }

    /// Rows in display order.
    pub fn rows(&self) -> Vec<Row> {
        let mut out = Vec::with_capacity(self.roots.len() * 2);
        for root in &self.roots {
            out.push(Row { id: root.id.clone(), depth: 0, title: root.title.clone(), live: None });
            out.extend(root.subs.iter().cloned());
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// Index in `rows()` of the row for `id`, if present.
    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.rows().iter().position(|r| r.id == id)
    }

    /// Shortens a label to `width` columns, marking the cut.
    pub fn fit(label: &str, indent: u8, width: u16) -> String {
        let avail = (width as usize).saturating_sub(indent as usize * 2 + 1);
        let label: String = label.split_whitespace().collect::<Vec<_>>().join(" ");
        if avail == 0 {
            return String::new();
        }
        if label.chars().count() <= avail {
            return label;
        }
        let mut s: String = label.chars().take(avail - 1).collect();
        s.push('…');
        s
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::SystemTime;

    use super::*;

    fn info(id: &str, title: &str, parent: Option<&str>) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            path: PathBuf::from(format!("{id}.jsonl")),
            modified: SystemTime::now(),
            parent: parent.map(str::to_owned),
            title: title.into(),
        }
    }

    #[test]
    fn nests_subagents_under_their_parent() {
        let infos = vec![
            info("b-sub2", "second task", Some("b")),
            info("b-sub1", "first task", Some("b")),
            info("b", "refactor the parser", None),
            info("a", "fix the bug", None),
        ];
        let mut live = HashMap::new();
        live.insert("b-sub2".to_string(), Live::Running);
        let sb = Sidebar::build(&infos, &live);
        let roots: Vec<_> = sb.roots.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(roots, ["b", "a"]);
        let subs: Vec<_> = sb.roots[0].subs.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(subs, ["b-sub1", "b-sub2"]);
        assert_eq!(sb.roots[0].subs[1].live, Some(Live::Running));
        assert_eq!(sb.roots[0].subs[0].live, None);
        assert!(sb.roots[1].subs.is_empty());
    }

    #[test]
    fn rows_are_depth_first_and_indexed() {
        let infos = vec![info("b-sub1", "sub", Some("b")), info("b", "root", None)];
        let sb = Sidebar::build(&infos, &HashMap::new());
        let rows = sb.rows();
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].id.as_str(), rows[0].depth), ("b", 0));
        assert_eq!((rows[1].id.as_str(), rows[1].depth), ("b-sub1", 1));
        assert_eq!(sb.index_of("b-sub1"), Some(1));
        assert_eq!(sb.index_of("missing"), None);
    }

    #[test]
    fn orphaned_subagents_are_dropped() {
        let infos = vec![info("gone-sub1", "lost", Some("gone"))];
        let sb = Sidebar::build(&infos, &HashMap::new());
        assert!(sb.is_empty());
        assert!(sb.rows().is_empty());
    }

    #[test]
    fn fit_truncates_to_the_rail_width() {
        let short = Sidebar::fit("hello", 0, 10);
        assert_eq!(short, "hello");
        let long = Sidebar::fit("a very long session title indeed", 0, 10);
        assert_eq!(long.chars().count(), 9);
        assert!(long.ends_with('…'));
        let indented = Sidebar::fit("abcdefghij", 1, 10);
        assert_eq!(indented.chars().count(), 7);
        assert_eq!(Sidebar::fit("x", 0, 0), "");
        assert_eq!(Sidebar::fit("multi\nline   title", 0, 40), "multi line title");
    }
}
