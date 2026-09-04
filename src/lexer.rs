//! A tolerant, single-pass lexer for interactive bash command lines.
//!
//! The goal is not to be a full parser but to classify every byte of the line
//! into a highlighting category, closely mirroring what
//! `zsh-syntax-highlighting` / `fast-syntax-highlighting` do for zsh:
//! command words are looked up (alias / function / builtin / PATH), arguments
//! are checked against the filesystem, quotes, expansions, redirections,
//! globs, comments and heredocs get their own categories.
//!
//! The lexer is deliberately forgiving: unterminated constructs simply extend
//! to the end of the input, which is exactly what a user wants to see while
//! typing.

use std::fmt;

/// Highlight category of a span.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Kind {
    Default,
    /// External command found in PATH (or an executable path).
    Command,
    UnknownCommand,
    Builtin,
    Alias,
    Function,
    Keyword,
    /// `sudo`, `command`, `exec`, `env`, ... — the real command follows.
    Precommand,
    /// Name in `name() { ...` or `function name`.
    FuncDef,
    Argument,
    Option,
    /// Argument that names an existing file or directory.
    Path,
    /// `NAME=` part of an assignment.
    Assignment,
    SingleQuoted,
    DoubleQuoted,
    DollarQuoted,
    /// `$var`, `${...}` outside of double quotes.
    Variable,
    /// `$var`, `${...}` inside double quotes.
    VariableDq,
    /// Backslash escape outside quotes.
    Escape,
    /// Backslash escape inside double quotes.
    EscapeDq,
    /// `$(`, `)` and backticks around a command substitution.
    CmdSubst,
    /// `$((`, `))`, `((`, `))`.
    ArithDelim,
    /// Body of an arithmetic expansion / command.
    Arith,
    /// `<(`, `>(` and the closing paren of a process substitution.
    ProcSubst,
    Redirect,
    /// `|`, `||`, `&&`, `;`, `&`, ...
    Separator,
    Comment,
    Glob,
    /// `{a,b}` / `{1..9}` brace expansion.
    Brace,
    HistoryExp,
    HeredocDelim,
    HeredocBody,
    Tilde,
    /// Subshell parens / group braces, colored by nesting depth.
    Bracket(u8),
    BracketError,
}

impl Kind {
    /// Name used in the style configuration (`BASH_TOOLS_STYLES`).
    pub fn name(self) -> &'static str {
        match self {
            Kind::Default => "default",
            Kind::Command => "command",
            Kind::UnknownCommand => "unknown-command",
            Kind::Builtin => "builtin",
            Kind::Alias => "alias",
            Kind::Function => "function",
            Kind::Keyword => "reserved-word",
            Kind::Precommand => "precommand",
            Kind::FuncDef => "function-definition",
            Kind::Argument => "argument",
            Kind::Option => "option",
            Kind::Path => "path",
            Kind::Assignment => "assign",
            Kind::SingleQuoted => "single-quoted",
            Kind::DoubleQuoted => "double-quoted",
            Kind::DollarQuoted => "dollar-quoted",
            Kind::Variable => "variable",
            Kind::VariableDq => "variable-in-quotes",
            Kind::Escape => "escape",
            Kind::EscapeDq => "escape-in-quotes",
            Kind::CmdSubst => "command-substitution",
            Kind::ArithDelim => "arithmetic-delimiter",
            Kind::Arith => "arithmetic",
            Kind::ProcSubst => "process-substitution",
            Kind::Redirect => "redirection",
            Kind::Separator => "separator",
            Kind::Comment => "comment",
            Kind::Glob => "globbing",
            Kind::Brace => "brace-expansion",
            Kind::HistoryExp => "history-expansion",
            Kind::HeredocDelim => "heredoc-delimiter",
            Kind::HeredocBody => "heredoc-body",
            Kind::Tilde => "tilde",
            Kind::Bracket(_) => "bracket",
            Kind::BracketError => "bracket-error",
        }
    }
}

/// A classified byte range of the input.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub kind: Kind,
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..{}:{:?}", self.start, self.end, self.kind)
    }
}

/// Environment queries the lexer needs to classify words.
pub trait Lookup {
    /// Classify a command name: `Some(Alias|Function|Builtin|Command)` or `None`
    /// when nothing of that name exists.
    fn command_kind(&self, name: &str) -> Option<Kind>;
    /// Whether `path` (possibly starting with `~`) exists on disk.
    fn path_exists(&self, path: &str) -> bool;
    /// Whether history expansion (`!!`) is enabled.
    fn histexpand(&self) -> bool {
        true
    }
}

/// A [`Lookup`] that knows nothing (everything is unknown, no paths).
pub struct NoLookup;
impl Lookup for NoLookup {
    fn command_kind(&self, _: &str) -> Option<Kind> {
        None
    }
    fn path_exists(&self, _: &str) -> bool {
        false
    }
}

pub const BUILTINS: &[&str] = &[
    ".", ":", "[", "alias", "bg", "bind", "break", "builtin", "caller", "cd", "command", "compgen",
    "complete", "compopt", "continue", "declare", "dirs", "disown", "echo", "enable", "eval",
    "exec", "exit", "export", "false", "fc", "fg", "getopts", "hash", "help", "history", "jobs",
    "kill", "let", "local", "logout", "mapfile", "popd", "printf", "pushd", "pwd", "read",
    "readarray", "readonly", "return", "set", "shift", "shopt", "source", "suspend", "test",
    "times", "trap", "true", "type", "typeset", "ulimit", "umask", "unalias", "unset", "wait",
];

pub const KEYWORDS: &[&str] = &[
    "!", "[[", "]]", "{", "}", "case", "coproc", "do", "done", "elif", "else", "esac", "fi",
    "for", "function", "if", "in", "select", "then", "time", "until", "while",
];

pub fn is_builtin(name: &str) -> bool {
    BUILTINS.contains(&name)
}

pub fn is_keyword(name: &str) -> bool {
    KEYWORDS.contains(&name)
}

/// Precommands: the next non-option word is again a command.
/// Returns the list of options that take a separate argument.
fn precommand_arg_options(name: &str) -> Option<&'static [&'static str]> {
    match name {
        "sudo" => Some(&["-u", "-g", "-p", "-C", "-r", "-t", "-U", "-h", "-D", "-R", "-T"]),
        "doas" => Some(&["-u", "-C"]),
        "pkexec" => Some(&["--user"]),
        "command" | "builtin" | "nohup" | "noglob" | "nocorrect" => Some(&[]),
        "exec" => Some(&["-a"]),
        "env" => Some(&["-u", "-C", "-S", "--unset", "--chdir", "--split-string"]),
        "nice" => Some(&["-n", "--adjustment"]),
        "stdbuf" => Some(&["-i", "-o", "-e"]),
        "ionice" => Some(&["-c", "-n", "-p"]),
        "chronic" | "unbuffer" | "time" | "timeout" => Some(&["-k", "-s", "--signal", "--kill-after"]),
        "xargs" => None,
        _ => None,
    }
}

fn is_name_start(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphabetic()
}
fn is_name_char(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}
fn is_metachar(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'|' | b'&' | b';' | b'(' | b')' | b'<' | b'>')
}
fn is_special_param(b: u8) -> bool {
    matches!(b, b'?' | b'$' | b'!' | b'#' | b'@' | b'*' | b'-' | b'0'..=b'9' | b'_')
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Term {
    Eof,
    Paren,
    Backtick,
    /// `${ cmd; }` (bash 5.3 "nofork" command substitution)
    Brace,
}

#[derive(Clone, Debug)]
struct Heredoc {
    delim: Vec<u8>,
    strip_tabs: bool,
}

/// What we expect the next word to be, when it is not an ordinary word.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Expect {
    None,
    /// After `for` / `select`: a variable name.
    ForVar,
    /// After `for x`: `in` or `do`.
    ForIn,
    /// After `case`: the subject word.
    CaseWord,
    /// After `case x`: `in`.
    CaseIn,
    /// After `function`: the name.
    FuncName,
    /// After a redirection operator: the target.
    RedirTarget,
    /// After `<<`: a heredoc delimiter.
    HeredocDelim(bool),
    /// Argument of a precommand option (e.g. the user after `sudo -u`).
    PrecmdOptArg,
}

#[derive(Clone, Debug)]
struct Ctx {
    cmdpos: bool,
    expect: Expect,
    /// Inside `[[ ... ]]`.
    in_test: bool,
    /// Nesting of `case ... esac` and whether we're between patterns.
    case_depth: u32,
    case_pattern: bool,
    /// Active precommand (e.g. `sudo`): next non-option word is the command.
    precmd: Option<&'static [&'static str]>,
    /// After `--`: everything is an argument.
    dashdash: bool,
    /// `for x in a b c`: words are plain arguments until `;`/newline.
    for_words: bool,
}

impl Ctx {
    fn new() -> Self {
        Ctx {
            cmdpos: true,
            expect: Expect::None,
            in_test: false,
            case_depth: 0,
            case_pattern: false,
            precmd: None,
            dashdash: false,
            for_words: false,
        }
    }
    fn new_command(&mut self) {
        self.cmdpos = true;
        self.expect = Expect::None;
        self.in_test = false;
        self.precmd = None;
        self.dashdash = false;
        self.for_words = false;
    }
}

/// Result of scanning one word.
struct Word {
    start: usize,
    end: usize,
    /// Index into `spans` of the first span of this word.
    first_span: usize,
    /// Literal text with quotes removed; only meaningful if `!has_expansion`.
    text: Vec<u8>,
    has_expansion: bool,
    has_glob: bool,
    /// Entire word is unquoted, unescaped plain text.
    plain: bool,
    /// Byte offset (relative to `start`) of the first unquoted `=` if the word
    /// begins with a valid assignment name.
    assign_eq: Option<usize>,
    starts_with_tilde: bool,
}

pub struct Lexer<'a> {
    s: &'a [u8],
    pos: usize,
    spans: Vec<Span>,
    lookup: &'a dyn Lookup,
    heredocs: Vec<Heredoc>,
    depth: u8,
    /// Nesting level of backtick substitutions we are inside of.
    in_backtick: u32,
    /// Set when the input ended inside an unfinished construct.
    pub incomplete: bool,
}

/// Lex `src` into a sorted, non-overlapping list of spans covering every byte.
pub fn lex(src: &str, lookup: &dyn Lookup) -> Vec<Span> {
    let mut lx = Lexer::new(src, lookup);
    lx.lex_list(Term::Eof, &mut Ctx::new());
    lx.finish()
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str, lookup: &'a dyn Lookup) -> Self {
        Lexer {
            s: src.as_bytes(),
            pos: 0,
            spans: Vec::with_capacity(32),
            lookup,
            heredocs: Vec::new(),
            depth: 0,
            in_backtick: 0,
            incomplete: false,
        }
    }

    /// Run the lexer over the whole input.
    pub fn run(mut self) -> (Vec<Span>, bool) {
        self.lex_list(Term::Eof, &mut Ctx::new());
        let inc = self.incomplete;
        (self.finish(), inc)
    }

    fn finish(mut self) -> Vec<Span> {
        // Fill gaps (whitespace etc.) with Default spans and merge adjacent
        // spans of equal kind so renderers get a compact list.
        let n = self.s.len();
        let mut out: Vec<Span> = Vec::with_capacity(self.spans.len() + 8);
        let mut cur = 0;
        self.spans.retain(|sp| sp.end > sp.start);
        for sp in self.spans {
            if sp.start > cur {
                push_merge(&mut out, Span { start: cur, end: sp.start, kind: Kind::Default });
            }
            if sp.start < cur {
                // overlapping span (should not happen); clip
                if sp.end <= cur {
                    continue;
                }
                push_merge(&mut out, Span { start: cur, end: sp.end, kind: sp.kind });
            } else {
                push_merge(&mut out, sp);
            }
            cur = sp.end.max(cur);
        }
        if cur < n {
            push_merge(&mut out, Span { start: cur, end: n, kind: Kind::Default });
        }
        out
    }

    // ----- low level helpers -------------------------------------------------

    #[inline]
    fn peek(&self) -> Option<u8> {
        self.s.get(self.pos).copied()
    }
    #[inline]
    fn peek_at(&self, off: usize) -> Option<u8> {
        self.s.get(self.pos + off).copied()
    }
    #[inline]
    fn at_end(&self) -> bool {
        self.pos >= self.s.len()
    }
    fn starts_with(&self, pat: &[u8]) -> bool {
        self.s[self.pos..].starts_with(pat)
    }
    fn push(&mut self, start: usize, end: usize, kind: Kind) {
        if end > start {
            self.spans.push(Span { start, end, kind });
        }
    }
    /// Length in bytes of the (possibly multibyte) character at `pos`.
    fn char_len(&self) -> usize {
        match self.s.get(self.pos) {
            None => 0,
            Some(&b) if b < 0x80 => 1,
            Some(&b) => {
                let n = if b >= 0xF0 {
                    4
                } else if b >= 0xE0 {
                    3
                } else if b >= 0xC0 {
                    2
                } else {
                    1
                };
                n.min(self.s.len() - self.pos)
            }
        }
    }
    fn skip_blanks(&mut self) {
        while let Some(b) = self.peek() {
            if b == b' ' || b == b'\t' {
                self.pos += 1;
            } else if b == b'\\' && self.peek_at(1) == Some(b'\n') {
                self.push(self.pos, self.pos + 2, Kind::Escape);
                self.pos += 2;
            } else {
                break;
            }
        }
    }

    // ----- command lists ------------------------------------------------------

    fn lex_list(&mut self, term: Term, ctx: &mut Ctx) {
        loop {
            self.skip_blanks();
            let start = self.pos;
            let b = match self.peek() {
                None => {
                    if term != Term::Eof {
                        self.incomplete = true;
                    }
                    if !ctx.cmdpos || self.heredocs.len() > 0 {
                        // a trailing operator means bash will ask for more
                    }
                    return;
                }
                Some(b) => b,
            };
            match b {
                b'\n' => {
                    self.pos += 1;
                    self.lex_heredoc_bodies();
                    if ctx.for_words {
                        // `for x in a b` newline → `do` follows
                        ctx.for_words = false;
                    }
                    ctx.new_command();
                }
                b'#' => {
                    // comment: `#` at the start of a word
                    let end = self.line_end();
                    self.push(start, end, Kind::Comment);
                    self.pos = end;
                }
                b')' => {
                    if term == Term::Paren {
                        return;
                    }
                    if ctx.case_pattern {
                        self.push(start, start + 1, Kind::Separator);
                        self.pos += 1;
                        ctx.case_pattern = false;
                        ctx.new_command();
                    } else {
                        self.push(start, start + 1, Kind::BracketError);
                        self.pos += 1;
                    }
                }
                b'}' if term == Term::Brace && self.word_is_lone_brace() => return,
                b'`' => {
                    if term == Term::Backtick {
                        return;
                    }
                    self.lex_backtick();
                    ctx.cmdpos = false;
                }
                b'(' => {
                    if self.starts_with(b"((") {
                        self.lex_arith_command();
                        ctx.cmdpos = false;
                        continue;
                    }
                    // function definition `name ()`?
                    if self.last_word_is_funcdef_candidate(ctx) {
                        let mut p = self.pos + 1;
                        while p < self.s.len() && (self.s[p] == b' ' || self.s[p] == b'\t') {
                            p += 1;
                        }
                        if self.s.get(p) == Some(&b')') {
                            // mark previous word as FuncDef
                            if let Some(sp) = self.spans.last_mut() {
                                sp.kind = Kind::FuncDef;
                            }
                            self.push(start, start + 1, Kind::Bracket(self.depth));
                            self.push(p, p + 1, Kind::Bracket(self.depth));
                            self.pos = p + 1;
                            ctx.new_command();
                            continue;
                        }
                    }
                    let level = self.depth;
                    self.push(start, start + 1, Kind::Bracket(level));
                    self.pos += 1;
                    self.depth = self.depth.wrapping_add(1);
                    let mut inner = Ctx::new();
                    self.lex_list(Term::Paren, &mut inner);
                    self.depth = level;
                    if self.peek() == Some(b')') {
                        self.push(self.pos, self.pos + 1, Kind::Bracket(level));
                        self.pos += 1;
                    } else if let Some(sp) = self.spans.iter_mut().rev().find(|s| s.start == start) {
                        sp.kind = Kind::BracketError;
                    }
                    ctx.cmdpos = false;
                }
                b';' => {
                    let len = if self.starts_with(b";;&") {
                        3
                    } else if self.starts_with(b";;") || self.starts_with(b";&") {
                        2
                    } else {
                        1
                    };
                    self.push(start, start + len, Kind::Separator);
                    self.pos += len;
                    ctx.new_command();
                    if len > 1 && ctx.case_depth > 0 {
                        ctx.case_pattern = true;
                    }
                }
                b'|' => {
                    let len = if self.starts_with(b"||") || self.starts_with(b"|&") { 2 } else { 1 };
                    self.push(start, start + len, Kind::Separator);
                    self.pos += len;
                    if ctx.case_pattern {
                        // pattern alternative separator; stay in pattern mode
                    } else {
                        ctx.new_command();
                    }
                }
                b'&' => {
                    if self.starts_with(b"&>>") || self.starts_with(b"&>") {
                        let len = if self.starts_with(b"&>>") { 3 } else { 2 };
                        self.push(start, start + len, Kind::Redirect);
                        self.pos += len;
                        ctx.expect = Expect::RedirTarget;
                        continue;
                    }
                    let len = if self.starts_with(b"&&") { 2 } else { 1 };
                    self.push(start, start + len, Kind::Separator);
                    self.pos += len;
                    ctx.new_command();
                }
                b'<' | b'>' => {
                    if !self.lex_redirect(ctx) {
                        // process substitution as a word
                        self.lex_word(ctx);
                    }
                }
                b'0'..=b'9' if self.digits_then_redirect() => {
                    self.lex_redirect(ctx);
                }
                b'{' if self.starts_with(b"{") && self.brace_fd_redirect() => {
                    self.lex_redirect(ctx);
                }
                _ => {
                    self.lex_word(ctx);
                }
            }
        }
    }

    fn word_is_lone_brace(&self) -> bool {
        let next = self.peek_at(1);
        next.is_none() || next.map(is_metachar).unwrap_or(false)
    }

    fn line_end(&self) -> usize {
        let mut p = self.pos;
        while p < self.s.len() && self.s[p] != b'\n' {
            p += 1;
        }
        p
    }

    fn last_word_is_funcdef_candidate(&self, ctx: &Ctx) -> bool {
        // Previous span must be a command-position word that ends right
        // before pos (allowing blanks). We only accept simple identifiers.
        if let Some(sp) = self.spans.last() {
            let gap = &self.s[sp.end..self.pos];
            if !gap.iter().all(|&b| b == b' ' || b == b'\t') {
                return false;
            }
            let word = &self.s[sp.start..sp.end];
            if word.is_empty() || !word.iter().all(|&b| is_name_char(b) || b == b'-' || b == b'.' || b == b':') {
                return false;
            }
            return matches!(
                sp.kind,
                Kind::Command
                    | Kind::UnknownCommand
                    | Kind::Builtin
                    | Kind::Alias
                    | Kind::Function
                    | Kind::Argument
                    | Kind::Precommand
            ) && !ctx.in_test
                && !ctx.case_pattern;
        }
        false
    }

    /// `2>`, `12>>`, ...
    fn digits_then_redirect(&self) -> bool {
        let mut p = self.pos;
        while p < self.s.len() && self.s[p].is_ascii_digit() {
            p += 1;
        }
        p > self.pos && matches!(self.s.get(p), Some(b'<') | Some(b'>'))
    }

    /// `{var}>file`
    fn brace_fd_redirect(&self) -> bool {
        let mut p = self.pos + 1;
        if !self.s.get(p).map(|&b| is_name_start(b)).unwrap_or(false) {
            return false;
        }
        while p < self.s.len() && is_name_char(self.s[p]) {
            p += 1;
        }
        self.s.get(p) == Some(&b'}') && matches!(self.s.get(p + 1), Some(b'<') | Some(b'>'))
    }

    /// Lex a redirection operator starting at pos. Returns false if what
    /// looks like `<(`/`>(` is a process substitution (handled as a word).
    fn lex_redirect(&mut self, ctx: &mut Ctx) -> bool {
        let start = self.pos;
        // optional fd number or {var}
        while self.peek().map(|b| b.is_ascii_digit()).unwrap_or(false) {
            self.pos += 1;
        }
        if self.peek() == Some(b'{') {
            while let Some(b) = self.peek() {
                self.pos += 1;
                if b == b'}' {
                    break;
                }
            }
        }
        let had_fd = self.pos > start;
        if !had_fd && (self.starts_with(b"<(") || self.starts_with(b">(")) {
            self.pos = start;
            return false;
        }
        let rest = &self.s[self.pos..];
        let (len, heredoc) = if rest.starts_with(b"<<<") {
            (3, None)
        } else if rest.starts_with(b"<<-") {
            (3, Some(true))
        } else if rest.starts_with(b"<<") {
            (2, Some(false))
        } else if rest.starts_with(b">>") || rest.starts_with(b"<>") || rest.starts_with(b">|") {
            (2, None)
        } else if rest.starts_with(b"<&") || rest.starts_with(b">&") {
            (2, None)
        } else {
            (1, None)
        };
        let is_dup = rest.starts_with(b"<&") || rest.starts_with(b">&");
        self.pos += len;
        self.push(start, self.pos, Kind::Redirect);
        if let Some(strip) = heredoc {
            ctx.expect = Expect::HeredocDelim(strip);
        } else if is_dup {
            // `2>&1`, `>&-`: fd targets are part of the redirection
            let tstart = self.pos;
            let mut p = self.pos;
            while p < self.s.len() && (self.s[p].is_ascii_digit() || self.s[p] == b'-') {
                p += 1;
            }
            if p > tstart && (p >= self.s.len() || is_metachar(self.s[p])) {
                self.pos = p;
                self.push(tstart, p, Kind::Redirect);
            } else {
                ctx.expect = Expect::RedirTarget;
            }
        } else {
            ctx.expect = Expect::RedirTarget;
        }
        true
    }

    fn lex_backtick(&mut self) {
        let start = self.pos;
        self.push(start, start + 1, Kind::CmdSubst);
        self.pos += 1;
        let level = self.depth;
        self.depth = self.depth.wrapping_add(1);
        self.in_backtick += 1;
        let mut inner = Ctx::new();
        self.lex_list(Term::Backtick, &mut inner);
        self.in_backtick -= 1;
        self.depth = level;
        if self.peek() == Some(b'`') {
            self.push(self.pos, self.pos + 1, Kind::CmdSubst);
            self.pos += 1;
        } else {
            self.incomplete = true;
        }
    }

    /// `<(cmd)` / `>(cmd)` starting at pos.
    fn lex_proc_subst(&mut self) {
        let start = self.pos;
        self.push(start, start + 2, Kind::ProcSubst);
        self.pos += 2;
        let level = self.depth;
        self.depth = self.depth.wrapping_add(1);
        let mut inner = Ctx::new();
        self.lex_list(Term::Paren, &mut inner);
        self.depth = level;
        if self.peek() == Some(b')') {
            self.push(self.pos, self.pos + 1, Kind::ProcSubst);
            self.pos += 1;
        } else {
            self.incomplete = true;
        }
    }

    /// `(( expr ))` as a command.
    fn lex_arith_command(&mut self) {
        let start = self.pos;
        self.push(start, start + 2, Kind::ArithDelim);
        self.pos += 2;
        self.lex_arith_body(b"))");
    }

    /// Scan an arithmetic body until `close` at nesting level zero.
    fn lex_arith_body(&mut self, close: &[u8]) {
        let bstart = self.pos;
        let mut depth = 0i32;
        while let Some(b) = self.peek() {
            if depth == 0 && self.starts_with(close) {
                self.push(bstart, self.pos, Kind::Arith);
                self.push(self.pos, self.pos + close.len(), Kind::ArithDelim);
                self.pos += close.len();
                return;
            }
            match b {
                b'(' => depth += 1,
                b')' => depth -= 1,
                b'\'' | b'"' => {
                    // quotes inside arithmetic are rare; skip them as a unit
                    self.pos += 1;
                    while let Some(c) = self.peek() {
                        self.pos += 1;
                        if c == b {
                            break;
                        }
                    }
                    continue;
                }
                _ => {}
            }
            self.pos += 1;
        }
        self.push(bstart, self.pos, Kind::Arith);
        self.incomplete = true;
    }

    /// Consume heredoc bodies that start after a newline.
    fn lex_heredoc_bodies(&mut self) {
        if self.heredocs.is_empty() {
            return;
        }
        let docs = std::mem::take(&mut self.heredocs);
        for hd in docs {
            let body_start = self.pos;
            loop {
                if self.at_end() {
                    self.push(body_start, self.pos, Kind::HeredocBody);
                    self.incomplete = true;
                    break;
                }
                let ls = self.pos;
                let le = self.line_end();
                let mut cs = ls;
                if hd.strip_tabs {
                    while cs < le && self.s[cs] == b'\t' {
                        cs += 1;
                    }
                }
                if &self.s[cs..le] == &hd.delim[..] {
                    self.push(body_start, ls, Kind::HeredocBody);
                    self.push(ls, le, Kind::HeredocDelim);
                    self.pos = le;
                    if self.peek() == Some(b'\n') {
                        self.pos += 1;
                    }
                    break;
                }
                self.pos = if le < self.s.len() { le + 1 } else { le };
            }
        }
    }

    // ----- words --------------------------------------------------------------

    fn lex_word(&mut self, ctx: &mut Ctx) {
        let w = self.scan_word(ctx.in_test);
        if w.end == w.start {
            // stray character we do not understand; consume one char
            let n = self.char_len().max(1);
            self.push(self.pos, self.pos + n, Kind::Default);
            self.pos += n;
            return;
        }
        self.classify(w, ctx);
    }

    fn scan_word(&mut self, in_test: bool) -> Word {
        let start = self.pos;
        let first_span = self.spans.len();
        let mut w = Word {
            start,
            end: start,
            first_span,
            text: Vec::new(),
            has_expansion: false,
            has_glob: false,
            plain: true,
            assign_eq: None,
            starts_with_tilde: false,
        };
        // plain run tracking: we emit Default spans for plain runs and fix
        // their kind in classify()
        let mut run_start = self.pos;
        macro_rules! flush_run {
            () => {
                if self.pos > run_start {
                    self.push(run_start, self.pos, Kind::Default);
                }
            };
        }
        let mut name_ok = self.peek().map(is_name_start).unwrap_or(false);
        let mut first = true;
        while let Some(b) = self.peek() {
            if is_metachar(b) {
                // process substitution can start inside a word: `x=<(cmd)`
                if (b == b'<' || b == b'>') && self.peek_at(1) == Some(b'(') {
                    flush_run!();
                    self.lex_proc_subst();
                    w.has_expansion = true;
                    w.plain = false;
                    name_ok = false;
                    run_start = self.pos;
                    first = false;
                    continue;
                }
                break;
            }
            if b == b'`' && self.in_backtick > 0 {
                break;
            }
            // `]]` closes a test and is a word of its own
            if in_test && self.starts_with(b"]]") {
                if self.pos == start {
                    w.text.extend_from_slice(b"]]");
                    self.pos += 2;
                }
                break;
            }
            match b {
                b'\\' => {
                    flush_run!();
                    if self.peek_at(1) == Some(b'\n') {
                        self.push(self.pos, self.pos + 2, Kind::Escape);
                        self.pos += 2;
                    } else if self.pos + 1 < self.s.len() {
                        self.pos += 1;
                        let n = self.char_len();
                        w.text.extend_from_slice(&self.s[self.pos..self.pos + n]);
                        self.push(self.pos - 1, self.pos + n, Kind::Escape);
                        self.pos += n;
                    } else {
                        self.push(self.pos, self.pos + 1, Kind::Escape);
                        self.pos += 1;
                        self.incomplete = true;
                    }
                    w.plain = false;
                    name_ok = false;
                    run_start = self.pos;
                }
                b'\'' => {
                    flush_run!();
                    let qs = self.pos;
                    self.pos += 1;
                    let cs = self.pos;
                    while let Some(c) = self.peek() {
                        if c == b'\'' {
                            break;
                        }
                        self.pos += 1;
                    }
                    w.text.extend_from_slice(&self.s[cs..self.pos]);
                    if self.peek() == Some(b'\'') {
                        self.pos += 1;
                    } else {
                        self.incomplete = true;
                    }
                    self.push(qs, self.pos, Kind::SingleQuoted);
                    w.plain = false;
                    name_ok = false;
                    run_start = self.pos;
                }
                b'"' => {
                    flush_run!();
                    self.lex_double_quoted(&mut w);
                    name_ok = false;
                    run_start = self.pos;
                }
                b'$' => {
                    flush_run!();
                    self.lex_dollar(&mut w, false);
                    name_ok = false;
                    run_start = self.pos;
                }
                b'`' => {
                    flush_run!();
                    self.lex_backtick();
                    w.has_expansion = true;
                    w.plain = false;
                    name_ok = false;
                    run_start = self.pos;
                }
                b'*' | b'?' => {
                    flush_run!();
                    self.push(self.pos, self.pos + 1, Kind::Glob);
                    w.text.push(b);
                    self.pos += 1;
                    w.has_glob = true;
                    w.plain = false;
                    name_ok = false;
                    run_start = self.pos;
                }
                b'[' => {
                    if let Some(end) = self.glob_bracket_end() {
                        flush_run!();
                        self.push(self.pos, end, Kind::Glob);
                        w.text.extend_from_slice(&self.s[self.pos..end]);
                        self.pos = end;
                        w.has_glob = true;
                        w.plain = false;
                        name_ok = false;
                        run_start = self.pos;
                    } else {
                        w.text.push(b);
                        self.pos += 1;
                        name_ok = false;
                    }
                }
                b'{' => {
                    if let Some(end) = self.brace_expansion_end() {
                        flush_run!();
                        self.push(self.pos, end, Kind::Brace);
                        w.text.extend_from_slice(&self.s[self.pos..end]);
                        self.pos = end;
                        w.has_glob = true;
                        w.plain = false;
                        name_ok = false;
                        run_start = self.pos;
                    } else {
                        w.text.push(b);
                        self.pos += 1;
                        name_ok = false;
                    }
                }
                b'~' if first
                    || (w.assign_eq.is_some()
                        && matches!(self.s.get(self.pos - 1), Some(b'=') | Some(b':'))) =>
                {
                    let mut p = self.pos + 1;
                    while p < self.s.len() && (is_name_char(self.s[p]) || self.s[p] == b'-' || self.s[p] == b'.') {
                        p += 1;
                    }
                    if p >= self.s.len() || self.s[p] == b'/' || is_metachar(self.s[p]) || self.s[p] == b':' {
                        flush_run!();
                        self.push(self.pos, p, Kind::Tilde);
                        w.text.extend_from_slice(&self.s[self.pos..p]);
                        if first {
                            w.starts_with_tilde = true;
                        }
                        self.pos = p;
                        run_start = self.pos;
                    } else {
                        w.text.push(b);
                        self.pos += 1;
                    }
                    name_ok = false;
                }
                b'!' if self.lookup.histexpand() && self.history_expansion_len() > 0 => {
                    flush_run!();
                    let n = self.history_expansion_len();
                    self.push(self.pos, self.pos + n, Kind::HistoryExp);
                    self.pos += n;
                    w.has_expansion = true;
                    w.plain = false;
                    name_ok = false;
                    run_start = self.pos;
                }
                b'=' => {
                    if w.assign_eq.is_none() && name_ok && self.pos > start {
                        w.assign_eq = Some(self.pos - start);
                    } else if w.assign_eq.is_none()
                        && self.pos > start
                        && self.s[self.pos - 1] == b'+'
                        && self.pos - 1 > start
                        && self.s[start..self.pos - 1].iter().all(|&c| is_name_char(c))
                        && is_name_start(self.s[start])
                    {
                        w.assign_eq = Some(self.pos - start);
                    } else if w.assign_eq.is_none() && self.pos > start && self.s[self.pos - 1] == b']' {
                        // name[idx]=
                        if let Some(lb) = self.s[start..self.pos].iter().position(|&c| c == b'[') {
                            if lb > 0 && is_name_start(self.s[start]) && self.s[start..start + lb].iter().all(|&c| is_name_char(c)) {
                                w.assign_eq = Some(self.pos - start);
                            }
                        }
                    }
                    w.text.push(b);
                    self.pos += 1;
                    name_ok = false;
                }
                b'+' if name_ok && self.peek_at(1) == Some(b'=') => {
                    w.text.push(b);
                    self.pos += 1;
                    // keep name_ok so that `=` branch records the assignment
                }
                _ => {
                    let n = self.char_len();
                    if !is_name_char(b) {
                        name_ok = false;
                    }
                    w.text.extend_from_slice(&self.s[self.pos..self.pos + n]);
                    self.pos += n;
                }
            }
            first = false;
        }
        flush_run!();
        w.end = self.pos;
        w
    }

    fn glob_bracket_end(&self) -> Option<usize> {
        // `[` ... `]` with optional leading `!` or `^`; `]` may be first char
        let mut p = self.pos + 1;
        if matches!(self.s.get(p), Some(b'!') | Some(b'^')) {
            p += 1;
        }
        if self.s.get(p) == Some(&b']') {
            p += 1;
        }
        while p < self.s.len() {
            let c = self.s[p];
            if c == b']' {
                return Some(p + 1);
            }
            if is_metachar(c) || c == b'\'' || c == b'"' {
                return None;
            }
            if c == b'[' && self.s.get(p + 1) == Some(&b':') {
                // character class [:alpha:]
                if let Some(off) = self.s[p + 2..].windows(2).position(|w| w == b":]") {
                    p += 2 + off + 2;
                    continue;
                }
            }
            p += 1;
        }
        None
    }

    fn brace_expansion_end(&self) -> Option<usize> {
        let mut p = self.pos + 1;
        let mut depth = 1;
        let mut has_comma = false;
        let mut has_dots = false;
        while p < self.s.len() {
            let c = self.s[p];
            match c {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return if has_comma || has_dots { Some(p + 1) } else { None };
                    }
                }
                b',' if depth == 1 => has_comma = true,
                b'.' if depth == 1 && self.s.get(p + 1) == Some(&b'.') => has_dots = true,
                b'\\' => p += 1,
                b'\'' | b'"' => {
                    // quoted section inside braces: skip
                    p += 1;
                    while p < self.s.len() && self.s[p] != c {
                        p += 1;
                    }
                }
                b'$' if self.s.get(p + 1) == Some(&b'(') || self.s.get(p + 1) == Some(&b'{') => {
                    return None;
                }
                _ if is_metachar(c) => return None,
                _ => {}
            }
            p += 1;
        }
        None
    }

    fn history_expansion_len(&self) -> usize {
        // `!` followed by: `!`, `$`, `*`, `^`, `-n`, `n`, `word`, `?word?`
        let p = self.pos + 1;
        match self.s.get(p) {
            None => 0,
            Some(&c) => match c {
                b'!' | b'$' | b'*' | b'^' | b'#' => 2,
                b'-' | b'0'..=b'9' => {
                    let mut q = p;
                    if c == b'-' {
                        q += 1;
                    }
                    let ds = q;
                    while q < self.s.len() && self.s[q].is_ascii_digit() {
                        q += 1;
                    }
                    if q > ds {
                        q - self.pos
                    } else {
                        0
                    }
                }
                b'?' => {
                    let mut q = p + 1;
                    while q < self.s.len() && self.s[q] != b'?' && self.s[q] != b'\n' {
                        q += 1;
                    }
                    if q < self.s.len() && self.s[q] == b'?' {
                        q + 1 - self.pos
                    } else {
                        q - self.pos
                    }
                }
                _ if is_name_char(c) || c == b'/' || c == b'.' => {
                    let mut q = p;
                    while q < self.s.len() && !is_metachar(self.s[q]) && !matches!(self.s[q], b'=' | b':' | b'"' | b'\'') {
                        q += 1;
                    }
                    q - self.pos
                }
                _ => 0,
            },
        }
    }

    /// Scan `"..."` starting at pos (which is the opening quote).
    fn lex_double_quoted(&mut self, w: &mut Word) {
        let qs = self.pos;
        self.pos += 1;
        let mut run_start = qs;
        w.plain = false;
        loop {
            let b = match self.peek() {
                None => {
                    self.push(run_start, self.pos, Kind::DoubleQuoted);
                    self.incomplete = true;
                    return;
                }
                Some(b) => b,
            };
            match b {
                b'"' => {
                    self.pos += 1;
                    self.push(run_start, self.pos, Kind::DoubleQuoted);
                    return;
                }
                b'\\' => {
                    if let Some(n) = self.peek_at(1) {
                        if matches!(n, b'$' | b'`' | b'"' | b'\\' | b'\n') {
                            self.push(run_start, self.pos, Kind::DoubleQuoted);
                            self.push(self.pos, self.pos + 2, Kind::EscapeDq);
                            if n != b'\n' {
                                w.text.push(n);
                            }
                            self.pos += 2;
                            run_start = self.pos;
                            continue;
                        }
                    }
                    w.text.push(b);
                    self.pos += 1;
                }
                b'$' => {
                    self.push(run_start, self.pos, Kind::DoubleQuoted);
                    self.lex_dollar(w, true);
                    run_start = self.pos;
                }
                b'`' => {
                    self.push(run_start, self.pos, Kind::DoubleQuoted);
                    self.lex_backtick();
                    w.has_expansion = true;
                    run_start = self.pos;
                }
                b'!' if self.lookup.histexpand() && self.history_expansion_len() > 0 => {
                    self.push(run_start, self.pos, Kind::DoubleQuoted);
                    let n = self.history_expansion_len();
                    self.push(self.pos, self.pos + n, Kind::HistoryExp);
                    self.pos += n;
                    w.has_expansion = true;
                    run_start = self.pos;
                }
                _ => {
                    let n = self.char_len();
                    w.text.extend_from_slice(&self.s[self.pos..self.pos + n]);
                    self.pos += n;
                }
            }
        }
    }

    /// Scan a `$...` construct at pos. `dq` tells whether we are inside
    /// double quotes (affects the span kind for variables).
    fn lex_dollar(&mut self, w: &mut Word, dq: bool) {
        let start = self.pos;
        let vkind = if dq { Kind::VariableDq } else { Kind::Variable };
        match self.peek_at(1) {
            Some(b'\'') if !dq => {
                // $'...'
                self.pos += 2;
                while let Some(c) = self.peek() {
                    if c == b'\\' {
                        self.pos += 2;
                        continue;
                    }
                    self.pos += 1;
                    if c == b'\'' {
                        self.push(start, self.pos, Kind::DollarQuoted);
                        w.plain = false;
                        return;
                    }
                }
                self.pos = self.pos.min(self.s.len());
                self.push(start, self.pos, Kind::DollarQuoted);
                self.incomplete = true;
                w.plain = false;
            }
            Some(b'"') if !dq => {
                // $"..." locale string: treat like a double-quoted string
                self.pos += 1;
                self.push(start, self.pos, Kind::DoubleQuoted);
                self.lex_double_quoted(w);
            }
            Some(b'(') => {
                w.has_expansion = true;
                w.plain = false;
                if self.peek_at(2) == Some(b'(') {
                    self.push(start, start + 3, Kind::ArithDelim);
                    self.pos += 3;
                    self.lex_arith_body(b"))");
                } else {
                    self.push(start, start + 2, Kind::CmdSubst);
                    self.pos += 2;
                    let level = self.depth;
                    self.depth = self.depth.wrapping_add(1);
                    let mut inner = Ctx::new();
                    self.lex_list(Term::Paren, &mut inner);
                    self.depth = level;
                    if self.peek() == Some(b')') {
                        self.push(self.pos, self.pos + 1, Kind::CmdSubst);
                        self.pos += 1;
                    } else {
                        self.incomplete = true;
                    }
                }
            }
            Some(b'{') => {
                w.has_expansion = true;
                w.plain = false;
                // bash 5.3: `${ cmd; }` / `${| cmd; }`
                if matches!(self.peek_at(2), Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'|')) {
                    let mut hl = 2;
                    if self.peek_at(2) == Some(b'|') {
                        hl = 3;
                    }
                    self.push(start, start + hl, Kind::CmdSubst);
                    self.pos += hl;
                    let level = self.depth;
                    self.depth = self.depth.wrapping_add(1);
                    let mut inner = Ctx::new();
                    self.lex_list(Term::Brace, &mut inner);
                    self.depth = level;
                    if self.peek() == Some(b'}') {
                        self.push(self.pos, self.pos + 1, Kind::CmdSubst);
                        self.pos += 1;
                    } else {
                        self.incomplete = true;
                    }
                    return;
                }
                self.pos += 2;
                let mut depth = 1;
                while let Some(c) = self.peek() {
                    match c {
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                self.pos += 1;
                                self.push(start, self.pos, vkind);
                                return;
                            }
                        }
                        b'\\' => {
                            self.pos += 1;
                        }
                        b'\'' if !dq => {
                            self.pos += 1;
                            while let Some(d) = self.peek() {
                                self.pos += 1;
                                if d == b'\'' {
                                    break;
                                }
                            }
                            continue;
                        }
                        b'"' => {
                            // nested double quotes inside ${...}
                            self.pos += 1;
                            while let Some(d) = self.peek() {
                                if d == b'\\' {
                                    self.pos += 2;
                                    continue;
                                }
                                self.pos += 1;
                                if d == b'"' {
                                    break;
                                }
                            }
                            continue;
                        }
                        b'$' if self.peek_at(1) == Some(b'(') => {
                            // command substitution inside parameter expansion
                            self.push(start, self.pos, vkind);
                            let mut dummy = Word {
                                start: self.pos,
                                end: self.pos,
                                first_span: 0,
                                text: Vec::new(),
                                has_expansion: false,
                                has_glob: false,
                                plain: false,
                                assign_eq: None,
                                starts_with_tilde: false,
                            };
                            self.lex_dollar(&mut dummy, dq);
                            let cont = self.pos;
                            // continue scanning; remaining part gets its own span
                            let save_start = cont;
                            let mut d2 = depth;
                            while let Some(e) = self.peek() {
                                match e {
                                    b'{' => d2 += 1,
                                    b'}' => {
                                        d2 -= 1;
                                        if d2 == 0 {
                                            self.pos += 1;
                                            self.push(save_start, self.pos, vkind);
                                            return;
                                        }
                                    }
                                    _ => {}
                                }
                                self.pos += 1;
                            }
                            self.push(save_start, self.pos, vkind);
                            self.incomplete = true;
                            return;
                        }
                        _ => {}
                    }
                    self.pos += self.char_len().max(1);
                }
                self.push(start, self.pos, vkind);
                self.incomplete = true;
            }
            Some(c) if is_name_start(c) => {
                w.has_expansion = true;
                w.plain = false;
                let mut p = self.pos + 2;
                while p < self.s.len() && is_name_char(self.s[p]) {
                    p += 1;
                }
                self.pos = p;
                self.push(start, p, vkind);
            }
            Some(c) if is_special_param(c) => {
                w.has_expansion = true;
                w.plain = false;
                self.pos += 2;
                self.push(start, self.pos, vkind);
            }
            _ => {
                // lone `$`
                w.text.push(b'$');
                self.pos += 1;
                self.push(start, self.pos, if dq { Kind::DoubleQuoted } else { Kind::Default });
            }
        }
    }

    // ----- classification -------------------------------------------------------

    fn set_plain_kind(&mut self, w: &Word, kind: Kind) {
        for sp in &mut self.spans[w.first_span..] {
            if sp.kind == Kind::Default && sp.start >= w.start && sp.end <= w.end {
                sp.kind = kind;
            }
        }
    }

    fn classify(&mut self, w: Word, ctx: &mut Ctx) {
        let text = std::str::from_utf8(&w.text).unwrap_or("");
        let raw = std::str::from_utf8(&self.s[w.start..w.end]).unwrap_or("");

        // Things we were told to expect.
        match ctx.expect {
            Expect::RedirTarget => {
                ctx.expect = Expect::None;
                let kind = if !w.has_expansion && !w.has_glob && !text.is_empty() && self.lookup.path_exists(text) {
                    Kind::Path
                } else {
                    Kind::Argument
                };
                self.set_plain_kind(&w, kind);
                return;
            }
            Expect::HeredocDelim(strip) => {
                ctx.expect = Expect::None;
                self.set_plain_kind(&w, Kind::HeredocDelim);
                // make quoted delimiters fully HeredocDelim too
                for sp in &mut self.spans[w.first_span..] {
                    if sp.start >= w.start && sp.end <= w.end {
                        sp.kind = Kind::HeredocDelim;
                    }
                }
                self.heredocs.push(Heredoc { delim: w.text.clone(), strip_tabs: strip });
                return;
            }
            Expect::ForVar => {
                ctx.expect = Expect::ForIn;
                self.set_plain_kind(&w, Kind::Argument);
                return;
            }
            Expect::ForIn => {
                ctx.expect = Expect::None;
                if w.plain && text == "in" {
                    self.set_plain_kind(&w, Kind::Keyword);
                    ctx.for_words = true;
                    ctx.cmdpos = false;
                    return;
                }
                if w.plain && text == "do" {
                    self.set_plain_kind(&w, Kind::Keyword);
                    ctx.new_command();
                    return;
                }
                // fallthrough to normal classification
            }
            Expect::CaseWord => {
                ctx.expect = Expect::CaseIn;
                self.classify_argument(&w, text, ctx);
                return;
            }
            Expect::CaseIn => {
                ctx.expect = Expect::None;
                if w.plain && text == "in" {
                    self.set_plain_kind(&w, Kind::Keyword);
                    ctx.case_pattern = true;
                    ctx.cmdpos = false;
                    return;
                }
            }
            Expect::FuncName => {
                ctx.expect = Expect::None;
                self.set_plain_kind(&w, Kind::FuncDef);
                ctx.cmdpos = true;
                return;
            }
            Expect::PrecmdOptArg => {
                ctx.expect = Expect::None;
                self.set_plain_kind(&w, Kind::Argument);
                return;
            }
            Expect::None => {}
        }

        if ctx.case_pattern {
            if w.plain && text == "esac" {
                self.set_plain_kind(&w, Kind::Keyword);
                ctx.case_pattern = false;
                ctx.case_depth = ctx.case_depth.saturating_sub(1);
                ctx.cmdpos = false;
                return;
            }
            // pattern words: globs already marked, rest is argument
            self.set_plain_kind(&w, Kind::Argument);
            return;
        }

        if ctx.in_test {
            if w.plain && text == "]]" {
                self.set_plain_kind(&w, Kind::Keyword);
                ctx.in_test = false;
                ctx.cmdpos = false;
                return;
            }
            self.classify_argument(&w, text, ctx);
            return;
        }

        if ctx.for_words {
            self.classify_argument(&w, text, ctx);
            return;
        }

        if ctx.cmdpos {
            // assignment before the command
            if let Some(eq) = w.assign_eq {
                self.push_assignment(&w, eq, text);
                return;
            }
            if w.plain {
                if let Some(kw) = self.keyword_action(text, ctx) {
                    self.set_plain_kind(&w, Kind::Keyword);
                    kw(ctx);
                    return;
                }
                if text == "]]" {
                    self.set_plain_kind(&w, Kind::BracketError);
                    return;
                }
            }
            // precommand handling
            if let Some(argopts) = ctx.precmd {
                if !w.has_expansion && text.starts_with('-') && text.len() > 1 {
                    self.set_plain_kind(&w, Kind::Option);
                    if argopts.contains(&text) {
                        ctx.expect = Expect::PrecmdOptArg;
                    }
                    return;
                }
                if let Some(eq) = w.assign_eq {
                    self.push_assignment(&w, eq, text);
                    return;
                }
            }
            // the command word
            let kind = if w.has_expansion || w.has_glob {
                Kind::Default
            } else if text.is_empty() {
                Kind::Default
            } else if is_keyword(text) {
                // e.g. quoted keyword: bash treats it as a command name
                self.lookup.command_kind(text).unwrap_or(Kind::UnknownCommand)
            } else {
                self.lookup.command_kind(text).unwrap_or(Kind::UnknownCommand)
            };
            let kind = if w.plain && precommand_arg_options(text).is_some() && kind != Kind::UnknownCommand {
                Kind::Precommand
            } else {
                kind
            };
            self.set_plain_kind(&w, kind);
            if kind == Kind::Precommand {
                ctx.precmd = precommand_arg_options(text);
                ctx.cmdpos = true;
            } else {
                ctx.cmdpos = false;
                ctx.precmd = None;
            }
            let _ = raw;
            return;
        }

        self.classify_argument(&w, text, ctx);
    }

    fn push_assignment(&mut self, w: &Word, eq: usize, text: &str) {
        // NAME= part
        let name_end = w.start + eq + 1;
        // spans of the word that lie within the name part
        let mut tails: Vec<Span> = Vec::new();
        for sp in &mut self.spans[w.first_span..] {
            if sp.kind == Kind::Default && sp.start >= w.start && sp.end <= w.end {
                if sp.end <= name_end {
                    sp.kind = Kind::Assignment;
                } else if sp.start < name_end {
                    // split span
                    tails.push(Span { start: name_end, end: sp.end, kind: Kind::Argument });
                    sp.end = name_end;
                    sp.kind = Kind::Assignment;
                } else {
                    sp.kind = Kind::Argument;
                }
            }
        }
        self.spans.extend(tails);
        // value part path check
        if !w.has_expansion && !w.has_glob {
            if let Some(eqi) = text.find('=') {
                let val = &text[eqi + 1..];
                if !val.is_empty() && self.lookup.path_exists(val) {
                    for sp in &mut self.spans[w.first_span..] {
                        if sp.kind == Kind::Argument && sp.start >= name_end && sp.end <= w.end {
                            sp.kind = Kind::Path;
                        }
                    }
                }
            }
        }
        self.spans.sort_by_key(|s| s.start);
    }

    fn classify_argument(&mut self, w: &Word, text: &str, ctx: &mut Ctx) {
        if w.plain && text == "--" {
            ctx.dashdash = true;
            self.set_plain_kind(w, Kind::Option);
            return;
        }
        if !ctx.dashdash && !w.has_expansion && text.starts_with('-') && text.len() > 1 && w.plain {
            self.set_plain_kind(w, Kind::Option);
            return;
        }
        if ctx.in_test && w.plain && text.starts_with('-') && text.len() > 1 {
            self.set_plain_kind(w, Kind::Option);
            return;
        }
        if !w.has_expansion && !w.has_glob && !text.is_empty() && self.lookup.path_exists(text) {
            self.set_plain_kind(w, Kind::Path);
            for sp in &mut self.spans[w.first_span..] {
                if sp.kind == Kind::Tilde && sp.start >= w.start && sp.end <= w.end {
                    // keep tilde style
                }
            }
            return;
        }
        let _ = w.starts_with_tilde;
        self.set_plain_kind(w, Kind::Argument);
    }

    /// If `text` is a reserved word valid in command position, return the
    /// context update to apply.
    fn keyword_action(&self, text: &str, ctx: &Ctx) -> Option<fn(&mut Ctx)> {
        Some(match text {
            "if" | "while" | "until" | "then" | "else" | "elif" | "do" | "{" | "!" | "time" | "coproc" => {
                |c: &mut Ctx| {
                    c.cmdpos = true;
                    c.precmd = None;
                }
            }
            "fi" | "done" | "esac" | "}" => |c: &mut Ctx| {
                c.cmdpos = false;
            },
            "for" | "select" => |c: &mut Ctx| {
                c.expect = Expect::ForVar;
                c.cmdpos = false;
            },
            "case" => |c: &mut Ctx| {
                c.expect = Expect::CaseWord;
                c.case_depth += 1;
                c.cmdpos = false;
            },
            "function" => |c: &mut Ctx| {
                c.expect = Expect::FuncName;
                c.cmdpos = false;
            },
            "[[" => |c: &mut Ctx| {
                c.in_test = true;
                c.cmdpos = false;
            },
            "in" if ctx.case_depth > 0 => |c: &mut Ctx| {
                c.case_pattern = true;
                c.cmdpos = false;
            },
            _ => return None,
        })
    }
}

fn push_merge(out: &mut Vec<Span>, sp: Span) {
    if let Some(last) = out.last_mut() {
        if last.kind == sp.kind && last.end == sp.start {
            last.end = sp.end;
            return;
        }
    }
    out.push(sp);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct L;
    impl Lookup for L {
        fn command_kind(&self, name: &str) -> Option<Kind> {
            match name {
                "ls" | "grep" | "cat" | "sudo" | "env" | "git" | "make" | "/bin/ls" | "./run.sh" => Some(Kind::Command),
                "echo" | "cd" | "[" | "command" | "exec" | "builtin" => Some(Kind::Builtin),
                "ll" => Some(Kind::Alias),
                "myfn" => Some(Kind::Function),
                _ => None,
            }
        }
        fn path_exists(&self, p: &str) -> bool {
            matches!(p, "/etc" | "/etc/passwd" | "~/x" | "src" | "a b")
        }
    }

    fn kinds(src: &str) -> Vec<(String, Kind)> {
        lex(src, &L)
            .into_iter()
            .filter(|s| s.kind != Kind::Default || !src[s.start..s.end].trim().is_empty())
            .map(|s| (src[s.start..s.end].to_string(), s.kind))
            .collect()
    }

    fn find(src: &str, frag: &str) -> Kind {
        let off = src.find(frag).expect("fragment present");
        let spans = lex(src, &L);
        for s in &spans {
            if s.start <= off && off < s.end {
                return s.kind;
            }
        }
        panic!("no span for {frag:?} in {src:?}: {spans:?}");
    }

    #[test]
    fn covers_everything() {
        for src in ["", "ls", "ls -la /etc | grep x", "echo \"a $b\" 'c' $(ls) `x`", "a=1 b=2 ls", "\"unterminated"] {
            let spans = lex(src, &L);
            let mut cur = 0;
            for s in &spans {
                assert_eq!(s.start, cur, "gap in {src:?}: {spans:?}");
                assert!(s.end > s.start);
                cur = s.end;
            }
            assert_eq!(cur, src.len());
        }
    }

    #[test]
    fn commands() {
        assert_eq!(find("ls -la", "ls"), Kind::Command);
        assert_eq!(find("nope -la", "nope"), Kind::UnknownCommand);
        assert_eq!(find("echo hi", "echo"), Kind::Builtin);
        assert_eq!(find("ll", "ll"), Kind::Alias);
        assert_eq!(find("myfn x", "myfn"), Kind::Function);
        assert_eq!(find("/bin/ls", "/bin/ls"), Kind::Command);
        assert_eq!(find("ls -la", "-la"), Kind::Option);
        assert_eq!(find("ls /etc", "/etc"), Kind::Path);
        assert_eq!(find("ls /nope", "/nope"), Kind::Argument);
        assert_eq!(find("ls | grep x", "grep"), Kind::Command);
        assert_eq!(find("ls | grep x", "|"), Kind::Separator);
        assert_eq!(find("ls && nope", "nope"), Kind::UnknownCommand);
        assert_eq!(find("ls; nope", "nope"), Kind::UnknownCommand);
        assert_eq!(find("ls -- -x", "-x"), Kind::Argument);
    }

    #[test]
    fn precommands() {
        assert_eq!(find("sudo ls", "sudo"), Kind::Precommand);
        assert_eq!(find("sudo ls", "ls"), Kind::Command);
        assert_eq!(find("sudo -u root ls", "-u"), Kind::Option);
        assert_eq!(find("sudo -u root ls", "root"), Kind::Argument);
        assert_eq!(find("sudo -u root ls", "ls"), Kind::Command);
        assert_eq!(find("env FOO=1 nope", "FOO="), Kind::Assignment);
        assert_eq!(find("env FOO=1 nope", "nope"), Kind::UnknownCommand);
        assert_eq!(find("command -v ls", "ls"), Kind::Command);
    }

    #[test]
    fn quotes_and_expansions() {
        assert_eq!(find("echo 'a b'", "'a b'"), Kind::SingleQuoted);
        assert_eq!(find("echo \"a $b c\"", "\"a "), Kind::DoubleQuoted);
        assert_eq!(find("echo \"a $b c\"", "$b"), Kind::VariableDq);
        assert_eq!(find("echo \"a \\$b\"", "\\$"), Kind::EscapeDq);
        assert_eq!(find("echo $HOME", "$HOME"), Kind::Variable);
        assert_eq!(find("echo ${HOME:-x}", "${HOME:-x}"), Kind::Variable);
        assert_eq!(find("echo $'a\\n'", "$'a"), Kind::DollarQuoted);
        assert_eq!(find("echo $(ls -l)", "$("), Kind::CmdSubst);
        assert_eq!(find("echo $(ls -l)", "ls"), Kind::Command);
        assert_eq!(find("echo $(nope)", "nope"), Kind::UnknownCommand);
        assert_eq!(find("echo $((1+2))", "$(("), Kind::ArithDelim);
        assert_eq!(find("echo $((1+2))", "1+2"), Kind::Arith);
        assert_eq!(find("echo `ls`", "ls"), Kind::Command);
        assert_eq!(find("echo a\\ b", "\\ "), Kind::Escape);
        assert_eq!(find("echo ~/x", "~"), Kind::Tilde);
        assert_eq!(find("echo ~/x", "/x"), Kind::Path);
        assert_eq!(find("echo !!", "!!"), Kind::HistoryExp);
        assert_eq!(find("echo !$", "!$"), Kind::HistoryExp);
        assert_eq!(find("echo *.rs", "*"), Kind::Glob);
        assert_eq!(find("echo [abc]x", "[abc]"), Kind::Glob);
        assert_eq!(find("echo {a,b}", "{a,b}"), Kind::Brace);
        assert_eq!(find("echo {1..3}", "{1..3}"), Kind::Brace);
        assert_eq!(find("echo x # hi", "# hi"), Kind::Comment);
        assert_eq!(find("echo x#y", "x#y"), Kind::Argument);
    }

    #[test]
    fn redirections() {
        assert_eq!(find("ls > out", ">"), Kind::Redirect);
        assert_eq!(find("ls > /etc", "/etc"), Kind::Path);
        assert_eq!(find("ls 2>&1", "2>&1"), Kind::Redirect);
        assert_eq!(find("ls &> out", "&>"), Kind::Redirect);
        assert_eq!(find("cat <<< str", "<<<"), Kind::Redirect);
        assert_eq!(find("diff <(ls) <(ls -a)", "<("), Kind::ProcSubst);
        assert_eq!(find("diff <(ls) <(ls -a)", "ls -a"), Kind::Command);
        assert_eq!(find("cat <<EOF\nbody\nEOF\n", "EOF\n"), Kind::HeredocDelim);
        assert_eq!(find("cat <<EOF\nbody\nEOF\n", "body"), Kind::HeredocBody);
        assert_eq!(find("cat <<'EOF'\nbody\nEOF\n", "body"), Kind::HeredocBody);
        assert_eq!(find("cat <<-EOF\n\tbody\n\tEOF\n", "body"), Kind::HeredocBody);
        assert_eq!(find("cat <<EOF | grep x\nbody\nEOF\n", "grep"), Kind::Command);
    }

    #[test]
    fn keywords_and_structures() {
        assert_eq!(find("if ls; then echo; fi", "if"), Kind::Keyword);
        assert_eq!(find("if ls; then echo; fi", "ls"), Kind::Command);
        assert_eq!(find("if ls; then echo; fi", "then"), Kind::Keyword);
        assert_eq!(find("if ls; then echo; fi", "echo"), Kind::Builtin);
        assert_eq!(find("for x in a b; do echo $x; done", "for"), Kind::Keyword);
        assert_eq!(find("for x in a b; do echo $x; done", "x in"), Kind::Argument);
        assert_eq!(find("for x in a b; do echo $x; done", "in"), Kind::Keyword);
        assert_eq!(find("for x in a b; do echo $x; done", "a b"), Kind::Argument);
        assert_eq!(find("for x in a b; do echo $x; done", "do"), Kind::Keyword);
        assert_eq!(find("for x in a b; do echo $x; done", "echo"), Kind::Builtin);
        assert_eq!(find("while true; do ls; done", "while"), Kind::Keyword);
        assert_eq!(find("case $x in a|b) ls;; *) nope;; esac", "case"), Kind::Keyword);
        assert_eq!(find("case $x in a|b) ls;; *) nope;; esac", "in"), Kind::Keyword);
        assert_eq!(find("case $x in a|b) ls;; *) nope;; esac", "ls"), Kind::Command);
        assert_eq!(find("case $x in a|b) ls;; *) nope;; esac", "nope"), Kind::UnknownCommand);
        assert_eq!(find("case $x in a|b) ls;; *) nope;; esac", "esac"), Kind::Keyword);
        assert_eq!(find("[[ -f x ]] && ls", "[["), Kind::Keyword);
        assert_eq!(find("[[ -f x ]] && ls", "-f"), Kind::Option);
        assert_eq!(find("[[ -f x ]] && ls", "]]"), Kind::Keyword);
        assert_eq!(find("[[ -f x ]] && ls", "ls"), Kind::Command);
        assert_eq!(find("[ -f x ] && ls", "["), Kind::Builtin);
        assert_eq!(find("f() { ls; }", "f"), Kind::FuncDef);
        assert_eq!(find("f() { ls; }", "{"), Kind::Keyword);
        assert_eq!(find("f() { ls; }", "ls"), Kind::Command);
        assert_eq!(find("f() { ls; }", "}"), Kind::Keyword);
        assert_eq!(find("function f { ls; }", "f {"), Kind::FuncDef);
        assert_eq!(find("(ls)", "("), Kind::Bracket(0));
        assert_eq!(find("(ls)", "ls"), Kind::Command);
        assert_eq!(find("{ ls; }", "ls"), Kind::Command);
        assert_eq!(find("! ls", "!"), Kind::Keyword);
        assert_eq!(find("time ls", "time"), Kind::Keyword);
        assert_eq!(find("time ls", "ls"), Kind::Command);
        assert_eq!(find("x=1 ls", "x="), Kind::Assignment);
        assert_eq!(find("x+=1 ls", "x+="), Kind::Assignment);
        assert_eq!(find("a[1]=2 ls", "a[1]="), Kind::Assignment);
        assert_eq!(find("x=/etc ls", "/etc"), Kind::Path);
        assert_eq!(find("ls x=1", "x=1"), Kind::Argument);
        assert_eq!(find("ls )", ")"), Kind::BracketError);
        assert_eq!(find("((i++))", "(("), Kind::ArithDelim);
        assert_eq!(find("((i++))", "i++"), Kind::Arith);
    }

    #[test]
    fn continuation_and_newlines() {
        assert_eq!(find("ls \\\n -l", "-l"), Kind::Option);
        assert_eq!(find("ls\nnope", "nope"), Kind::UnknownCommand);
        assert_eq!(find("echo \"a\nb\"", "b\""), Kind::DoubleQuoted);
    }

    #[test]
    fn incomplete_detection() {
        let inc = |s: &str| Lexer::new(s, &L).run().1;
        assert!(inc("echo \"a"));
        assert!(inc("echo $(ls"));
        assert!(inc("cat <<EOF\nfoo"));
        assert!(!inc("ls -la"));
        assert!(!inc("echo \"a\""));
    }

    #[test]
    fn unicode() {
        let src = "echo 'héllo wörld' ünïcode";
        let spans = lex(src, &L);
        assert_eq!(spans.last().unwrap().end, src.len());
        assert_eq!(find(src, "ünïcode"), Kind::Argument);
    }

    #[test]
    fn dump() {
        let k = kinds("sudo -u root ls -la \"$HOME/x\" 2>&1 | grep -v 'a' # c");
        assert!(k.iter().any(|(t, kd)| t == "sudo" && *kd == Kind::Precommand));
        assert!(k.iter().any(|(t, kd)| t == "# c" && *kd == Kind::Comment));
    }
}
