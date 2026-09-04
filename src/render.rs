//! Turns a highlighted line (plus optional suggestion and completion list)
//! into the terminal byte sequence that paints it over what readline drew.
//!
//! Invariants that keep readline's idea of the cursor position intact:
//! * the frame starts with DECSC (`ESC 7`) and ends with DECRC (`ESC 8`);
//! * every cursor movement is relative (rows) or absolute-in-row (columns)
//!   and never scrolls: rows are entered with `CR` + `CUD`, never with LF;
//! * the frame never writes below the rows the caller says are available.

use crate::layout::{Disp, Layout};
use crate::lexer::{Kind, Span};
use crate::style::{push_num, Style, Theme, Ui};
use unicode_width::UnicodeWidthChar;

/// One entry of the completion list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListEntry {
    pub text: String,
    pub is_dir: bool,
    pub desc: String,
}

/// A group of entries with a header ("commands", "files", "history").
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListGroup {
    pub header: String,
    /// Length (in chars) of the prefix that entries match (drawn bold).
    pub match_len: usize,
    pub entries: Vec<ListEntry>,
}

pub struct Frame<'a> {
    pub theme: &'a Theme,
    pub text: &'a str,
    pub spans: &'a [Span],
    pub layout: &'a Layout,
    /// Ghost text drawn after the buffer (history suggestion).
    pub suggestion: &'a str,
    pub groups: &'a [ListGroup],
    /// Index (global across groups) of the selected list entry, if any.
    pub selected: Option<usize>,
    /// Rows below the block that we may draw on.
    pub avail_rows: usize,
    /// Whether a list/suggestion was drawn last time (so we must clear).
    pub clear_below: bool,
}

struct Cursor {
    row: usize,
    col: usize,
    style: Option<Style>,
}

impl Cursor {
    fn goto(&mut self, out: &mut Vec<u8>, row: usize, col: usize) {
        if row > self.row {
            out.push(b'\r');
            out.extend_from_slice(b"\x1b[");
            push_num(out, row - self.row);
            out.push(b'B');
            self.col = 0;
        } else if row < self.row {
            out.extend_from_slice(b"\x1b[");
            push_num(out, self.row - row);
            out.push(b'A');
        }
        self.row = row;
        if col != self.col {
            if col == 0 {
                out.push(b'\r');
            } else {
                out.extend_from_slice(b"\x1b[");
                push_num(out, col + 1);
                out.push(b'G');
            }
            self.col = col;
        }
    }

    fn set_style(&mut self, out: &mut Vec<u8>, st: Style) {
        if self.style != Some(st) {
            st.write_sgr(out);
            self.style = Some(st);
        }
    }
}

/// Render the frame. Returns whether anything was drawn below the block
/// (so the caller knows a later frame must clear it).
pub fn render(f: &Frame, out: &mut Vec<u8>) -> bool {
    let lay = f.layout;
    let mut cur = Cursor { row: lay.point_row, col: lay.point_col, style: None };
    out.extend_from_slice(b"\x1b7");

    // --- highlighted text ---------------------------------------------------
    let mut si = 0usize;
    let width = lay.width;
    for it in &lay.items {
        if it.width == 0 && it.disp != Disp::Char {
            continue;
        }
        // style for this byte
        while si < f.spans.len() && f.spans[si].end <= it.start && f.spans[si].end < f.text.len() {
            si += 1;
        }
        let kind = f
            .spans
            .get(si)
            .filter(|s| s.start <= it.start && it.start < s.end)
            .map(|s| s.kind)
            .unwrap_or(Kind::Default);
        let st = f.theme.style(kind);
        // Unstyled cells are skipped: readline already drew them plain and
        // the cursor is repositioned lazily by goto() before the next draw.
        if st.is_none() {
            continue;
        }
        cur.goto(out, it.row, it.col);
        cur.set_style(out, st);
        match it.disp {
            Disp::Char => out.extend_from_slice(&f.text.as_bytes()[it.start..it.end]),
            Disp::Spaces(n) => out.extend(std::iter::repeat(b' ').take(n as usize)),
            Disp::Lit(b) => out.push(b),
            Disp::Nothing => {}
        }
        cur.col += it.width;
        if cur.col >= width {
            // the terminal is now in "pending wrap" state; force a known
            // position next time by resetting our notion of the column
            cur.col = usize::MAX;
        }
    }
    if cur.style.is_some() {
        out.extend_from_slice(b"\x1b[0m");
        cur.style = None;
    }
    let mut drew_below = false;

    // --- suggestion ----------------------------------------------------------
    let mut below_row = lay.end_row; // last row used by text or suggestion
    let mut below_col = lay.end_col;
    let mut needs_clear = f.clear_below;
    if !f.suggestion.is_empty() {
        let st = f.theme.ui(Ui::Suggestion);
        cur.goto(out, lay.end_row, lay.end_col);
        cur.set_style(out, st);
        let max_row = lay.end_row + f.avail_rows;
        let mut row = lay.end_row;
        let mut col = lay.end_col;
        for c in f.suggestion.chars() {
            if c == '\n' {
                break;
            }
            let w = if (c as u32) < 0x20 { 2 } else { c.width().unwrap_or(1) };
            if col + w > width {
                if row + 1 > max_row {
                    break;
                }
                row += 1;
                col = 0;
                cur.goto(out, row, 0);
                cur.set_style(out, st);
                drew_below = true;
            }
            if (c as u32) < 0x20 {
                out.push(b'^');
                out.push((c as u8) ^ 0x40);
            } else {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            col += w;
            cur.col = col;
            if col >= width {
                if row + 1 > max_row {
                    break;
                }
                row += 1;
                col = 0;
                cur.goto(out, row, 0);
                cur.set_style(out, st);
                drew_below = true;
            }
        }
        out.extend_from_slice(b"\x1b[0m");
        cur.style = None;
        below_row = row;
        below_col = col;
        needs_clear = true;
    }

    // --- completion list -------------------------------------------------------
    let total: usize = f.groups.iter().map(|g| g.entries.len()).sum();
    let last_row = lay.end_row + f.avail_rows;
    // Where an erase-to-end-of-screen can start without eating a cell we drew.
    let clear_pos = if below_col < width {
        Some((below_row, below_col))
    } else if below_row + 1 <= last_row {
        Some((below_row + 1, 0))
    } else {
        None
    };
    let list_rows_avail = last_row.saturating_sub(below_row);
    if total > 0 && list_rows_avail > 0 {
        if let Some((r, c)) = clear_pos {
            cur.goto(out, r, c);
            out.extend_from_slice(b"\x1b[J");
        }
        let rows = render_list(f, out, &mut cur, below_row + 1, list_rows_avail);
        if rows > 0 {
            drew_below = true;
        }
    } else if needs_clear {
        if let Some((r, c)) = clear_pos {
            cur.goto(out, r, c);
            out.extend_from_slice(b"\x1b[J");
        }
    }

    out.extend_from_slice(b"\x1b[0m\x1b8");
    drew_below
}

/// Lay out the list entries in columns and draw up to `max_rows` rows
/// starting at `first_row`. Returns the number of rows drawn.
fn render_list(f: &Frame, out: &mut Vec<u8>, cur: &mut Cursor, first_row: usize, max_rows: usize) -> usize {
    let width = f.layout.width;
    let mut rows_used = 0usize;
    let mut global_index = 0usize;
    let sel_style = f.theme.ui(Ui::ListSelected);
    let hdr_style = f.theme.ui(Ui::ListHeader);
    let match_style = f.theme.ui(Ui::ListMatch);
    let dir_style = f.theme.ui(Ui::ListDir);
    let desc_style = f.theme.ui(Ui::ListDesc);

    for g in f.groups {
        if g.entries.is_empty() {
            continue;
        }
        if rows_used >= max_rows {
            break;
        }
        let has_desc = g.entries.iter().any(|e| !e.desc.is_empty());
        // column geometry
        let maxw = g.entries.iter().map(|e| display_width(&e.text)).max().unwrap_or(1).max(1);
        let colw = (maxw + 2).min(width);
        let ncols = if has_desc { 1 } else { (width / colw).max(1) };
        let nrows = (g.entries.len() + ncols - 1) / ncols;
        // header
        let header_rows = if g.header.is_empty() { 0 } else { 1 };
        let body_rows_avail = max_rows - rows_used - header_rows;
        if body_rows_avail == 0 {
            break;
        }
        // which rows to show: keep the selected entry visible
        let sel_row = f
            .selected
            .filter(|&s| s >= global_index && s < global_index + g.entries.len())
            .map(|s| (s - global_index) % nrows);
        let show_rows = nrows.min(body_rows_avail);
        let first_shown = match sel_row {
            Some(r) if r >= show_rows => r + 1 - show_rows,
            _ => 0,
        };
        if header_rows == 1 {
            cur.goto(out, first_row + rows_used, 0);
            cur.set_style(out, hdr_style);
            let mut hdr: String = g.header.chars().take(width.saturating_sub(1)).collect();
            if nrows > show_rows {
                hdr.push_str(&format!(" ({} more)", g.entries.len() - show_rows * ncols.min(g.entries.len())));
            }
            let hdr: String = hdr.chars().take(width.saturating_sub(1)).collect();
            out.extend_from_slice(hdr.as_bytes());
            out.extend_from_slice(b"\x1b[0m\x1b[K");
            cur.style = None;
            cur.col = usize::MAX;
            rows_used += 1;
        }
        for r in first_shown..first_shown + show_rows {
            cur.goto(out, first_row + rows_used, 0);
            let mut col = 0usize;
            for c in 0..ncols {
                let idx = c * nrows + r;
                if idx >= g.entries.len() {
                    break;
                }
                let e = &g.entries[idx];
                let is_sel = f.selected == Some(global_index + idx);
                let base = if e.is_dir { dir_style } else { Style::NONE };
                if col > 0 {
                    // move to the column start (pads with spaces implicitly
                    // because we cleared the row below)
                    cur.goto(out, first_row + rows_used, col);
                }
                let mut written = 0usize;
                let avail = width.saturating_sub(col + 1);
                if is_sel {
                    cur.set_style(out, sel_style);
                }
                for (ci, ch) in e.text.chars().enumerate() {
                    let w = ch.width().unwrap_or(1);
                    if written + w > avail {
                        break;
                    }
                    if !is_sel {
                        let st = if ci < g.match_len { merge(base, match_style) } else { base };
                        cur.set_style(out, st);
                    }
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                    written += w;
                }
                if has_desc && !e.desc.is_empty() && written + 3 < avail {
                    cur.set_style(out, if is_sel { sel_style } else { desc_style });
                    let pad = (maxw + 2).saturating_sub(written);
                    out.extend(std::iter::repeat(b' ').take(pad));
                    written += pad;
                    for ch in e.desc.chars() {
                        let w = ch.width().unwrap_or(1);
                        if written + w > avail {
                            break;
                        }
                        let mut buf = [0u8; 4];
                        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        written += w;
                    }
                }
                out.extend_from_slice(b"\x1b[0m");
                cur.style = None;
                col += colw;
                cur.col = usize::MAX;
                if col >= width {
                    break;
                }
            }
            out.extend_from_slice(b"\x1b[K");
            rows_used += 1;
            if rows_used >= max_rows {
                break;
            }
        }
        global_index += g.entries.len();
    }
    rows_used
}

fn merge(a: Style, b: Style) -> Style {
    Style {
        fg: if b.fg != crate::style::Color::None { b.fg } else { a.fg },
        bg: if b.bg != crate::style::Color::None { b.bg } else { a.bg },
        bold: a.bold || b.bold,
        dim: a.dim || b.dim,
        italic: a.italic || b.italic,
        underline: a.underline || b.underline,
        reverse: a.reverse || b.reverse,
        strike: a.strike || b.strike,
    }
}

pub fn display_width(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(1)).sum()
}

/// Render `text` with inline ANSI colors (for `bash-tools highlight`).
pub fn render_inline(text: &str, spans: &[Span], theme: &Theme) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() * 2);
    for sp in spans {
        let st = theme.style(sp.kind);
        if st.is_none() {
            out.extend_from_slice(&text.as_bytes()[sp.start..sp.end]);
        } else {
            st.write_sgr(&mut out);
            out.extend_from_slice(&text.as_bytes()[sp.start..sp.end]);
            out.extend_from_slice(b"\x1b[0m");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::layout;
    use crate::lexer::{lex, NoLookup};

    #[test]
    fn frame_roundtrip() {
        let text = "ls -la";
        let spans = lex(text, &NoLookup);
        let lay = layout(text, 6, 2, 80);
        let theme = Theme::default();
        let f = Frame {
            theme: &theme,
            text,
            spans: &spans,
            layout: &lay,
            suggestion: "",
            groups: &[],
            selected: None,
            avail_rows: 5,
            clear_below: false,
        };
        let mut out = Vec::new();
        let below = render(&f, &mut out);
        assert!(!below);
        let s = String::from_utf8_lossy(&out);
        assert!(s.starts_with("\x1b7"));
        assert!(s.ends_with("\x1b[0m\x1b8"));
        // "ls" unknown → red bold at column 3
        assert!(s.contains("\x1b[3G\x1b[0;1;31mls"));
        // "-la" option cyan
        assert!(s.contains("\x1b[0;36m-la"));
    }

    #[test]
    fn list_and_suggestion() {
        let text = "ec";
        let spans = lex(text, &NoLookup);
        let lay = layout(text, 2, 2, 40);
        let theme = Theme::default();
        let groups = vec![ListGroup {
            header: "commands".into(),
            match_len: 2,
            entries: vec![
                ListEntry { text: "echo".into(), is_dir: false, desc: String::new() },
                ListEntry { text: "ecryptfs".into(), is_dir: false, desc: String::new() },
            ],
        }];
        let f = Frame {
            theme: &theme,
            text,
            spans: &spans,
            layout: &lay,
            suggestion: "ho hi",
            groups: &groups,
            selected: Some(1),
            avail_rows: 4,
            clear_below: false,
        };
        let mut out = Vec::new();
        let below = render(&f, &mut out);
        assert!(below);
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("ho hi"));
        assert!(s.contains("commands"));
        assert!(s.contains("\x1b[0;7mecryptfs"));
        // list rows entered with CR + CUD, never LF
        assert!(!s.contains('\n'));
        assert!(s.contains("\r\x1b[1B"));
    }
}
