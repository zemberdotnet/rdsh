//! Small helpers shared across modules: clocks, session ids, text sanitizing.

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch; 0 if the clock is before the epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Converts days since 1970-01-01 to a (year, month, day) civil date.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = yoe as i64 + era * 400;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Today's UTC date as `YYYY-MM-DD`.
pub fn today() -> String {
    let (y, m, d) = civil_from_days((now_ms() / 86_400_000) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// A sortable, collision-resistant session id: `YYYYMMDD-HHMMSS-xxxx` (UTC).
pub fn new_session_id() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs() as i64;
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let sod = secs.rem_euclid(86_400);
    let salt = (now.subsec_nanos() ^ std::process::id().wrapping_mul(2_654_435_761)) & 0xffff;
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}-{salt:04x}",
        sod / 3600,
        sod % 3600 / 60,
        sod % 60
    )
}

/// Keeps the head and tail of `s` so that at most `max` characters remain, marking the cut.
pub fn truncate_middle(s: String, max: usize) -> String {
    let total = s.chars().count();
    if total <= max {
        return s;
    }
    let head = max * 2 / 3;
    let tail = max - head;
    let head_end = s.char_indices().nth(head).map_or(s.len(), |(i, _)| i);
    let tail_start = s.char_indices().nth(total - tail).map_or(s.len(), |(i, _)| i);
    format!(
        "{}\n… [{} characters omitted] …\n{}",
        &s[..head_end],
        total - head - tail,
        &s[tail_start..]
    )
}

/// Makes text safe for terminal display: drops ANSI escapes, carriage returns and other
/// control characters, and expands tabs to four spaces.
pub fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                // CSI: ESC [ params final-byte; OSC: ESC ] ... BEL|ESC\; others: ESC + one char.
                match chars.peek() {
                    Some('[') => {
                        chars.next();
                        for c in chars.by_ref() {
                            if ('@'..='~').contains(&c) {
                                break;
                            }
                        }
                    }
                    Some(']') => {
                        chars.next();
                        while let Some(c) = chars.next() {
                            if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                                break;
                            }
                        }
                    }
                    Some(_) => {
                        chars.next();
                    }
                    None => {}
                }
            }
            '\t' => out.push_str("    "),
            '\n' => out.push('\n'),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Standard base64 encoding (used for OSC 52 clipboard writes).
pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_399), (2025, 11, 7));
    }

    #[test]
    fn truncation_keeps_head_and_tail() {
        let s: String = "a".repeat(50) + &"b".repeat(50);
        let t = truncate_middle(s, 30);
        assert!(t.starts_with("aaaa"));
        assert!(t.ends_with("bbbb"));
        assert!(t.contains("70 characters omitted"));
        assert_eq!(truncate_middle("short".into(), 30), "short");
    }

    #[test]
    fn sanitize_strips_escapes() {
        assert_eq!(sanitize("\x1b[31mred\x1b[0m\tx\r\n"), "red    x\n");
        assert_eq!(sanitize("\x1b]0;title\x07ok"), "ok");
    }

    #[test]
    fn base64_matches_reference() {
        assert_eq!(base64(b"hello"), "aGVsbG8=");
        assert_eq!(base64(b"hi!"), "aGkh");
        assert_eq!(base64(b"h"), "aA==");
    }
}
