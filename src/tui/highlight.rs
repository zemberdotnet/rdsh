//! Tree-sitter syntax highlighting for the file view.
//!
//! Each language's configuration is built on first use and cached for the process. Output is
//! one list of style runs per source line, with columns in display chars (after the same tab
//! expansion and control-character stripping that `util::sanitize` applies).

use std::{path::Path, sync::OnceLock};

use ratatui::style::{Modifier, Style};
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

/// A styled char range `[start, end)` within one display line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    pub start: usize,
    pub end: usize,
    pub style: Style,
}

/// Files larger than this are shown without highlighting.
const MAX_BYTES: usize = 2 * 1024 * 1024;

/// Capture names we style; tree-sitter matches each capture to the longest listed prefix.
const NAMES: &[&str] = &[
    "attribute",
    "comment",
    "constant",
    "constant.builtin",
    "constructor",
    "embedded",
    "escape",
    "function",
    "function.builtin",
    "function.macro",
    "keyword",
    "label",
    "number",
    "operator",
    "property",
    "punctuation",
    "string",
    "string.special",
    "tag",
    "type",
    "type.builtin",
    "variable.builtin",
    "variable.parameter",
    "boolean",
    "module",
    "text.title",
    "text.literal",
    "text.uri",
    "markup.heading",
    "markup.raw",
    "markup.link",
];

fn style_for(name: &str) -> Style {
    use super::theme::*;
    let s = Style::default();
    match name {
        "comment" => s.fg(GRAY).add_modifier(Modifier::ITALIC),
        "keyword" => s.fg(RED),
        "string" | "string.special" | "text.literal" | "markup.raw" => s.fg(GREEN),
        "escape" => s.fg(ORANGE),
        "number" | "boolean" | "constant" | "constant.builtin" => s.fg(PURPLE),
        "function" | "function.builtin" => s.fg(GREEN).add_modifier(Modifier::BOLD),
        "function.macro" | "attribute" | "module" | "tag" | "label" => s.fg(AQUA),
        "type" | "type.builtin" | "constructor" => s.fg(YELLOW),
        "property" | "variable.parameter" => s.fg(BLUE),
        "variable.builtin" => s.fg(ORANGE),
        "operator" => s.fg(FG),
        "punctuation" => s.fg(FG_DIM),
        "text.title" | "markup.heading" => s.fg(ORANGE).add_modifier(Modifier::BOLD),
        "text.uri" | "markup.link" => s.fg(BLUE).add_modifier(Modifier::UNDERLINED),
        _ => s,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lang {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Go,
    C,
    Cpp,
    Json,
    Bash,
    Toml,
    Yaml,
    Nix,
    Markdown,
}

const LANGS: usize = 14;

fn detect(path: &Path) -> Option<Lang> {
    let name = path.file_name()?.to_str()?;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    Some(match ext {
        "rs" => Lang::Rust,
        "py" | "pyi" => Lang::Python,
        "js" | "mjs" | "cjs" | "jsx" => Lang::JavaScript,
        "ts" | "mts" | "cts" => Lang::TypeScript,
        "tsx" => Lang::Tsx,
        "go" => Lang::Go,
        "c" | "h" => Lang::C,
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => Lang::Cpp,
        "json" | "jsonc" => Lang::Json,
        "sh" | "bash" | "zsh" => Lang::Bash,
        "toml" => Lang::Toml,
        "yml" | "yaml" => Lang::Yaml,
        "nix" => Lang::Nix,
        "md" | "markdown" => Lang::Markdown,
        _ => match name {
            "Cargo.lock" => Lang::Toml,
            ".bashrc" | ".bash_profile" | ".profile" | ".zshrc" | ".envrc" => Lang::Bash,
            _ => return None,
        },
    })
}

fn build(lang: Lang) -> Option<HighlightConfiguration> {
    let (language, query): (tree_sitter::Language, String) = match lang {
        Lang::Rust => (tree_sitter_rust::LANGUAGE.into(), tree_sitter_rust::HIGHLIGHTS_QUERY.into()),
        Lang::Python => (tree_sitter_python::LANGUAGE.into(), tree_sitter_python::HIGHLIGHTS_QUERY.into()),
        Lang::JavaScript => (tree_sitter_javascript::LANGUAGE.into(), {
            format!("{}\n{}", tree_sitter_javascript::JSX_HIGHLIGHT_QUERY, tree_sitter_javascript::HIGHLIGHT_QUERY)
        }),
        // TypeScript's queries extend JavaScript's; the more specific patterns come first.
        Lang::TypeScript => (
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            format!("{}\n{}", tree_sitter_typescript::HIGHLIGHTS_QUERY, tree_sitter_javascript::HIGHLIGHT_QUERY),
        ),
        Lang::Tsx => (
            tree_sitter_typescript::LANGUAGE_TSX.into(),
            format!(
                "{}\n{}\n{}",
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                tree_sitter_javascript::HIGHLIGHT_QUERY
            ),
        ),
        Lang::Go => (tree_sitter_go::LANGUAGE.into(), tree_sitter_go::HIGHLIGHTS_QUERY.into()),
        Lang::C => (tree_sitter_c::LANGUAGE.into(), tree_sitter_c::HIGHLIGHT_QUERY.into()),
        // C++ queries extend C's.
        Lang::Cpp => (
            tree_sitter_cpp::LANGUAGE.into(),
            format!("{}\n{}", tree_sitter_cpp::HIGHLIGHT_QUERY, tree_sitter_c::HIGHLIGHT_QUERY),
        ),
        Lang::Json => (tree_sitter_json::LANGUAGE.into(), tree_sitter_json::HIGHLIGHTS_QUERY.into()),
        Lang::Bash => (tree_sitter_bash::LANGUAGE.into(), tree_sitter_bash::HIGHLIGHT_QUERY.into()),
        Lang::Toml => (tree_sitter_toml_ng::LANGUAGE.into(), tree_sitter_toml_ng::HIGHLIGHTS_QUERY.into()),
        Lang::Yaml => (tree_sitter_yaml::LANGUAGE.into(), tree_sitter_yaml::HIGHLIGHTS_QUERY.into()),
        Lang::Nix => (tree_sitter_nix::LANGUAGE.into(), tree_sitter_nix::HIGHLIGHTS_QUERY.into()),
        Lang::Markdown => (tree_sitter_md::LANGUAGE.into(), tree_sitter_md::HIGHLIGHT_QUERY_BLOCK.into()),
    };
    // A grammar whose bundled query fails to compile is shown unhighlighted rather than
    // failing the file view; the test suite compiles every configuration.
    let mut cfg = HighlightConfiguration::new(language, format!("{lang:?}"), &query, "", "").ok()?;
    cfg.configure(NAMES);
    Some(cfg)
}

fn config(lang: Lang) -> Option<&'static HighlightConfiguration> {
    static CONFIGS: [OnceLock<Option<HighlightConfiguration>>; LANGS] = [const { OnceLock::new() }; LANGS];
    CONFIGS[lang as usize].get_or_init(|| build(lang)).as_ref()
}

/// Display column of every char boundary in `line` (index = raw char index), matching
/// `util::sanitize`: tabs widen to four columns and control characters take none.
fn display_cols(line: &str) -> Vec<usize> {
    let mut cols = Vec::with_capacity(line.len() + 1);
    let mut col = 0;
    for c in line.chars() {
        cols.push(col);
        col += match c {
            '\t' => 4,
            c if c.is_control() => 0,
            _ => 1,
        };
    }
    cols.push(col);
    cols
}

/// Highlights `source` as the language implied by `path`.
///
/// @returns one run list per line of `source` (`str::lines` order), or None when the language
/// is unknown, the file is too large, or parsing fails.
pub fn highlight(path: &Path, source: &str) -> Option<Vec<Vec<Run>>> {
    if source.len() > MAX_BYTES {
        return None;
    }
    let cfg = config(detect(path)?)?;
    let mut highlighter = Highlighter::new();
    let events = highlighter.highlight(cfg, source.as_bytes(), None, None, |_| None).ok()?;

    let lines: Vec<&str> = source.lines().collect();
    let mut starts = Vec::with_capacity(lines.len());
    let mut off = 0;
    for l in source.split_inclusive('\n') {
        starts.push(off);
        off += l.len();
    }
    let cols: Vec<Vec<usize>> = lines.iter().map(|l| display_cols(l)).collect();
    let mut out: Vec<Vec<Run>> = vec![Vec::new(); lines.len()];
    let mut stack: Vec<usize> = Vec::new();

    for ev in events {
        match ev.ok()? {
            HighlightEvent::HighlightStart(h) => stack.push(h.0),
            HighlightEvent::HighlightEnd => {
                stack.pop();
            }
            HighlightEvent::Source { start, end } => {
                let Some(&h) = stack.last() else { continue };
                let style = style_for(NAMES[h]);
                let mut li = starts.partition_point(|&s| s <= start).saturating_sub(1);
                let mut s = start;
                while s < end && li < lines.len() {
                    let line_start = starts[li];
                    let line = lines[li];
                    let line_end = line_start + line.len();
                    let e = end.min(line_end);
                    if s < e {
                        let a = line[..s - line_start].chars().count();
                        let b = a + line[s - line_start..e - line_start].chars().count();
                        let (a, b) = (cols[li][a], cols[li][b]);
                        if a < b {
                            out[li].push(Run { start: a, end: b, style });
                        }
                    }
                    li += 1;
                    s = starts.get(li).copied().unwrap_or(end);
                }
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bundled_grammar_compiles_its_queries() {
        for lang in [
            Lang::Rust,
            Lang::Python,
            Lang::JavaScript,
            Lang::TypeScript,
            Lang::Tsx,
            Lang::Go,
            Lang::C,
            Lang::Cpp,
            Lang::Json,
            Lang::Bash,
            Lang::Toml,
            Lang::Yaml,
            Lang::Nix,
            Lang::Markdown,
        ] {
            assert!(build(lang).is_some(), "{lang:?} highlight query failed to compile");
        }
    }

    #[test]
    fn rust_keywords_strings_and_comments_get_styles_at_display_columns() {
        let src = "fn main() {\n\tlet s = \"hi\"; // note\n}\n";
        let hl = highlight(Path::new("x.rs"), src).unwrap();
        assert_eq!(hl.len(), 3);
        let at = |line: usize, col: usize| hl[line].iter().find(|r| r.start <= col && col < r.end).map(|r| r.style);
        assert_eq!(at(0, 0), Some(style_for("keyword")));
        assert_eq!(at(0, 3), Some(style_for("function")));
        // Line 1 starts with a tab (4 display columns): `let` is at column 4.
        assert_eq!(at(1, 4), Some(style_for("keyword")));
        assert_eq!(at(1, 12), Some(style_for("string")));
        assert_eq!(at(1, 18), Some(style_for("comment")));
    }

    #[test]
    fn unknown_languages_are_not_highlighted() {
        assert!(highlight(Path::new("notes.txt"), "plain").is_none());
        assert_eq!(detect(Path::new("flake.nix")), Some(Lang::Nix));
        assert_eq!(detect(Path::new("dir/Cargo.lock")), Some(Lang::Toml));
    }
}
