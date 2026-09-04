//! The per-shell daemon. It talks to bash over a FIFO (bash → daemon) and a
//! pipe (daemon → bash), paints highlighted text / suggestions / completion
//! lists directly onto the terminal, and nudges bash with `SIGWINCH` so that
//! painting happens from a trap while readline is idle (after it has redrawn
//! the line itself).
//!
//! Protocol (all fields are NUL-terminated, first field is the message type):
//!
//! | msg | fields                                   | meaning                              |
//! |-----|------------------------------------------|--------------------------------------|
//! | i   | pid home histfile styles rows opts       | init                                 |
//! | p   | ps1 ps2 pwd path reserved                | a new prompt is about to be shown    |
//! | H   | text                                     | last history entry                   |
//! | l/e | line point                               | line changed (insert / edit)         |
//! | a   | –                                        | new readline call after accept-line  |
//! | t/T | line point                               | Tab / Shift-Tab (expects reply)      |
//! | d   | –                                        | bash finished a reply-based hook     |
//! | g   | –                                        | paint now (from the WINCH trap)      |
//! | r   | –                                        | signal again later                   |
//! | A/F | names (newline separated)                | aliases / functions                  |
//! | V   | `bind -v` output                         | readline variables                   |
//! | x   | –                                        | exit                                 |
//!
//! Replies: `t`/`T` → `I\0line\0point\0` or `N\0`; `g` → one byte
//! (`y` if something was drawn below the line, else `n`).

use crate::complete::{self, CommandIndex, History};
use crate::layout::{self, Layout};
use crate::lexer::{self, Kind, Lookup, Span};
use crate::render::{self, Frame, ListEntry, ListGroup};
use crate::style::Theme;
use crate::sys;
use std::fs::File;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::time::Instant;

const SIGNAL_WAIT_US: u64 = 4000;

/// Buffered reader over the FIFO with NUL-delimited fields.
struct Chan {
    fd: RawFd,
    buf: Vec<u8>,
    pos: usize,
    eof: bool,
    ppid: i32,
    /// Set once the first byte arrived; before that, EOF just means bash
    /// has not opened its end of the FIFO yet.
    connected: bool,
}

impl Chan {
    fn new(fd: RawFd, ppid: i32) -> Self {
        Chan { fd, buf: Vec::with_capacity(8192), pos: 0, eof: false, ppid, connected: false }
    }

    fn fill(&mut self, block: bool) -> bool {
        if self.eof {
            return false;
        }
        if self.pos > 0 && self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        } else if self.pos > 65536 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        loop {
            if !sys::poll_readable(self.fd, if block { 1000 } else { 0 }) {
                if !block {
                    return false;
                }
                // periodic liveness check of the parent shell
                if sys::getppid() != self.ppid {
                    self.eof = true;
                    return false;
                }
                continue;
            }
            let mut tmp = [0u8; 8192];
            match sys::read_some(self.fd, &mut tmp) {
                Ok(0) => {
                    if !self.connected {
                        if !block {
                            return false;
                        }
                        if sys::getppid() != self.ppid {
                            self.eof = true;
                            return false;
                        }
                        sys::nap_us(5000);
                        continue;
                    }
                    self.eof = true;
                    return false;
                }
                Ok(n) => {
                    self.connected = true;
                    self.buf.extend_from_slice(&tmp[..n]);
                    return true;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if !block {
                        return false;
                    }
                    continue;
                }
                Err(_) => {
                    self.eof = true;
                    return false;
                }
            }
        }
    }

    /// Next NUL-terminated field (blocking). `None` on EOF.
    fn field(&mut self) -> Option<Vec<u8>> {
        loop {
            if let Some(off) = self.buf[self.pos..].iter().position(|&b| b == 0) {
                let f = self.buf[self.pos..self.pos + off].to_vec();
                self.pos += off + 1;
                return Some(f);
            }
            if !self.fill(true) {
                return None;
            }
        }
    }

    fn field_str(&mut self) -> Option<String> {
        self.field().map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    /// Whether another message is already waiting.
    fn pending(&mut self) -> bool {
        if self.pos < self.buf.len() {
            return true;
        }
        self.fill(false)
    }
}

struct ReadlineVars {
    show_mode: bool,
    emacs_mode_string: String,
    vi_ins_mode_string: String,
    vi_mode: bool,
    ignore_case: bool,
}

impl Default for ReadlineVars {
    fn default() -> Self {
        ReadlineVars {
            show_mode: false,
            emacs_mode_string: "@".into(),
            vi_ins_mode_string: "(ins)".into(),
            vi_mode: false,
            ignore_case: false,
        }
    }
}

impl ReadlineVars {
    fn parse(&mut self, text: &str) {
        for line in text.lines() {
            let line = line.trim();
            let rest = match line.strip_prefix("set ") {
                Some(r) => r,
                None => continue,
            };
            let (name, val) = match rest.split_once(' ') {
                Some(x) => x,
                None => continue,
            };
            let val = val.trim();
            match name {
                "show-mode-in-prompt" => self.show_mode = val == "on",
                "editing-mode" => self.vi_mode = val == "vi",
                "completion-ignore-case" => self.ignore_case = val == "on",
                "emacs-mode-string" => self.emacs_mode_string = unescape_rl(val),
                "vi-ins-mode-string" => self.vi_ins_mode_string = unescape_rl(val),
                _ => {}
            }
        }
    }

    fn modmark(&self) -> usize {
        if !self.show_mode {
            return 0;
        }
        let s = if self.vi_mode { &self.vi_ins_mode_string } else { &self.emacs_mode_string };
        layout::prompt_width(s)
    }
}

/// Undo the escaping `bind -v` uses for mode strings.
fn unescape_rl(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('1') => out.push('\x01'),
                Some('2') => out.push('\x02'),
                Some('e') => out.push('\x1b'),
                Some('n') => out.push('\n'),
                Some(o) => {
                    out.push('\\');
                    out.push(o);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

struct Env<'a> {
    cmds: &'a CommandIndex,
    cwd: &'a PathBuf,
    home: &'a str,
}

impl<'a> Lookup for Env<'a> {
    fn command_kind(&self, name: &str) -> Option<Kind> {
        if name.contains('/') {
            return if complete::is_executable(name, self.cwd, self.home) { Some(Kind::Command) } else { None };
        }
        self.cmds.kind_of(name)
    }
    fn path_exists(&self, path: &str) -> bool {
        complete::path_exists(path, self.cwd, self.home)
    }
}

/// One completion candidate, with everything needed to insert it.
#[derive(Clone, Debug)]
struct Cand {
    /// Text shown in the list.
    display: String,
    /// Unquoted text that replaces the current word (or the whole line).
    insert: String,
    whole_line: bool,
    is_dir: bool,
    /// Append a space after inserting.
    space: bool,
}

struct Menu {
    cands: Vec<Cand>,
    idx: Option<usize>,
    /// Line/point the menu produced last; if bash reports something else
    /// the user typed and the menu is gone.
    line: String,
    point: usize,
    word_start: usize,
    /// Quote character the word started with, if any.
    quote: Option<char>,
    /// Chars of the original word (for the bold match prefix).
    match_len: usize,
}

pub struct Daemon {
    tty: File,
    chan: Chan,
    bash_pid: i32,
    theme: Theme,
    list_rows: usize,
    suggestions: bool,
    show_list: bool,
    rlvars: ReadlineVars,
    ps1: String,
    ps2: String,
    reserved: usize,
    cwd: PathBuf,
    home: String,
    cmds: CommandIndex,
    history: History,
    line: String,
    point: usize,
    at_prompt: bool,
    ps2_mode: bool,
    ps2_ctx: String,
    rows_above: usize,
    seen_prompt_since_accept: bool,
    drew_below: bool,
    menu: Option<Menu>,
    last_layout_rows: usize,
    last_end_col: usize,
    /// Cached frame for (line, point, size).
    frame: Vec<u8>,
    frame_key: (String, usize, usize, usize, bool),
    frame_valid: bool,
    /// The list is only shown while the user is typing (not right after a
    /// new prompt), and can be toggled off by the accept hook.
    list_visible: bool,
    log: Option<File>,
}

pub fn run() -> ! {
    // Signals: we live in bash's process group, so C-c etc. reach us too.
    for sig in [libc::SIGINT, libc::SIGQUIT, libc::SIGTSTP, libc::SIGTTIN, libc::SIGTTOU, libc::SIGWINCH, libc::SIGPIPE] {
        sys::ignore_signal(sig);
    }
    sys::die_with_parent();
    let ppid = sys::getppid();

    let tty = match sys::open_tty() {
        Some(t) => t,
        None => {
            eprintln!("bash-tools: cannot open terminal");
            std::process::exit(1);
        }
    };

    // Create the FIFO, announce it, then open it (blocks until bash connects).
    let dir = sys::runtime_dir();
    let path = format!("{}/bash-tools-{}-{}.fifo", dir, sys::getpid(), ppid);
    let _ = std::fs::remove_file(&path);
    if let Err(e) = sys::mkfifo(&path) {
        eprintln!("bash-tools: mkfifo {path}: {e}");
        std::process::exit(1);
    }
    {
        use std::io::Write;
        let mut so = std::io::stdout();
        let _ = writeln!(so, "{} {}", path, sys::getpid());
        let _ = so.flush();
    }
    // Non-blocking open so that a shell which never connects cannot leave
    // us stuck in open(2) forever; Chan handles the not-yet-connected state.
    let fifo = match sys::open_fifo_reader(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("bash-tools: open fifo: {e}");
            let _ = std::fs::remove_file(&path);
            std::process::exit(1);
        }
    };

    let log = std::env::var("BASH_TOOLS_DEBUG").ok().and_then(|p| {
        if p.is_empty() {
            None
        } else {
            std::fs::OpenOptions::new().create(true).append(true).open(p).ok()
        }
    });

    let mut d = Daemon {
        tty,
        chan: Chan::new(fifo.as_raw_fd(), ppid),
        bash_pid: ppid,
        theme: Theme::default(),
        list_rows: 8,
        suggestions: true,
        show_list: true,
        rlvars: ReadlineVars::default(),
        ps1: String::new(),
        ps2: "> ".into(),
        reserved: 0,
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
        home: std::env::var("HOME").unwrap_or_default(),
        cmds: CommandIndex::default(),
        history: History::default(),
        line: String::new(),
        point: 0,
        at_prompt: false,
        ps2_mode: false,
        ps2_ctx: String::new(),
        rows_above: 0,
        seen_prompt_since_accept: true,
        drew_below: false,
        menu: None,
        last_layout_rows: 1,
        last_end_col: 0,
        frame: Vec::new(),
        frame_key: (String::new(), 0, 0, 0, false),
        frame_valid: false,
        list_visible: false,
        log,
    };
    // keep the FIFO File alive for the daemon's lifetime
    let _fifo = fifo;
    d.main_loop();
    std::process::exit(0)
}

impl Daemon {
    fn debug(&mut self, msg: &str) {
        if let Some(f) = &mut self.log {
            use std::io::Write;
            let _ = writeln!(f, "{:?} {}", Instant::now(), msg);
        }
    }

    fn main_loop(&mut self) {
        while let Some(t) = self.chan.field() {
            let ty = t.first().copied().unwrap_or(0);
            match ty {
                b'i' => self.on_init(),
                b'p' => self.on_prompt(),
                b'H' => {
                    if let Some(h) = self.chan.field_str() {
                        let h = h.trim_start_matches([' ', '\t']).trim_end_matches('\n');
                        self.history.push(h);
                    }
                }
                b'l' | b'e' => {
                    let line = self.chan.field_str().unwrap_or_default();
                    let point = self.chan.field_str().unwrap_or_default().trim().parse().unwrap_or(0);
                    self.on_line(line, point, ty == b'l');
                }
                b'a' => self.on_accept(),
                b't' | b'T' => {
                    let line = self.chan.field_str().unwrap_or_default();
                    let point = self.chan.field_str().unwrap_or_default().trim().parse().unwrap_or(0);
                    self.on_tab(line, point, ty == b'T');
                }
                b'd' => self.request_paint(),
                b'g' => self.on_go(),
                b'r' => self.request_paint(),
                b'A' => {
                    let s = self.chan.field_str().unwrap_or_default();
                    self.cmds.set_aliases(s.lines().map(|l| l.to_string()));
                }
                b'F' => {
                    let s = self.chan.field_str().unwrap_or_default();
                    self.cmds.set_functions(s.lines().map(|l| l.to_string()));
                }
                b'V' => {
                    let s = self.chan.field_str().unwrap_or_default();
                    self.rlvars.parse(&s);
                }
                b'x' => return,
                _ => {
                    self.debug(&format!("unknown message {:?}", t));
                }
            }
        }
    }

    fn reply(&mut self, data: &[u8]) {
        use std::io::Write;
        let mut so = std::io::stdout().lock();
        let _ = so.write_all(data);
        let _ = so.flush();
    }

    // ----- message handlers ---------------------------------------------------

    fn on_init(&mut self) {
        let pid: i32 = self.chan.field_str().unwrap_or_default().trim().parse().unwrap_or(0);
        if pid > 0 {
            self.bash_pid = pid;
        }
        let home = self.chan.field_str().unwrap_or_default();
        if !home.is_empty() {
            self.home = home;
        }
        let histfile = self.chan.field_str().unwrap_or_default();
        let styles = self.chan.field_str().unwrap_or_default();
        let rows = self.chan.field_str().unwrap_or_default();
        let opts = self.chan.field_str().unwrap_or_default();
        self.theme.apply(&styles);
        self.list_rows = rows.trim().parse().unwrap_or(8);
        for o in opts.split(',') {
            match o.trim() {
                "nosuggest" => self.suggestions = false,
                "nolist" => self.show_list = false,
                _ => {}
            }
        }
        let hf = if histfile.is_empty() { format!("{}/.bash_history", self.home) } else { histfile };
        self.history.load(&hf);
        self.debug(&format!("init pid={} home={} hist={} entries={}", self.bash_pid, self.home, hf, self.history.len()));
    }

    fn on_prompt(&mut self) {
        self.ps1 = self.chan.field_str().unwrap_or_default();
        self.ps2 = self.chan.field_str().unwrap_or_default();
        let pwd = self.chan.field_str().unwrap_or_default();
        let path = self.chan.field_str().unwrap_or_default();
        self.reserved = self.chan.field_str().unwrap_or_default().trim().parse().unwrap_or(0);
        if !pwd.is_empty() {
            self.cwd = PathBuf::from(pwd);
        }
        self.cmds.refresh_path(&path, false);
        self.history.reload_if_grown();
        self.at_prompt = true;
        self.ps2_mode = false;
        self.ps2_ctx.clear();
        self.rows_above = 0;
        self.seen_prompt_since_accept = true;
        self.line.clear();
        self.point = 0;
        self.menu = None;
        self.drew_below = false;
        self.list_visible = false;
        self.frame_valid = false;
    }

    fn on_accept(&mut self) {
        if self.seen_prompt_since_accept {
            // normal: a fresh PS1 prompt (on_prompt already reset state)
            self.seen_prompt_since_accept = false;
            return;
        }
        // No PROMPT_COMMAND ran: bash asked for a continuation line (PS2).
        let consumed = if self.last_end_col == 0 && self.last_layout_rows > 1 {
            self.last_layout_rows - 1
        } else {
            self.last_layout_rows
        };
        let prompt = if self.ps2_mode { self.ps2.clone() } else { self.ps1.clone() };
        let (cols, _) = self.size();
        self.rows_above += layout::prompt_prefix_rows(&prompt, cols) + consumed;
        self.ps2_ctx.push_str(&self.line);
        self.ps2_ctx.push('\n');
        self.ps2_mode = true;
        self.line.clear();
        self.point = 0;
        self.menu = None;
        self.list_visible = false;
        self.frame_valid = false;
        // the accepted line is part of the command, not history yet
    }

    fn on_line(&mut self, line: String, point: usize, _insert: bool) {
        if let Some(m) = &self.menu {
            if m.line != line || m.point != point {
                self.menu = None;
            }
        }
        self.line = line;
        self.point = point;
        self.at_prompt = true;
        self.list_visible = !self.line.is_empty();
        self.frame_valid = false;
        self.compute_frame();
        self.request_paint();
    }

    fn on_go(&mut self) {
        let mut ack = b'n';
        if self.at_prompt && !self.line.is_empty() {
            self.compute_frame();
            let frame = std::mem::take(&mut self.frame);
            let _ = sys::write_all(self.tty.as_raw_fd(), &frame);
            self.frame = frame;
            if self.drew_below {
                ack = b'y';
            }
        } else if self.at_prompt && self.drew_below {
            // line became empty: erase whatever we drew below
            let (cols, _) = self.size();
            let pw = self.prompt_cols();
            let lay = layout::layout("", 0, pw, cols);
            let mut out = Vec::new();
            let f = Frame {
                theme: &self.theme,
                text: "",
                spans: &[],
                layout: &lay,
                suggestion: "",
                groups: &[],
                selected: None,
                avail_rows: 0,
                clear_below: true,
            };
            render::render(&f, &mut out);
            let _ = sys::write_all(self.tty.as_raw_fd(), &out);
            self.drew_below = false;
        }
        self.reply(&[ack]);
    }

    fn on_tab(&mut self, line: String, point: usize, backward: bool) {
        self.at_prompt = true;
        let reuse = matches!(&self.menu, Some(m) if m.line == line && m.point == point);
        if !reuse {
            self.line = line.clone();
            self.point = point;
            self.menu = self.build_menu();
        }
        let m = match &mut self.menu {
            Some(m) if !m.cands.is_empty() => m,
            _ => {
                self.menu = None;
                self.reply(b"N\0");
                return;
            }
        };
        let n = m.cands.len();
        let first_time = !reuse;
        let mut result: Option<(String, usize)> = None;
        if first_time {
            if n == 1 {
                m.idx = Some(0);
            } else {
                // common prefix of the word candidates
                let cp = complete::common_prefix(m.cands.iter().filter(|c| !c.whole_line).map(|c| c.insert.as_str()));
                let typed_len = m.match_len;
                if cp.chars().count() > typed_len && !backward {
                    // insert the common prefix, keep menu open without selection
                    let quote = m.quote;
                    let prefix_line: String = line.chars().take(m.word_start).collect();
                    let rest: String = line.chars().skip(point).collect();
                    let ins = complete::quote_for_insert(&cp, quote);
                    let ins = match quote {
                        Some(q) => format!("{q}{ins}"),
                        None => ins,
                    };
                    let new_line = format!("{prefix_line}{ins}{rest}");
                    let new_point = m.word_start + ins.chars().count();
                    m.match_len = cp.chars().count();
                    m.line = new_line.clone();
                    m.point = new_point;
                    result = Some((new_line, new_point));
                } else {
                    m.idx = Some(if backward { n - 1 } else { 0 });
                }
            }
        } else {
            m.idx = Some(match m.idx {
                None => {
                    if backward {
                        n - 1
                    } else {
                        0
                    }
                }
                Some(i) => {
                    if backward {
                        (i + n - 1) % n
                    } else {
                        (i + 1) % n
                    }
                }
            });
        }
        if result.is_none() {
            let i = m.idx.unwrap();
            let c = &m.cands[i];
            let (new_line, new_point) = if c.whole_line {
                (c.insert.clone(), c.insert.chars().count())
            } else {
                let prefix_line: String = m.line.chars().take(m.word_start).collect();
                // the rest of the line after the word being completed: we
                // only ever replace up to the original point
                let rest: String = line.chars().skip(point).collect();
                let mut ins = complete::quote_for_insert(&c.insert, m.quote);
                if let Some(q) = m.quote {
                    ins.insert(0, q);
                    if c.space {
                        ins.push(q);
                    }
                }
                if c.space && !c.is_dir {
                    ins.push(' ');
                }
                let np = m.word_start + ins.chars().count();
                (format!("{prefix_line}{ins}{rest}"), np)
            };
            m.line = new_line.clone();
            m.point = new_point;
            result = Some((new_line, new_point));
        }
        let (new_line, new_point) = result.unwrap();
        self.line = new_line.clone();
        self.point = new_point;
        self.list_visible = true;
        self.frame_valid = false;
        let mut r = Vec::with_capacity(new_line.len() + 16);
        r.extend_from_slice(b"I\0");
        r.extend_from_slice(new_line.as_bytes());
        r.push(0);
        r.extend_from_slice(new_point.to_string().as_bytes());
        r.push(0);
        self.reply(&r);
        self.compute_frame();
    }

    // ----- painting -------------------------------------------------------------

    fn size(&self) -> (usize, usize) {
        sys::winsize(self.tty.as_raw_fd()).unwrap_or((80, 24))
    }

    fn prompt_cols(&self) -> usize {
        let p = if self.ps2_mode { &self.ps2 } else { &self.ps1 };
        let (_, last) = layout::split_prompt(p);
        layout::prompt_width(last) + self.rlvars.modmark()
    }

    fn avail_rows(&self, lay: &Layout, cols: usize) -> usize {
        let p = if self.ps2_mode { &self.ps2 } else { &self.ps1 };
        let above = self.rows_above + layout::prompt_prefix_rows(p, cols);
        self.reserved.saturating_sub(above + lay.rows - 1)
    }

    /// Highlight spans for the current line (taking PS2 context into account).
    fn spans(&self) -> Vec<Span> {
        let env = Env { cmds: &self.cmds, cwd: &self.cwd, home: &self.home };
        if self.ps2_ctx.is_empty() {
            return lexer::lex(&self.line, &env);
        }
        let full = format!("{}{}", self.ps2_ctx, self.line);
        let off = self.ps2_ctx.len();
        lexer::lex(&full, &env)
            .into_iter()
            .filter(|s| s.end > off)
            .map(|s| Span { start: s.start.saturating_sub(off), end: s.end - off, kind: s.kind })
            .collect()
    }

    fn compute_frame(&mut self) {
        let (cols, rows) = self.size();
        let key = (self.line.clone(), self.point, cols, rows, self.menu.is_some());
        if self.frame_valid && self.frame_key == key {
            return;
        }
        let spans = self.spans();
        let pw = self.prompt_cols();
        let lay = layout::layout(&self.line, self.point, pw, cols);
        self.last_layout_rows = lay.rows;
        self.last_end_col = lay.end_col;
        let avail = self.avail_rows(&lay, cols);

        // suggestion
        let mut suggestion = String::new();
        if self.suggestions && !self.ps2_mode && self.point == self.line.chars().count() && !self.line.is_empty() && self.menu.is_none() {
            if let Some(s) = self.history.suggest(&self.line) {
                suggestion = s[self.line.len()..].to_string();
            }
        }

        // completion list
        let mut groups: Vec<ListGroup> = Vec::new();
        let mut selected = None;
        if self.show_list && self.list_visible && avail > 0 && self.list_rows > 0 {
            if let Some(m) = &self.menu {
                groups = cands_to_groups(&m.cands, m.match_len, self.line.chars().count());
                selected = m.idx;
            } else {
                let (cands, match_len) = self.candidates();
                groups = cands_to_groups(&cands, match_len, self.line.chars().count());
            }
        }

        let f = Frame {
            theme: &self.theme,
            text: &self.line,
            spans: &spans,
            layout: &lay,
            suggestion: &suggestion,
            groups: &groups,
            selected,
            avail_rows: avail,
            clear_below: self.drew_below,
        };
        let mut out = Vec::with_capacity(self.line.len() * 3 + 256);
        self.drew_below = render::render(&f, &mut out);
        self.frame = out;
        self.frame_key = key;
        self.frame_valid = true;
    }

    /// Ask bash to run the WINCH trap once it is idle in `read()`.
    fn request_paint(&mut self) {
        if !self.at_prompt {
            return;
        }
        let start = Instant::now();
        loop {
            if self.chan.pending() {
                // newer input is waiting; it will trigger its own paint
                return;
            }
            match sys::is_asleep(self.bash_pid) {
                Some(true) | None => break,
                Some(false) => {}
            }
            if start.elapsed().as_micros() as u64 > SIGNAL_WAIT_US {
                break;
            }
            sys::nap_us(30);
        }
        sys::kill(self.bash_pid, libc::SIGWINCH);
    }

    // ----- completion -------------------------------------------------------------

    /// Is the word starting at char index `wstart` in command position?
    fn is_command_position(&self, wstart: usize) -> bool {
        let prefix: String = self.line.chars().take(wstart).collect();
        let probe = format!("{}{}bt__probe__", self.ps2_ctx, prefix);
        let env = Env { cmds: &self.cmds, cwd: &self.cwd, home: &self.home };
        let spans = lexer::lex(&probe, &env);
        let off = probe.len() - "bt__probe__".len();
        spans
            .iter()
            .find(|s| s.start <= off && off < s.end)
            .map(|s| matches!(s.kind, Kind::UnknownCommand | Kind::Command | Kind::Builtin | Kind::Alias | Kind::Function | Kind::Precommand | Kind::Keyword | Kind::Default))
            .unwrap_or(true)
    }

    /// The command name of the simple command the cursor is in (for
    /// `cd`-style directory-only completion).
    fn current_command(&self, wstart: usize) -> Option<String> {
        let prefix: String = self.line.chars().take(wstart).collect();
        let full = format!("{}{}", self.ps2_ctx, prefix);
        let env = Env { cmds: &self.cmds, cwd: &self.cwd, home: &self.home };
        let spans = lexer::lex(&full, &env);
        let mut last_cmd: Option<String> = None;
        for s in &spans {
            match s.kind {
                Kind::Command | Kind::Builtin | Kind::Alias | Kind::Function | Kind::UnknownCommand => {
                    last_cmd = Some(full[s.start..s.end].to_string());
                }
                Kind::Separator | Kind::CmdSubst => last_cmd = None,
                _ => {}
            }
        }
        last_cmd
    }

    /// Candidates for the current word plus history lines; returns them and
    /// the number of chars of the typed word (for match highlighting).
    fn candidates(&mut self) -> (Vec<Cand>, usize) {
        let (wstart, word) = complete::current_word(&self.line, self.point);
        let ic = self.rlvars.ignore_case;
        let mut cands: Vec<Cand> = Vec::new();
        let limit = self.list_rows * 8 + 8;
        let cmdpos = self.is_command_position(wstart);
        if cmdpos {
            if !word.is_empty() && !word.contains('/') {
                for c in self.cmds.complete(&word, ic, limit) {
                    cands.push(Cand { display: c.clone(), insert: c, whole_line: false, is_dir: false, space: true });
                }
            }
            if word.contains('/') || word.starts_with('.') {
                for f in complete::complete_files(&word, &self.cwd, &self.home, ic, false, limit) {
                    let (dir, _) = complete::split_dir(&word);
                    cands.push(Cand {
                        display: f.name.clone(),
                        insert: format!("{dir}{}", f.name),
                        whole_line: false,
                        is_dir: f.is_dir,
                        space: true,
                    });
                }
            }
        } else {
            let only_dirs = matches!(self.current_command(wstart).as_deref(), Some("cd" | "pushd" | "rmdir" | "mkdir"));
            for f in complete::complete_files(&word, &self.cwd, &self.home, ic, only_dirs, limit) {
                let (dir, _) = complete::split_dir(&word);
                cands.push(Cand {
                    display: f.name.clone(),
                    insert: format!("{dir}{}", f.name),
                    whole_line: false,
                    is_dir: f.is_dir,
                    space: true,
                });
            }
        }
        // history lines starting with the whole line
        let lp = self.line.trim_start();
        if !lp.is_empty() {
            for h in self.history.matching(lp, 6) {
                cands.push(Cand { display: h.to_string(), insert: h.to_string(), whole_line: true, is_dir: false, space: false });
            }
        }
        (cands, word.chars().count())
    }

    fn build_menu(&mut self) -> Option<Menu> {
        let (wstart, word) = complete::current_word(&self.line, self.point);
        let (cands, match_len) = self.candidates();
        if cands.is_empty() {
            return None;
        }
        let quote = self.line.chars().nth(wstart).filter(|c| *c == '\'' || *c == '"');
        let _ = word;
        Some(Menu { cands, idx: None, line: self.line.clone(), point: self.point, word_start: wstart, quote, match_len })
    }
}

fn cands_to_groups(cands: &[Cand], match_len: usize, line_len: usize) -> Vec<ListGroup> {
    let mut words = ListGroup { header: String::new(), match_len, entries: Vec::new() };
    let mut hist = ListGroup { header: "history".into(), match_len: line_len, entries: Vec::new() };
    for c in cands {
        let e = ListEntry { text: c.display.clone(), is_dir: c.is_dir, desc: String::new() };
        if c.whole_line {
            hist.entries.push(e);
        } else {
            words.entries.push(e);
        }
    }
    let mut out = Vec::new();
    if !words.entries.is_empty() {
        out.push(words);
    }
    if !hist.entries.is_empty() {
        out.push(hist);
    }
    out
}
