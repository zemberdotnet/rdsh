//! Vim-style cursor motions over a list of display lines.

/// A cursor position: display row and char column.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub row: usize,
    pub col: usize,
}

/// Read access to the rows a cursor moves over.
pub trait Lines {
    fn count(&self) -> usize;
    fn line(&self, row: usize) -> &str;

    fn len_of(&self, row: usize) -> usize {
        self.line(row).chars().count()
    }
}

/// Cursor with a remembered column for vertical moves (`want`; `usize::MAX` = end of line).
#[derive(Clone, Copy, Debug, Default)]
pub struct Cursor {
    pub pos: Pos,
    pub want: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    Down,
    Up,
    Left,
    Right,
    LineStart,
    FirstNonBlank,
    LineEnd,
    WordForward,
    WordBack,
    WordEnd,
    Top,
    Bottom,
    /// Move by a signed number of rows (half pages, pages).
    Rows(isize),
    /// Jump to an absolute row.
    Row(usize),
    NextBlock,
    PrevBlock,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Space,
    Word,
    Punct,
}

fn class(c: char) -> Class {
    if c.is_whitespace() {
        Class::Space
    } else if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

fn chars(lines: &dyn Lines, row: usize) -> Vec<char> {
    lines.line(row).chars().collect()
}

impl Cursor {
    /// Keeps the cursor inside `lines`.
    pub fn clamp(&mut self, lines: &dyn Lines) {
        let n = lines.count();
        if n == 0 {
            self.pos = Pos::default();
            return;
        }
        self.pos.row = self.pos.row.min(n - 1);
        self.pos.col = self.pos.col.min(lines.len_of(self.pos.row).saturating_sub(1));
    }

    fn set_row(&mut self, row: usize, lines: &dyn Lines) {
        let n = lines.count();
        if n == 0 {
            return;
        }
        self.pos.row = row.min(n - 1);
        self.pos.col = self.want.min(lines.len_of(self.pos.row).saturating_sub(1));
    }

    fn set_pos(&mut self, p: Pos) {
        self.pos = p;
        self.want = p.col;
    }

    /// Moves to an exact position (mouse clicks, search hits).
    pub fn jump(&mut self, p: Pos, lines: &dyn Lines) {
        self.set_pos(p);
        self.clamp(lines);
        self.want = self.pos.col;
    }
}

/// Applies `motion` `count` times.
///
/// @param blocks sorted first rows of blocks (messages, hunks) for `NextBlock`/`PrevBlock`.
pub fn apply(c: &mut Cursor, motion: Motion, lines: &dyn Lines, blocks: &[usize], count: usize) {
    let n = lines.count();
    if n == 0 {
        return;
    }
    let count = count.max(1);
    let row = c.pos.row;
    let len = lines.len_of(row);
    match motion {
        Motion::Down => c.set_row(row + count, lines),
        Motion::Up => c.set_row(row.saturating_sub(count), lines),
        Motion::Left => c.set_pos(Pos { row, col: c.pos.col.saturating_sub(count) }),
        Motion::Right => c.set_pos(Pos { row, col: (c.pos.col + count).min(len.saturating_sub(1)) }),
        Motion::LineStart => c.set_pos(Pos { row, col: 0 }),
        Motion::FirstNonBlank => {
            let col = lines.line(row).chars().position(|ch| !ch.is_whitespace()).unwrap_or(0);
            c.set_pos(Pos { row, col })
        }
        Motion::LineEnd => {
            c.pos.col = len.saturating_sub(1);
            c.want = usize::MAX;
        }
        Motion::WordForward => (0..count).for_each(|_| c.set_pos(word_forward(lines, c.pos))),
        Motion::WordBack => (0..count).for_each(|_| c.set_pos(word_back(lines, c.pos))),
        Motion::WordEnd => (0..count).for_each(|_| c.set_pos(word_end(lines, c.pos))),
        Motion::Top => {
            c.want = 0;
            c.set_row(0, lines)
        }
        Motion::Bottom => {
            c.want = 0;
            c.set_row(n - 1, lines)
        }
        Motion::Rows(d) => c.set_row((row as isize + d).max(0) as usize, lines),
        Motion::Row(r) => c.set_row(r, lines),
        Motion::NextBlock => {
            let mut r = row;
            for _ in 0..count {
                r = blocks.iter().copied().find(|&b| b > r).unwrap_or(n - 1);
            }
            c.want = 0;
            c.set_row(r, lines)
        }
        Motion::PrevBlock => {
            let mut r = row;
            for _ in 0..count {
                r = blocks.iter().copied().rfind(|&b| b < r).unwrap_or(0);
            }
            c.want = 0;
            c.set_row(r, lines)
        }
    }
}

fn word_forward(lines: &dyn Lines, p: Pos) -> Pos {
    let n = lines.count();
    let mut r = p.row;
    let mut cs = chars(lines, r);
    let mut i = p.col;
    if i < cs.len() {
        let k = class(cs[i]);
        if k != Class::Space {
            while i < cs.len() && class(cs[i]) == k {
                i += 1;
            }
        }
    }
    loop {
        while i < cs.len() && class(cs[i]) == Class::Space {
            i += 1;
        }
        if i < cs.len() {
            return Pos { row: r, col: i };
        }
        if r + 1 >= n {
            return Pos { row: r, col: cs.len().saturating_sub(1) };
        }
        r += 1;
        cs = chars(lines, r);
        i = 0;
        if cs.is_empty() {
            return Pos { row: r, col: 0 };
        }
    }
}

fn word_back(lines: &dyn Lines, p: Pos) -> Pos {
    let mut r = p.row;
    let mut cs = chars(lines, r);
    let mut i = p.col.min(cs.len());
    loop {
        if i == 0 {
            if r == 0 {
                return Pos::default();
            }
            r -= 1;
            cs = chars(lines, r);
            if cs.is_empty() {
                return Pos { row: r, col: 0 };
            }
            i = cs.len();
        }
        i -= 1;
        if class(cs[i]) != Class::Space {
            break;
        }
    }
    let k = class(cs[i]);
    while i > 0 && class(cs[i - 1]) == k {
        i -= 1;
    }
    Pos { row: r, col: i }
}

fn word_end(lines: &dyn Lines, p: Pos) -> Pos {
    let n = lines.count();
    let mut r = p.row;
    let mut cs = chars(lines, r);
    let mut i = p.col + 1;
    loop {
        if i >= cs.len() {
            if r + 1 >= n {
                return Pos { row: r, col: cs.len().saturating_sub(1) };
            }
            r += 1;
            cs = chars(lines, r);
            i = 0;
            continue;
        }
        if class(cs[i]) != Class::Space {
            break;
        }
        i += 1;
    }
    let k = class(cs[i]);
    while i + 1 < cs.len() && class(cs[i + 1]) == k {
        i += 1;
    }
    Pos { row: r, col: i }
}

/// Finds the next (or previous) occurrence of `needle`, wrapping around.
/// Case-insensitive unless `needle` contains an uppercase letter (smartcase).
pub fn search(lines: &dyn Lines, from: Pos, needle: &str, forward: bool) -> Option<Pos> {
    let n = lines.count();
    if n == 0 || needle.is_empty() {
        return None;
    }
    let smart = needle.chars().any(char::is_uppercase);
    let needle = if smart { needle.to_owned() } else { needle.to_ascii_lowercase() };
    let hits = |row: usize| -> Vec<usize> {
        let line = lines.line(row);
        let hay = if smart { line.to_owned() } else { line.to_ascii_lowercase() };
        hay.match_indices(&needle).map(|(b, _)| line[..b].chars().count()).collect()
    };
    for step in 0..=n {
        let row = if forward { (from.row + step) % n } else { (from.row + n - step % n) % n };
        let cols = hits(row);
        let found = if forward {
            cols.into_iter().find(|&c| step > 0 || c > from.col)
        } else {
            cols.into_iter().rev().find(|&c| step > 0 || c < from.col)
        };
        if let Some(col) = found {
            return Some(Pos { row, col });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    struct V(Vec<&'static str>);
    impl Lines for V {
        fn count(&self) -> usize {
            self.0.len()
        }
        fn line(&self, row: usize) -> &str {
            self.0[row]
        }
    }

    fn at(row: usize, col: usize) -> Cursor {
        Cursor { pos: Pos { row, col }, want: col }
    }

    #[test]
    fn word_motions_cross_lines() {
        let v = V(vec!["foo bar.baz", "", "  qux"]);
        let mut c = at(0, 0);
        apply(&mut c, Motion::WordForward, &v, &[], 1);
        assert_eq!(c.pos, Pos { row: 0, col: 4 });
        apply(&mut c, Motion::WordForward, &v, &[], 2);
        assert_eq!(c.pos, Pos { row: 0, col: 8 });
        apply(&mut c, Motion::WordForward, &v, &[], 1);
        assert_eq!(c.pos, Pos { row: 1, col: 0 });
        apply(&mut c, Motion::WordForward, &v, &[], 1);
        assert_eq!(c.pos, Pos { row: 2, col: 2 });
        apply(&mut c, Motion::WordBack, &v, &[], 1);
        assert_eq!(c.pos, Pos { row: 1, col: 0 });
        apply(&mut c, Motion::WordBack, &v, &[], 1);
        assert_eq!(c.pos, Pos { row: 0, col: 8 });
        let mut c = at(0, 0);
        apply(&mut c, Motion::WordEnd, &v, &[], 1);
        assert_eq!(c.pos, Pos { row: 0, col: 2 });
    }

    #[test]
    fn vertical_moves_remember_column_and_blocks_jump() {
        let v = V(vec!["long line here", "ab", "another long one", "x"]);
        let mut c = at(0, 10);
        apply(&mut c, Motion::Down, &v, &[], 1);
        assert_eq!(c.pos, Pos { row: 1, col: 1 });
        apply(&mut c, Motion::Down, &v, &[], 1);
        assert_eq!(c.pos, Pos { row: 2, col: 10 });
        apply(&mut c, Motion::Bottom, &v, &[], 1);
        assert_eq!(c.pos.row, 3);
        let blocks = [0, 2];
        apply(&mut c, Motion::PrevBlock, &v, &blocks, 1);
        assert_eq!(c.pos.row, 2);
        apply(&mut c, Motion::PrevBlock, &v, &blocks, 1);
        assert_eq!(c.pos.row, 0);
        apply(&mut c, Motion::NextBlock, &v, &blocks, 1);
        assert_eq!(c.pos.row, 2);
        apply(&mut c, Motion::NextBlock, &v, &blocks, 1);
        assert_eq!(c.pos.row, 3);
    }

    #[test]
    fn smartcase_search_wraps() {
        let v = V(vec!["Alpha beta", "gamma ALPHA"]);
        let p = search(&v, Pos { row: 0, col: 0 }, "alpha", true).unwrap();
        assert_eq!(p, Pos { row: 1, col: 6 });
        let p = search(&v, p, "alpha", true).unwrap();
        assert_eq!(p, Pos { row: 0, col: 0 });
        assert!(search(&v, p, "ALPHA", true).is_some_and(|p| p.row == 1));
        assert_eq!(search(&v, Pos { row: 1, col: 6 }, "beta", false), Some(Pos { row: 0, col: 6 }));
        assert_eq!(search(&v, Pos::default(), "zzz", true), None);
    }
}
