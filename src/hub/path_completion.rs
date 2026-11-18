//! Completes a path typed in the prompt, relative to a session's working directory.

use std::path::{Component, Path};

/// At most this many candidates are returned.
const LIMIT: usize = 100;

/// Entries of the directory named by `prefix` whose names start with its last part.
///
/// Paths stay inside `cwd` (absolute paths and `..` complete to nothing), files ignored by git
/// are left out, and hidden entries only show when the typed name starts with a dot.
/// Directories end with `/` and come first.
///
/// @returns workspace-relative paths, each starting with `prefix`.
pub fn complete(cwd: &Path, prefix: &str) -> Vec<String> {
    let rel = Path::new(prefix);
    if rel.components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir)) {
        return Vec::new();
    }
    let (dir, name) = match prefix.rfind('/') {
        Some(i) => (&prefix[..=i], &prefix[i + 1..]),
        None => ("", prefix),
    };
    let base = cwd.join(dir);
    if !base.is_dir() {
        return Vec::new();
    }
    let hidden = name.starts_with('.');
    let walk = ignore::WalkBuilder::new(&base)
        .max_depth(Some(1))
        .hidden(!hidden)
        .parents(true)
        .require_git(false)
        .build();
    let mut found: Vec<(bool, String)> = walk
        .filter_map(Result::ok)
        .filter(|e| e.depth() == 1)
        .filter_map(|e| {
            let file = e.file_name().to_string_lossy().into_owned();
            if !file.starts_with(name) || file == ".git" {
                return None;
            }
            let is_dir = e.file_type().is_some_and(|t| t.is_dir());
            Some((is_dir, format!("{dir}{file}{}", if is_dir { "/" } else { "" })))
        })
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    found.into_iter().take(LIMIT).map(|(_, p)| p).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completes_inside_the_workspace_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/tui")).unwrap();
        std::fs::write(root.join("src/main.rs"), "").unwrap();
        std::fs::write(root.join("src/mod.rs"), "").unwrap();
        std::fs::write(root.join(".env"), "").unwrap();
        std::fs::write(root.join("skip.log"), "").unwrap();
        std::fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        let cwd = root;

        assert_eq!(complete(cwd, "src/m"), ["src/main.rs", "src/mod.rs"]);
        assert_eq!(complete(cwd, "src/"), ["src/tui/", "src/main.rs", "src/mod.rs"]);
        assert_eq!(complete(cwd, "s"), ["src/"], "ignored files are left out");
        assert_eq!(complete(cwd, "."), [".env", ".gitignore"], "hidden entries need a dot");
        assert!(complete(cwd, "../").is_empty());
        assert!(complete(cwd, "/etc/").is_empty());
        assert!(complete(cwd, "nope/x").is_empty());
    }
}
