//! Git-backed views of a file: its current diff against HEAD and the full file with
//! changed lines marked.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `diff --git`, `index`, `---`, `+++` and similar header lines.
    Meta,
    /// `@@ -a,b +c,d @@`
    Hunk,
    Add,
    Del,
    Ctx,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DiffLine {
    pub kind: Kind,
    /// Line text without the diff prefix (`+`, `-`, ` `); full text for Meta/Hunk.
    pub text: String,
    pub old: Option<usize>,
    pub new: Option<usize>,
}

impl DiffLine {
    /// Prefix used when quoting this line in a comment.
    pub fn sign(&self) -> &'static str {
        match self.kind {
            Kind::Add => "+",
            Kind::Del => "-",
            Kind::Ctx => " ",
            Kind::Meta | Kind::Hunk => "",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Tracked with uncommitted changes.
    Modified,
    /// Not tracked by git; every line counts as added.
    Untracked,
    /// Tracked and identical to HEAD.
    Clean,
    /// Not inside a git work tree.
    NoRepo,
}

pub struct FileDiff {
    pub path: PathBuf,
    /// Path relative to the working directory when possible.
    pub rel: String,
    pub status: Status,
    pub diff: Vec<DiffLine>,
    /// Entire current file; lines added relative to HEAD have `Kind::Add`.
    pub full: Vec<DiffLine>,
    pub added: usize,
    pub removed: usize,
    /// Set when the file could not be read (binary, missing, permission).
    pub error: Option<String>,
}

const MAX_FULL_LINES: usize = 50_000;

fn git(cwd: &Path, args: &[&str]) -> Option<(bool, String)> {
    let out = Command::new("git").arg("-C").arg(cwd).args(args).output().ok()?;
    Some((out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned()))
}

/// Loads the diff and full views of `path`.
///
/// @param cwd working directory (used for git and relative display paths).
/// @param path file to show; need not be tracked.
/// @returns both views; never fails, problems are reported in `error`.
pub fn load(cwd: &Path, path: &Path) -> FileDiff {
    let rel = path.strip_prefix(cwd).unwrap_or(path).to_string_lossy().into_owned();
    let dir = path.parent().filter(|p| p.is_dir()).unwrap_or(cwd);
    let p = path.to_string_lossy();
    let in_repo = git(dir, &["rev-parse", "--is-inside-work-tree"]).is_some_and(|(ok, s)| ok && s.trim() == "true");
    let (status, diff_text) = if !in_repo {
        (Status::NoRepo, String::new())
    } else if !git(dir, &["ls-files", "--error-unmatch", "--", &p]).is_some_and(|(ok, _)| ok) {
        (Status::Untracked, String::new())
    } else {
        let args = ["diff", "--no-color", "--no-ext-diff", "HEAD", "--", &p];
        let text = match git(dir, &args) {
            Some((true, t)) => t,
            // No HEAD yet (fresh repository): compare against the index.
            _ => git(dir, &["diff", "--no-color", "--no-ext-diff", "--", &p]).map(|(_, t)| t).unwrap_or_default(),
        };
        (if text.trim().is_empty() { Status::Clean } else { Status::Modified }, text)
    };

    let (content, error) = match std::fs::read(path) {
        Ok(b) if b[..b.len().min(8000)].contains(&0) => (String::new(), Some("binary file".to_owned())),
        Ok(b) => (String::from_utf8_lossy(&b).into_owned(), None),
        Err(e) => (String::new(), Some(e.to_string())),
    };

    let mut diff = parse_unified(&diff_text);
    if status == Status::Untracked {
        diff = content
            .lines()
            .enumerate()
            .map(|(i, l)| DiffLine { kind: Kind::Add, text: l.to_owned(), old: None, new: Some(i + 1) })
            .collect();
        if !diff.is_empty() {
            diff.insert(0, DiffLine {
                kind: Kind::Hunk,
                text: format!("@@ -0,0 +1,{} @@ (untracked)", diff.len()),
                old: None,
                new: None,
            });
        }
    }
    let added_lines: HashSet<usize> = diff.iter().filter(|d| d.kind == Kind::Add).filter_map(|d| d.new).collect();
    let added = diff.iter().filter(|d| d.kind == Kind::Add).count();
    let removed = diff.iter().filter(|d| d.kind == Kind::Del).count();
    let full = content
        .lines()
        .take(MAX_FULL_LINES)
        .enumerate()
        .map(|(i, l)| DiffLine {
            kind: if added_lines.contains(&(i + 1)) { Kind::Add } else { Kind::Ctx },
            text: l.to_owned(),
            old: None,
            new: Some(i + 1),
        })
        .collect();
    FileDiff { path: path.to_owned(), rel, status, diff, full, added, removed, error }
}

/// Parses `git diff` output into typed lines with old/new line numbers.
pub fn parse_unified(text: &str) -> Vec<DiffLine> {
    let mut out = Vec::new();
    let (mut old, mut new) = (0usize, 0usize);
    let mut in_hunk = false;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("@@") {
            // @@ -a,b +c,d @@ context
            let mut nums = rest.split_whitespace();
            old = nums.next().and_then(|s| s.trim_start_matches('-').split(',').next()?.parse().ok()).unwrap_or(1);
            new = nums.next().and_then(|s| s.trim_start_matches('+').split(',').next()?.parse().ok()).unwrap_or(1);
            in_hunk = true;
            out.push(DiffLine { kind: Kind::Hunk, text: line.to_owned(), old: None, new: None });
            continue;
        }
        if !in_hunk || line.starts_with("diff --git") {
            in_hunk = false;
            out.push(DiffLine { kind: Kind::Meta, text: line.to_owned(), old: None, new: None });
            continue;
        }
        let (kind, body) = match line.chars().next() {
            Some('+') => (Kind::Add, &line[1..]),
            Some('-') => (Kind::Del, &line[1..]),
            Some(' ') => (Kind::Ctx, &line[1..]),
            Some('\\') => {
                out.push(DiffLine { kind: Kind::Meta, text: line.to_owned(), old: None, new: None });
                continue;
            }
            _ => (Kind::Ctx, line),
        };
        let (o, n) = match kind {
            Kind::Add => {
                new += 1;
                (None, Some(new - 1))
            }
            Kind::Del => {
                old += 1;
                (Some(old - 1), None)
            }
            _ => {
                old += 1;
                new += 1;
                (Some(old - 1), Some(new - 1))
            }
        };
        out.push(DiffLine { kind, text: body.to_owned(), old: o, new: n });
    }
    out
}

/// Files with uncommitted changes (`git status --porcelain`), as (status, path) pairs.
pub fn changed_files(cwd: &Path) -> Vec<(String, String)> {
    let Some((true, text)) = git(cwd, &["status", "--porcelain=v1", "--untracked-files=all"]) else {
        return Vec::new();
    };
    // Porcelain paths are relative to the repository root.
    let root = git(cwd, &["rev-parse", "--show-toplevel"]).map(|(_, s)| PathBuf::from(s.trim()));
    text.lines()
        .filter(|l| l.len() > 3)
        .map(|l| {
            let path = l[3..].rsplit(" -> ").next().unwrap_or(&l[3..]).trim_matches('"');
            let abs = root.as_ref().map_or_else(|| PathBuf::from(path), |r| r.join(path));
            let shown = abs.strip_prefix(cwd).map_or_else(|_| abs.to_string_lossy().into_owned(), |p| p.to_string_lossy().into_owned());
            (l[..2].trim().to_owned(), shown)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hunks_with_line_numbers() {
        let d = parse_unified(
            "diff --git a/x b/x\nindex 1..2 100644\n--- a/x\n+++ b/x\n@@ -2,3 +2,4 @@ fn\n ctx\n-old\n+new\n+more\n ctx2\n",
        );
        let kinds: Vec<Kind> = d.iter().map(|l| l.kind).collect();
        assert_eq!(kinds[..5], [Kind::Meta, Kind::Meta, Kind::Meta, Kind::Meta, Kind::Hunk]);
        let body: Vec<_> = d[5..].iter().map(|l| (l.kind, l.old, l.new, l.text.as_str())).collect();
        assert_eq!(body, vec![
            (Kind::Ctx, Some(2), Some(2), "ctx"),
            (Kind::Del, Some(3), None, "old"),
            (Kind::Add, None, Some(3), "new"),
            (Kind::Add, None, Some(4), "more"),
            (Kind::Ctx, Some(4), Some(5), "ctx2"),
        ]);
    }

    #[test]
    fn loads_git_states() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let run = |args: &[&str]| {
            assert!(Command::new("git").arg("-C").arg(root).args(args).output().unwrap().status.success());
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "init"]);
        assert_eq!(load(root, &root.join("a.txt")).status, Status::Clean);
        std::fs::write(root.join("a.txt"), "one\nTWO\nthree\n").unwrap();
        let f = load(root, &root.join("a.txt"));
        assert_eq!(f.status, Status::Modified);
        assert_eq!((f.added, f.removed), (1, 1));
        assert_eq!(f.full[1].kind, Kind::Add);
        assert_eq!(f.rel, "a.txt");
        std::fs::write(root.join("b.txt"), "new\n").unwrap();
        let b = load(root, &root.join("b.txt"));
        assert_eq!(b.status, Status::Untracked);
        assert_eq!(b.diff.len(), 2);
        let changed = changed_files(root);
        assert!(changed.contains(&("M".into(), "a.txt".into())));
        assert!(changed.contains(&("??".into(), "b.txt".into())));
    }
}
