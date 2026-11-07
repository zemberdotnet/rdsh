//! Detects references to existing files (`src/main.rs`, `./a/b.txt:42`) in displayed text.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};

use regex::Regex;

/// A clickable file reference inside one line. `start..end` are char offsets.
#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    pub start: usize,
    pub end: usize,
    pub path: Arc<PathBuf>,
    pub line: Option<usize>,
}

static CANDIDATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[~\w./@+\-]+(?::\d+){0,2}").expect("valid link regex"));

/// Resolves candidate strings to files under a working directory, caching filesystem checks.
pub struct LinkResolver {
    cwd: PathBuf,
    cache: HashMap<String, Option<Arc<PathBuf>>>,
    /// Files live on another machine: accept path-like candidates without checking the disk.
    trust: bool,
}

/// Extensions treated as file names when files cannot be checked (remote sessions).
const KNOWN_EXTENSIONS: &[&str] = &[
    "rs", "py", "js", "ts", "tsx", "jsx", "go", "c", "h", "cc", "cpp", "hpp", "java", "kt", "rb", "md", "toml", "json",
    "yaml", "yml", "nix", "sh", "txt", "lock", "html", "css", "sql", "proto", "cfg", "ini", "xml",
];

impl LinkResolver {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd, cache: HashMap::new(), trust: false }
    }

    /// A resolver for files on another machine: paths with a `/` or a known extension link
    /// without a filesystem check.
    pub fn trusting(cwd: PathBuf) -> Self {
        Self { cwd, cache: HashMap::new(), trust: true }
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    fn resolve(&mut self, candidate: &str) -> Option<Arc<PathBuf>> {
        if let Some(hit) = self.cache.get(candidate) {
            return hit.clone();
        }
        let path = crate::tools::resolve_path(&self.cwd, candidate);
        let found = if self.trust {
            let ext = Path::new(candidate).extension().and_then(|e| e.to_str()).unwrap_or("");
            (candidate.contains('/') || KNOWN_EXTENSIONS.contains(&ext)).then(|| Arc::new(path))
        } else {
            path.is_file().then(|| Arc::new(path))
        };
        if self.cache.len() > 50_000 {
            self.cache.clear();
        }
        self.cache.insert(candidate.to_owned(), found.clone());
        found
    }

    /// Finds file references in `line`.
    ///
    /// @param line one display line (no newlines).
    /// @returns links in left-to-right order, with char offsets into `line`.
    pub fn find(&mut self, line: &str) -> Vec<Link> {
        let mut out = Vec::new();
        for m in CANDIDATE.find_iter(line) {
            let raw = m.as_str();
            let trimmed = raw.trim_end_matches(['.', ',', ':', ';']);
            let (path_part, line_no) = split_line_suffix(trimmed);
            if path_part.len() < 2 || !(path_part.contains('/') || path_part.contains('.')) {
                continue;
            }
            if path_part.chars().all(|c| c == '.' || c == '/') {
                continue;
            }
            let Some(path) = self.resolve(path_part) else { continue };
            let start = line[..m.start()].chars().count();
            let end = start + trimmed.chars().count();
            out.push(Link { start, end, path, line: line_no });
        }
        out
    }
}

/// Splits `path:12:5` into (`path`, Some(12)).
fn split_line_suffix(s: &str) -> (&str, Option<usize>) {
    let mut parts = s.splitn(3, ':');
    let path = parts.next().unwrap_or(s);
    let line = parts.next().and_then(|l| l.parse().ok());
    (path, line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_existing_files_with_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        let mut r = LinkResolver::new(dir.path().to_path_buf());
        let links = r.find("see `src/main.rs:42:7`, Cargo.toml. and src/nope.rs or e.g. 1.5");
        assert_eq!(links.len(), 2);
        assert_eq!((links[0].start, links[0].end, links[0].line), (5, 21, Some(42)));
        assert!(links[0].path.ends_with("src/main.rs"));
        assert_eq!(links[1].line, None);
        assert!(links[1].path.ends_with("Cargo.toml"));
        // grep-style output
        let l = r.find("src/main.rs:3: fn main() {}");
        assert_eq!(l[0].line, Some(3));
    }

    #[test]
    fn remote_paths_link_without_a_disk_check() {
        let mut r = LinkResolver::trusting(PathBuf::from("/srv/app"));
        let l = r.find("see src/main.rs:4, notes.md, e.g. 1.5 and v2.0");
        let found: Vec<_> = l.iter().map(|k| (k.path.display().to_string(), k.line)).collect();
        assert_eq!(found, [("/srv/app/src/main.rs".to_owned(), Some(4)), ("/srv/app/notes.md".to_owned(), None)]);
    }

    #[test]
    fn char_offsets_with_multibyte_text() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "").unwrap();
        let mut r = LinkResolver::new(dir.path().to_path_buf());
        let l = r.find("✓ édité a.rs");
        assert_eq!((l[0].start, l[0].end), (8, 12));
    }
}
