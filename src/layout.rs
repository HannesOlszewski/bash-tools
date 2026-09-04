//! A model of how readline lays out the prompt's last line plus the edit
//! buffer on the terminal. This mirrors the rules in readline's `display.c`
//! (`rl_redisplay`): tabs expand to the next multiple of eight columns
//! (relative to the screen column), control characters show as `^X`, wide
//! characters that don't fit at the end of a row are pushed to the next row
//! and the gap is padded with spaces, and a newline in the buffer is a hard
//! row break.
//!
//! Row numbers are relative to the first screen row of the prompt's last
//! line ("row 0" of the block readline manages).

use unicode_width::UnicodeWidthChar;

/// What to draw for one buffer character.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Disp {
    /// A printable character (draw the source bytes).
    Char,
    /// `n` spaces (tab expansion or wide-char padding).
    Spaces(u8),
    /// A single literal ASCII byte (used for the `^X` control-char display).
    Lit(u8),
    /// Nothing is drawn (newline / zero-width char).
    Nothing,
}

/// Placement of one (part of a) buffer character.
#[derive(Clone, Copy, Debug)]
pub struct Item {
    /// Byte range in the source text.
    pub start: usize,
    pub end: usize,
    pub row: usize,
    pub col: usize,
    pub width: usize,
    pub disp: Disp,
}

#[derive(Clone, Debug)]
pub struct Layout {
    pub width: usize,
    pub items: Vec<Item>,
    /// Number of rows occupied (including a trailing empty row when the text
    /// ends exactly at a row boundary, which is how readline counts).
    pub rows: usize,
    /// Position right after the last character.
    pub end_row: usize,
    pub end_col: usize,
    pub point_row: usize,
    pub point_col: usize,
    /// Column where the text starts on row `text_row` (after the prompt).
    pub text_row: usize,
    pub text_col: usize,
}

/// Visible width of a prompt string as readline computes it: characters
/// between `\x01` and `\x02` are invisible; everything else counts with its
/// wcwidth (control characters count as 1, like readline's `_rl_col_width`).
pub fn prompt_width(prompt: &str) -> usize {
    let mut w = 0;
    let mut invisible = false;
    for c in prompt.chars() {
        match c {
            '\x01' => invisible = true,
            '\x02' => invisible = false,
            _ if invisible => {}
            '\r' => {}
            _ => {
                w += match c.width() {
                    Some(n) => n,
                    None => 1,
                }
            }
        }
    }
    w
}

/// Split a prompt into (prefix lines, last line).
pub fn split_prompt(prompt: &str) -> (&str, &str) {
    match prompt.rfind('\n') {
        Some(i) => (&prompt[..i], &prompt[i + 1..]),
        None => ("", prompt),
    }
}

/// Number of screen rows the lines *before* the last prompt line occupy.
pub fn prompt_prefix_rows(prompt: &str, width: usize) -> usize {
    let (prefix, _) = split_prompt(prompt);
    if prefix.is_empty() && !prompt.contains('\n') {
        return 0;
    }
    let w = width.max(1);
    prefix
        .split('\n')
        .map(|line| {
            let pw = prompt_width(line);
            if pw == 0 {
                1
            } else {
                (pw + w - 1) / w
            }
        })
        .sum()
}

/// Compute the layout of `text` when it starts after a prompt of visible
/// width `start_col` on a terminal `width` columns wide. `point` is a
/// character (not byte) offset, as bash reports `READLINE_POINT`.
pub fn layout(text: &str, point: usize, start_col: usize, width: usize) -> Layout {
    let width = width.max(2);
    let mut items: Vec<Item> = Vec::with_capacity(text.len() + 4);
    let mut row = 0usize;
    let mut lpos = start_col;
    while lpos >= width {
        row += 1;
        lpos -= width;
    }
    let text_row = row;
    let text_col = lpos;
    let mut point_pos: Option<(usize, usize)> = None;
    let mut ci = 0usize; // char index
    let bytes = text.as_bytes();
    let mut i = 0usize;

    // CHECK_LPOS equivalent
    macro_rules! advance {
        ($n:expr) => {{
            for _ in 0..$n {
                lpos += 1;
                if lpos >= width {
                    row += 1;
                    lpos = 0;
                }
            }
        }};
    }

    while i < bytes.len() {
        let c = text[i..].chars().next().unwrap();
        let clen = c.len_utf8();
        if ci == point {
            point_pos = Some((row, lpos));
        }
        if c == '\n' {
            items.push(Item { start: i, end: i + clen, row, col: lpos, width: 0, disp: Disp::Nothing });
            row += 1;
            lpos = 0;
        } else if c == '\t' {
            let temp = 8 - lpos % 8;
            if lpos + temp >= width {
                let first = width - lpos;
                items.push(Item { start: i, end: i + clen, row, col: lpos, width: first, disp: Disp::Spaces(first as u8) });
                row += 1;
                let rest = temp - first;
                lpos = rest;
                if rest > 0 {
                    items.push(Item { start: i, end: i + clen, row, col: 0, width: rest, disp: Disp::Spaces(rest as u8) });
                }
            } else {
                items.push(Item { start: i, end: i + clen, row, col: lpos, width: temp, disp: Disp::Spaces(temp as u8) });
                lpos += temp;
            }
        } else if (c as u32) < 0x20 || c == '\x7f' {
            let shown = if c == '\x7f' { b'?' } else { (c as u8) ^ 0x40 };
            // `^` and the letter are placed one cell at a time (may split rows)
            for b in [b'^', shown] {
                items.push(Item { start: i, end: i + clen, row, col: lpos, width: 1, disp: Disp::Lit(b) });
                advance!(1);
            }
        } else {
            let w = c.width().unwrap_or(1);
            if w == 0 {
                items.push(Item { start: i, end: i + clen, row, col: lpos, width: 0, disp: Disp::Nothing });
            } else {
                if lpos + w > width {
                    // pad to end of row, char goes to next row
                    let pad = width - lpos;
                    items.push(Item { start: i, end: i, row, col: lpos, width: pad, disp: Disp::Spaces(pad as u8) });
                    row += 1;
                    lpos = 0;
                    if ci == point {
                        point_pos = Some((row, lpos));
                    }
                }
                items.push(Item { start: i, end: i + clen, row, col: lpos, width: w, disp: Disp::Char });
                advance!(w);
            }
        }
        i += clen;
        ci += 1;
    }
    let (point_row, point_col) = point_pos.unwrap_or((row, lpos));
    Layout {
        width,
        items,
        rows: row + 1,
        end_row: row,
        end_col: lpos,
        point_row,
        point_col,
        text_row,
        text_col,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple() {
        let l = layout("ls -la", 6, 2, 80);
        assert_eq!(l.rows, 1);
        assert_eq!((l.point_row, l.point_col), (0, 8));
        assert_eq!((l.end_row, l.end_col), (0, 8));
        assert_eq!(l.items.len(), 6);
        assert_eq!(l.items[0].col, 2);
        let l = layout("ls -la", 2, 2, 80);
        assert_eq!((l.point_row, l.point_col), (0, 4));
    }

    #[test]
    fn wrapping() {
        // width 10, prompt 3 → 7 cells on row 0
        let l = layout("abcdefghij", 10, 3, 10);
        assert_eq!(l.rows, 2);
        assert_eq!(l.items[7].row, 1);
        assert_eq!(l.items[7].col, 0);
        assert_eq!((l.end_row, l.end_col), (1, 3));
        // text exactly fills the row → trailing empty row like readline
        let l = layout("abcdefg", 7, 3, 10);
        assert_eq!(l.rows, 2);
        assert_eq!((l.end_row, l.end_col), (1, 0));
        assert_eq!((l.point_row, l.point_col), (1, 0));
        // prompt wider than the screen
        let l = layout("x", 0, 25, 10);
        assert_eq!(l.text_row, 2);
        assert_eq!(l.text_col, 5);
        assert_eq!(l.items[0].row, 2);
    }

    #[test]
    fn tabs_and_controls() {
        let l = layout("a\tb", 3, 0, 80);
        assert_eq!(l.items[1].disp, Disp::Spaces(7));
        assert_eq!(l.items[2].col, 8);
        let l = layout("\x01x", 2, 0, 80);
        assert_eq!(l.items[0].disp, Disp::Lit(b'^'));
        assert_eq!(l.items[1].disp, Disp::Lit(b'A'));
        assert_eq!(l.items[2].col, 2);
        assert_eq!((l.point_row, l.point_col), (0, 3));
        // tab across a row boundary
        let l = layout("\t", 1, 5, 8);
        assert_eq!(l.items[0].disp, Disp::Spaces(3));
        assert_eq!(l.rows, 2);
        assert_eq!((l.end_row, l.end_col), (1, 0));
    }

    #[test]
    fn wide_and_newline() {
        let l = layout("a漢b", 3, 0, 80);
        assert_eq!(l.items[1].width, 2);
        assert_eq!(l.items[2].col, 3);
        // wide char that does not fit → padded
        let l = layout("ab漢", 3, 0, 3);
        assert_eq!(l.items[2].disp, Disp::Spaces(1));
        assert_eq!(l.items[3].row, 1);
        assert_eq!(l.items[3].col, 0);
        let l = layout("ab\ncd", 4, 2, 80);
        assert_eq!(l.rows, 2);
        assert_eq!(l.items[3].row, 1);
        assert_eq!(l.items[3].col, 0);
        assert_eq!((l.point_row, l.point_col), (1, 1));
        // combining char has zero width
        let l = layout("e\u{301}x", 3, 0, 80);
        assert_eq!(l.items[2].col, 1);
    }

    #[test]
    fn prompts() {
        assert_eq!(prompt_width("$ "), 2);
        assert_eq!(prompt_width("\x01\x1b[32m\x02user\x01\x1b[0m\x02$ "), 6);
        assert_eq!(prompt_width("漢字> "), 6);
        assert_eq!(split_prompt("a\nb\nc$ "), ("a\nb", "c$ "));
        assert_eq!(prompt_prefix_rows("a\nb\nc$ ", 80), 2);
        assert_eq!(prompt_prefix_rows("c$ ", 80), 0);
        assert_eq!(prompt_prefix_rows("aaaaaaaaaaaa\n$ ", 10), 2);
        assert_eq!(prompt_prefix_rows("\n$ ", 10), 1);
    }
}
