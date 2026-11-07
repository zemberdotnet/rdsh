//! Diff/full-file viewer state for one file opened from the conversation.

use std::path::Path;

use super::{
    highlight::{self, Run},
    motion::{Cursor, Lines, Pos},
};
use crate::{
    comments::Anchor,
    diff::{self, DiffLine, FileDiff, Kind, Status},
    util::sanitize,
};

/// Display text of each visible line.
pub struct TextLines(pub Vec<String>);

impl Lines for TextLines {
    fn count(&self) -> usize {
        self.0.len()
    }
    fn line(&self, row: usize) -> &str {
        &self.0[row]
    }
}

pub struct FileView {
    pub file: FileDiff,
    /// Showing the whole file (true) or only the diff hunks (false).
    pub full: bool,
    pub lines: Vec<DiffLine>,
    pub texts: TextLines,
    /// Rows where hunks (diff) or change groups (full) start.
    pub blocks: Vec<usize>,
    pub cursor: Cursor,
    pub scroll: usize,
    /// `(host, cwd)` when the file lives on a remote host (reloads go through it).
    pub remote: Option<(usize, String)>,
    /// First visible text column (long lines scroll horizontally with the cursor).
    pub hscroll: usize,
    /// Syntax runs per line of the current file (index = new line number - 1).
    hl_new: Vec<Vec<Run>>,
    /// Syntax runs per line of the HEAD version (index = old line number - 1).
    hl_old: Vec<Vec<Run>>,
}

fn is_noise(l: &DiffLine) -> bool {
    l.kind == Kind::Meta
        && ["diff --git", "index ", "--- ", "+++ ", "new file mode", "deleted file mode", "similarity", "rename "]
            .iter()
            .any(|p| l.text.starts_with(p))
}

impl FileView {
    /// Opens `file`, starting in diff mode when there are changes.
    ///
    /// @param line 1-based line to place the cursor on, if known.
    pub fn new(file: FileDiff, line: Option<usize>) -> Self {
        let full = file.diff.is_empty();
        let mut v = Self {
            file,
            full,
            lines: Vec::new(),
            texts: TextLines(Vec::new()),
            blocks: Vec::new(),
            cursor: Cursor::default(),
            scroll: 0,
            hscroll: 0,
            remote: None,
            hl_new: Vec::new(),
            hl_old: Vec::new(),
        };
        v.highlight();
        v.rebuild();
        match line {
            Some(l) => v.jump_to_line(l),
            None => {
                if let Some(&b) = v.blocks.first() {
                    v.cursor.pos.row = if v.full { b } else { (b + 1).min(v.lines.len().saturating_sub(1)) };
                }
            }
        }
        v
    }

    fn rebuild(&mut self) {
        let src = if self.full { &self.file.full } else { &self.file.diff };
        self.lines = src.iter().filter(|l| !is_noise(l)).cloned().collect();
        self.texts = TextLines(self.lines.iter().map(|l| sanitize(&l.text)).collect());
        self.blocks = if self.full {
            self.lines
                .iter()
                .enumerate()
                .filter(|(i, l)| l.kind == Kind::Add && (*i == 0 || self.lines[i - 1].kind != Kind::Add))
                .map(|(i, _)| i)
                .collect()
        } else {
            self.lines.iter().enumerate().filter(|(_, l)| l.kind == Kind::Hunk).map(|(i, _)| i).collect()
        };
        self.cursor.clamp(&self.texts);
    }

    fn highlight(&mut self) {
        let current = self.file.full.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");
        self.hl_new = highlight::highlight(&self.file.path, &current).unwrap_or_default();
        self.hl_old = self
            .file
            .head_source
            .as_deref()
            .and_then(|s| highlight::highlight(&self.file.path, s))
            .unwrap_or_default();
    }

    /// Syntax runs for display row `row`: added and context lines use the current file,
    /// deleted lines the HEAD version.
    pub fn syntax(&self, row: usize) -> &[Run] {
        let Some(l) = self.lines.get(row) else { return &[] };
        let (runs, n) = match l.kind {
            Kind::Add | Kind::Ctx => (&self.hl_new, l.new),
            Kind::Del => (&self.hl_old, l.old),
            Kind::Hunk | Kind::Meta => return &[],
        };
        n.and_then(|n| runs.get(n - 1)).map_or(&[], Vec::as_slice)
    }

    /// Current line number under the cursor (new side, else old side).
    pub fn current_line_number(&self) -> Option<usize> {
        self.lines.get(self.cursor.pos.row).and_then(|l| l.new.or(l.old))
    }

    /// Moves the cursor to the first row showing new-side line `line` (or the closest after it).
    pub fn jump_to_line(&mut self, line: usize) {
        let row = self
            .lines
            .iter()
            .position(|l| l.new.is_some_and(|n| n >= line))
            .or_else(|| self.lines.iter().rposition(|l| l.new.is_some()))
            .unwrap_or(0);
        self.cursor.jump(Pos { row, col: 0 }, &self.texts);
    }

    /// Switches between diff and full-file mode, keeping the line under the cursor.
    pub fn toggle_full(&mut self) {
        let line = self.current_line_number();
        self.full = !self.full;
        self.rebuild();
        if let Some(l) = line {
            self.jump_to_line(l);
        }
    }

    /// Shows a freshly loaded copy of the same file, keeping the line under the cursor.
    pub fn replace(&mut self, file: FileDiff) {
        let line = self.current_line_number();
        self.file = file;
        self.full = self.full || self.file.diff.is_empty();
        self.highlight();
        self.rebuild();
        if let Some(l) = line {
            self.jump_to_line(l);
        }
    }

    /// Re-reads the file and its diff from disk.
    pub fn reload(&mut self, cwd: &Path) {
        let line = self.current_line_number();
        let full = self.full;
        self.file = diff::load(cwd, &self.file.path.clone());
        self.highlight();
        self.full = full || self.file.diff.is_empty();
        self.rebuild();
        if let Some(l) = line {
            self.jump_to_line(l);
        }
    }

    /// Builds a comment anchor for rows `a..=b`.
    pub fn anchor(&self, a: usize, b: usize) -> Anchor {
        let last = self.lines.len().saturating_sub(1);
        let (a, b) = (a.min(b).min(last), a.max(b).min(last));
        let sel = &self.lines[a..=b];
        let nums: Vec<usize> = sel.iter().filter_map(|l| l.new.or(l.old)).collect();
        let lines = match (nums.iter().min(), nums.iter().max()) {
            (Some(&lo), Some(&hi)) => Some((lo, hi)),
            _ => None,
        };
        let snippet = sel.iter().map(|l| format!("{}{}", l.sign(), l.text)).collect::<Vec<_>>().join("\n");
        Anchor::File { path: self.file.rel.clone(), lines, snippet }
    }

    pub fn title(&self) -> String {
        let f = &self.file;
        let state = match f.status {
            Status::Modified => format!("+{} -{}", f.added, f.removed),
            Status::Untracked => format!("untracked, {} lines", f.full.len()),
            Status::Clean => "no changes".into(),
            Status::NoRepo => "not in a git repository".into(),
        };
        let mode = if self.full { "file" } else { "diff" };
        match &f.error {
            Some(e) => format!("{} — {e}", f.rel),
            None => format!("{} — {state} — {mode} view", f.rel),
        }
    }
}
