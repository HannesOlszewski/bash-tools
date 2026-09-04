//! Styles (SGR attributes) and the theme mapping token kinds to styles.
//!
//! The configuration syntax mirrors `ZSH_HIGHLIGHT_STYLES`:
//! `command=fg=green,bold;unknown-command=fg=red,bold;path=underline`.

use crate::lexer::Kind;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Color {
    #[default]
    None,
    /// 0..=15: the classic ANSI colors (8..=15 are the bright variants).
    Ansi(u8),
    /// 256-color index.
    Idx(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
    pub strike: bool,
}

impl Style {
    pub const NONE: Style = Style {
        fg: Color::None,
        bg: Color::None,
        bold: false,
        dim: false,
        italic: false,
        underline: false,
        reverse: false,
        strike: false,
    };

    pub fn is_none(&self) -> bool {
        *self == Style::NONE
    }

    /// Parse `fg=green,bold,underline` style specs. Unknown words are ignored.
    pub fn parse(spec: &str) -> Style {
        let mut st = Style::NONE;
        for part in spec.split(',') {
            let part = part.trim();
            if let Some(v) = part.strip_prefix("fg=") {
                st.fg = parse_color(v);
            } else if let Some(v) = part.strip_prefix("bg=") {
                st.bg = parse_color(v);
            } else {
                match part {
                    "bold" => st.bold = true,
                    "dim" | "faint" => st.dim = true,
                    "italic" => st.italic = true,
                    "underline" => st.underline = true,
                    "standout" | "reverse" => st.reverse = true,
                    "strike" | "strikethrough" => st.strike = true,
                    "none" => st = Style::NONE,
                    _ => {}
                }
            }
        }
        st
    }

    /// Append the SGR sequence selecting this style (always starts from a
    /// reset so that no attribute from a previous style leaks).
    pub fn write_sgr(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"\x1b[0");
        if self.bold {
            out.extend_from_slice(b";1");
        }
        if self.dim {
            out.extend_from_slice(b";2");
        }
        if self.italic {
            out.extend_from_slice(b";3");
        }
        if self.underline {
            out.extend_from_slice(b";4");
        }
        if self.reverse {
            out.extend_from_slice(b";7");
        }
        if self.strike {
            out.extend_from_slice(b";9");
        }
        write_color(out, self.fg, false);
        write_color(out, self.bg, true);
        out.push(b'm');
    }
}

fn write_color(out: &mut Vec<u8>, c: Color, bg: bool) {
    match c {
        Color::None => {}
        Color::Ansi(n) if n < 8 => {
            out.push(b';');
            push_num(out, if bg { 40 + n as usize } else { 30 + n as usize });
        }
        Color::Ansi(n) => {
            out.push(b';');
            push_num(out, if bg { 100 + (n - 8) as usize } else { 90 + (n - 8) as usize });
        }
        Color::Idx(n) => {
            out.extend_from_slice(if bg { b";48;5;" } else { b";38;5;" });
            push_num(out, n as usize);
        }
        Color::Rgb(r, g, b) => {
            out.extend_from_slice(if bg { b";48;2;" } else { b";38;2;" });
            push_num(out, r as usize);
            out.push(b';');
            push_num(out, g as usize);
            out.push(b';');
            push_num(out, b as usize);
        }
    }
}

pub fn push_num(out: &mut Vec<u8>, mut n: usize) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    if n == 0 {
        out.push(b'0');
        return;
    }
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    out.extend_from_slice(&buf[i..]);
}

fn parse_color(v: &str) -> Color {
    let v = v.trim();
    let named = match v {
        "black" => Some(0),
        "red" => Some(1),
        "green" => Some(2),
        "yellow" => Some(3),
        "blue" => Some(4),
        "magenta" => Some(5),
        "cyan" => Some(6),
        "white" => Some(7),
        "default" | "none" => return Color::None,
        _ => None,
    };
    if let Some(n) = named {
        return Color::Ansi(n);
    }
    if let Some(rest) = v.strip_prefix("bright") {
        if let Color::Ansi(n) = parse_color(rest.trim_start_matches('-')) {
            return Color::Ansi(n + 8);
        }
    }
    if let Some(hex) = v.strip_prefix('#') {
        if hex.len() == 6 {
            if let Ok(n) = u32::from_str_radix(hex, 16) {
                return Color::Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8);
            }
        }
    }
    if let Ok(n) = v.parse::<u16>() {
        if n < 16 {
            return Color::Ansi(n as u8);
        }
        if n < 256 {
            return Color::Idx(n as u8);
        }
    }
    Color::None
}

/// Extra UI elements that are not token kinds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ui {
    /// The selected entry in the completion menu.
    ListSelected,
    /// Group headers in the completion list.
    ListHeader,
    /// The part of a list entry that matches what was typed.
    ListMatch,
    /// Ghost text of the history suggestion.
    Suggestion,
    /// Directory entries in the completion list.
    ListDir,
    /// Description text next to a list entry.
    ListDesc,
}

impl Ui {
    pub fn name(self) -> &'static str {
        match self {
            Ui::ListSelected => "list-selected",
            Ui::ListHeader => "list-header",
            Ui::ListMatch => "list-match",
            Ui::Suggestion => "suggestion",
            Ui::ListDir => "list-directory",
            Ui::ListDesc => "list-description",
        }
    }
    const ALL: [Ui; 6] = [Ui::ListSelected, Ui::ListHeader, Ui::ListMatch, Ui::Suggestion, Ui::ListDir, Ui::ListDesc];
}

const KIND_ALL: [Kind; 35] = [
    Kind::Default,
    Kind::Command,
    Kind::UnknownCommand,
    Kind::Builtin,
    Kind::Alias,
    Kind::Function,
    Kind::Keyword,
    Kind::Precommand,
    Kind::FuncDef,
    Kind::Argument,
    Kind::Option,
    Kind::Path,
    Kind::Assignment,
    Kind::SingleQuoted,
    Kind::DoubleQuoted,
    Kind::DollarQuoted,
    Kind::Variable,
    Kind::VariableDq,
    Kind::Escape,
    Kind::EscapeDq,
    Kind::CmdSubst,
    Kind::ArithDelim,
    Kind::Arith,
    Kind::ProcSubst,
    Kind::Redirect,
    Kind::Separator,
    Kind::Comment,
    Kind::Glob,
    Kind::Brace,
    Kind::HistoryExp,
    Kind::HeredocDelim,
    Kind::HeredocBody,
    Kind::Tilde,
    Kind::BracketError,
    Kind::MatchingBracket,
];

fn kind_index(k: Kind) -> usize {
    match k {
        Kind::Bracket(_) => 0, // handled separately
        _ => KIND_ALL.iter().position(|&x| x == k).unwrap_or(0),
    }
}

#[derive(Clone, Debug)]
pub struct Theme {
    kinds: [Style; KIND_ALL.len()],
    brackets: [Style; 5],
    ui: [Style; Ui::ALL.len()],
}

impl Default for Theme {
    fn default() -> Self {
        let mut t = Theme {
            kinds: [Style::NONE; KIND_ALL.len()],
            brackets: [Style::NONE; 5],
            ui: [Style::NONE; Ui::ALL.len()],
        };
        let defaults: &[(&str, &str)] = &[
            ("unknown-command", "fg=red,bold"),
            ("reserved-word", "fg=yellow"),
            ("alias", "fg=green"),
            ("builtin", "fg=green"),
            ("function", "fg=green"),
            ("command", "fg=green"),
            ("precommand", "fg=green,underline"),
            ("function-definition", "fg=green,bold"),
            ("option", "fg=cyan"),
            ("path", "underline"),
            ("single-quoted", "fg=yellow"),
            ("double-quoted", "fg=yellow"),
            ("dollar-quoted", "fg=yellow"),
            ("variable", "fg=cyan"),
            ("variable-in-quotes", "fg=cyan"),
            ("escape", "fg=cyan"),
            ("escape-in-quotes", "fg=cyan"),
            ("command-substitution", "fg=magenta"),
            ("arithmetic-delimiter", "fg=magenta"),
            ("process-substitution", "fg=magenta"),
            ("redirection", "fg=magenta"),
            ("comment", "fg=8"),
            ("globbing", "fg=blue,bold"),
            ("brace-expansion", "fg=blue,bold"),
            ("history-expansion", "fg=blue,bold"),
            ("heredoc-delimiter", "fg=yellow"),
            ("heredoc-body", "fg=yellow"),
            ("bracket-error", "fg=red,bold"),
            ("cursor-matchingbracket", "standout"),
            ("bracket-level-1", "fg=blue,bold"),
            ("bracket-level-2", "fg=green,bold"),
            ("bracket-level-3", "fg=magenta,bold"),
            ("bracket-level-4", "fg=yellow,bold"),
            ("bracket-level-5", "fg=cyan,bold"),
            ("list-selected", "standout"),
            ("list-header", "fg=8"),
            ("list-match", "bold"),
            ("suggestion", "fg=8"),
            ("list-directory", "fg=blue"),
            ("list-description", "fg=8"),
        ];
        for (k, v) in defaults {
            t.set(k, v);
        }
        t
    }
}

impl Theme {
    /// Apply a `name=style;name=style` override string.
    pub fn apply(&mut self, spec: &str) {
        for item in spec.split(|c| c == ';' || c == '\n') {
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            if let Some((name, style)) = item.split_once('=') {
                self.set(name.trim(), style.trim());
            }
        }
    }

    fn set(&mut self, name: &str, spec: &str) -> bool {
        let st = Style::parse(spec);
        if let Some(rest) = name.strip_prefix("bracket-level-") {
            if let Ok(n) = rest.parse::<usize>() {
                if (1..=5).contains(&n) {
                    self.brackets[n - 1] = st;
                    return true;
                }
            }
            return false;
        }
        if let Some(i) = KIND_ALL.iter().position(|k| k.name() == name) {
            self.kinds[i] = st;
            return true;
        }
        if let Some(i) = Ui::ALL.iter().position(|u| u.name() == name) {
            self.ui[i] = st;
            return true;
        }
        false
    }

    pub fn style(&self, k: Kind) -> Style {
        match k {
            Kind::Bracket(level) => self.brackets[(level as usize) % 5],
            _ => self.kinds[kind_index(k)],
        }
    }

    pub fn ui(&self, u: Ui) -> Style {
        self.ui[Ui::ALL.iter().position(|&x| x == u).unwrap()]
    }

    /// All configurable names, for `bash-tools styles`.
    pub fn names() -> Vec<&'static str> {
        let mut v: Vec<&'static str> = KIND_ALL.iter().map(|k| k.name()).collect();
        v.extend(["bracket-level-1", "bracket-level-2", "bracket-level-3", "bracket-level-4", "bracket-level-5"]);
        v.extend(Ui::ALL.iter().map(|u| u.name()));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_styles() {
        let s = Style::parse("fg=green,bold");
        assert_eq!(s.fg, Color::Ansi(2));
        assert!(s.bold);
        let mut out = Vec::new();
        s.write_sgr(&mut out);
        assert_eq!(out, b"\x1b[0;1;32m");
        let s = Style::parse("fg=#ff0000,bg=12,underline");
        assert_eq!(s.fg, Color::Rgb(255, 0, 0));
        assert_eq!(s.bg, Color::Ansi(12));
        let mut out = Vec::new();
        s.write_sgr(&mut out);
        assert_eq!(out, b"\x1b[0;4;38;2;255;0;0;104m");
        assert_eq!(Style::parse("fg=200").fg, Color::Idx(200));
        assert_eq!(Style::parse("fg=brightred").fg, Color::Ansi(9));
    }

    #[test]
    fn theme_overrides() {
        let mut t = Theme::default();
        assert_eq!(t.style(Kind::Command).fg, Color::Ansi(2));
        t.apply("command=fg=blue;bracket-level-2=fg=red;list-selected=fg=black,bg=white");
        assert_eq!(t.style(Kind::Command).fg, Color::Ansi(4));
        assert_eq!(t.style(Kind::Bracket(1)).fg, Color::Ansi(1));
        assert_eq!(t.ui(Ui::ListSelected).bg, Color::Ansi(7));
        assert!(t.style(Kind::Default).is_none());
    }
}
