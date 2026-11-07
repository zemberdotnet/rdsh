//! The UI palette: gruvbox dark (https://github.com/morhetz/gruvbox).
//!
//! Every color the TUI draws comes from here. The whole frame is painted with `BG`/`FG`, so the
//! look does not depend on the terminal's own color scheme.

use ratatui::style::Color;

const fn hex(rgb: u32) -> Color {
    Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// Window background (gruvbox bg0).
pub const BG: Color = hex(0x282828);
/// Default text (gruvbox fg1).
pub const FG: Color = hex(0xebdbb2);
/// Secondary text: tool output, punctuation (gruvbox fg4).
pub const FG_DIM: Color = hex(0xa89984);
/// Hints, comments, gutters, separators (gruvbox gray).
pub const GRAY: Color = hex(0x928374);
/// Title bars and the user's messages (gruvbox bg1).
pub const PANEL: Color = hex(0x3c3836);
/// Visual selection (gruvbox bg2).
pub const SELECTION: Color = hex(0x504945);
/// Background of the user's own messages.
pub const USER_BG: Color = PANEL;
/// Background tint of added lines in the file view.
pub const ADD_BG: Color = hex(0x34381b);
/// Background tint of deleted lines in the file view.
pub const DEL_BG: Color = hex(0x402120);

pub const RED: Color = hex(0xfb4934);
pub const GREEN: Color = hex(0xb8bb26);
pub const YELLOW: Color = hex(0xfabd2f);
pub const BLUE: Color = hex(0x83a598);
pub const PURPLE: Color = hex(0xd3869b);
pub const AQUA: Color = hex(0x8ec07c);
pub const ORANGE: Color = hex(0xfe8019);
