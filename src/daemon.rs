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
//! | p   | ps1 ps2 pwd path reserved n wordbreaks   | a new prompt is about to be shown    |
//! | H   | text                                     | last history entry                   |
//! | l/e | line point n                             | line changed (insert / edit)         |
//! | a   | n                                        | new readline call after accept-line  |
//! | t/T | line point n                             | Tab / Shift-Tab (expects reply)      |
//! | d   | –                                        | bash finished a reply-based hook     |
//! | g   | n                                        | paint now (from the WINCH trap)      |
//! | r   | –                                        | signal again later                   |
//! | A/F | names (newline separated)                | aliases / functions                  |
//! | V   | `bind -v` output                         | readline variables                   |
//! | W   | `trap -p WINCH` output                   | reply: the user's trap command       |
//! | x   | –                                        | exit                                 |
//!
//! `n` is bash's count of executed command lines (bumped by `PS0`); anything
//! sent after a command started (`n` newer than at the last prompt) is
//! ignored, e.g. hooks firing inside a script's `read -e`.
//!
//! Replies: `t`/`T` → `I\0line\0point\0` or `N\0`; `g` → two bytes:
//! `y`/`n` (something was drawn below the line) and `m`/`r` (the SIGWINCH
//! was ours / a real resize, so bash knows whether to run the user's trap).

use crate::complete::{self, CommandIndex, DirCache, History};
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
    dirs: DirCache,
    history: History,
    line: String,
    point: usize,
    at_prompt: bool,
    ps2_mode: bool,
    ps2_ctx: String,
    rows_above: usize,
    /// Number of `p` messages seen (PS1 prompts shown).
    prompt_count: usize,
    /// bash's executed-command counter as of the last prompt.
    cmdno: u64,
    /// `COMP_WORDBREAKS` of the shell.
    wordbreaks: String,
    /// Signals we sent that bash has not yet answered with `g`.
    signals_sent: u32,
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
        dirs: DirCache::default(),
        history: History::default(),
        line: String::new(),
        point: 0,
        at_prompt: false,
        ps2_mode: false,
        ps2_ctx: String::new(),
        rows_above: 0,
        prompt_count: 0,
        cmdno: 0,
        wordbreaks: " \t\n\"'><=;|&(:".into(),
        signals_sent: 0,
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
            let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
            let _ = writeln!(f, "{}.{:06} {}", t.as_secs() % 1000, t.subsec_micros(), msg);
        }
    }

    fn main_loop(&mut self) {
        while let Some(t) = self.chan.field() {
            let ty = t.first().copied().unwrap_or(0);
            if !self.dispatch(ty) {
                return;
            }
        }
    }

    /// Handle one message; returns false on `x` (exit).
    fn dispatch(&mut self, ty: u8) -> bool {
        {
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
                    if self.cmdno_matches() {
                        self.on_line(line, point, ty == b'l');
                    }
                }
                b'a' => {
                    if self.cmdno_matches() {
                        self.on_accept();
                    }
                }
                b't' | b'T' => {
                    let line = self.chan.field_str().unwrap_or_default();
                    let point = self.chan.field_str().unwrap_or_default().trim().parse().unwrap_or(0);
                    if self.cmdno_matches() {
                        self.on_tab(line, point, ty == b'T');
                    } else {
                        self.reply(b"N\0");
                    }
                }
                b'd' => self.request_paint(),
                b'g' => {
                    let ok = self.cmdno_matches();
                    self.on_go(ok);
                }
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
                b'W' => {
                    // `trap -p WINCH` output → the user's trap command (or empty)
                    let s = self.chan.field_str().unwrap_or_default();
                    let cmd = user_trap_command(&s);
                    let mut r = cmd.into_bytes();
                    r.push(0);
                    self.reply(&r);
                }
                b'x' => return false,
                _ => {
                    self.debug(&format!("unknown message {:?}", ty as char));
                }
            }
        }
        true
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
        self.cmdno = self.chan.field_str().unwrap_or_default().trim().parse().unwrap_or(0);
        let wb = self.chan.field_str().unwrap_or_default();
        if !wb.is_empty() {
            self.wordbreaks = wb;
        }
        if !pwd.is_empty() {
            self.cwd = PathBuf::from(pwd);
        }
        self.cmds.refresh_path(&path, false);
        self.history.reload_if_grown();
        self.at_prompt = true;
        self.prompt_count += 1;
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
        if self.log.is_some() {
            self.debug(&format!("line {:?} point {}", line, point));
        }
        if let Some(m) = &self.menu {
            if m.line != line || m.point != point {
                self.menu = None;
            }
        }
        self.line = line;
        self.point = point;
        self.seen_prompt_since_accept = false;
        self.list_visible = !self.line.is_empty();
        self.frame_valid = false;
        self.compute_frame();
        self.request_paint();
    }

    /// Read the command counter field and compare with the one seen at the
    /// last prompt. A mismatch means a command line has been accepted since.
    fn cmdno_matches(&mut self) -> bool {
        let n = self.chan.field_str().unwrap_or_default();
        let n: u64 = n.trim().parse().unwrap_or(self.cmdno);
        if n != self.cmdno {
            self.debug(&format!("ignoring message: command {} running (prompt has {})", n, self.cmdno));
            self.at_prompt = false;
            return false;
        }
        self.at_prompt
    }

    fn on_go(&mut self, ok: bool) {
        if self.log.is_some() {
            self.debug("go");
        }
        let mine = if self.signals_sent > 0 {
            self.signals_sent -= 1;
            b'm'
        } else {
            b'r'
        };
        let mut ack = b'n';
        if !ok {
            self.reply(&[ack, mine]);
            return;
        }
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
        self.reply(&[ack, mine]);
    }

    fn on_tab(&mut self, line: String, point: usize, backward: bool) {
        self.seen_prompt_since_accept = false;
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

    /// Highlight spans for the current line (taking PS2 context into account),
    /// with the bracket under the cursor and its partner marked.
    fn spans(&self) -> Vec<Span> {
        let env = Env { cmds: &self.cmds, cwd: &self.cwd, home: &self.home };
        let mut spans = if self.ps2_ctx.is_empty() {
            lexer::lex(&self.line, &env)
        } else {
            let full = format!("{}{}", self.ps2_ctx, self.line);
            let off = self.ps2_ctx.len();
            lexer::lex(&full, &env)
                .into_iter()
                .filter(|s| s.end > off)
                .map(|s| Span { start: s.start.saturating_sub(off), end: s.end - off, kind: s.kind })
                .collect()
        };
        // byte offset of the cursor
        let point_byte = self.line.char_indices().nth(self.point).map(|(i, _)| i).unwrap_or(self.line.len());
        lexer::mark_matching_bracket(&self.line, &mut spans, point_byte);
        spans
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
        let mut polls = 0u32;
        loop {
            if self.chan.pending() {
                // newer input is waiting; it will trigger its own paint
                return;
            }
            match sys::is_asleep(self.bash_pid) {
                Some(true) | None => break,
                Some(false) => {}
            }
            polls += 1;
            if start.elapsed().as_micros() as u64 > SIGNAL_WAIT_US {
                break;
            }
            sys::nap_us(30);
        }
        sys::kill(self.bash_pid, libc::SIGWINCH);
        self.signals_sent += 1;
        if self.log.is_some() {
            self.debug(&format!("signal after {}us ({} polls)", start.elapsed().as_micros(), polls));
        }
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
                for f in self.dirs.complete(&word, &self.cwd, &self.home, ic, false, limit) {
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
            for f in self.dirs.complete(&word, &self.cwd, &self.home, ic, only_dirs, limit) {
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
        let (mut cands, match_len) = self.candidates();
        // Programmable completion for arguments (git, ssh, ...): ask bash.
        if !self.is_command_position(wstart) {
            if let Some(cmd) = self.current_command(wstart) {
                if let Some(extra) = self.compspec_candidates(&cmd, wstart, &word) {
                    // compspec results replace our file guesses (history stays)
                    cands.retain(|c| c.whole_line);
                    let mut merged = extra;
                    merged.extend(cands);
                    cands = merged;
                }
            }
        }
        if cands.is_empty() {
            return None;
        }
        let quote = self.line.chars().nth(wstart).filter(|c| *c == '\'' || *c == '"');
        let _ = word;
        Some(Menu { cands, idx: None, line: self.line.clone(), point: self.point, word_start: wstart, quote, match_len })
    }
}

/// Extract the command from `trap -- 'cmd' SIGWINCH`.
fn user_trap_command(output: &str) -> String {
    let line = output.lines().find(|l| l.starts_with("trap -- ")).unwrap_or("");
    let words = shell_words(line);
    // words: trap, --, cmd, SIGWINCH
    if words.len() >= 4 && words[0] == "trap" && words[1] == "--" {
        let cmd = words[2].trim();
        if cmd.is_empty() || cmd == "__bt_winch" {
            return String::new();
        }
        return cmd.to_string();
    }
    String::new()
}

/// A parsed `complete -p` line.
#[derive(Default, Debug)]
struct Spec {
    func: Option<String>,
    wordlist: Option<String>,
    command: Option<String>,
    glob: Option<String>,
    actions: Vec<String>,
    opts: Vec<String>,
    prefix: String,
    suffix: String,
}

/// Split a line into shell words, honoring quotes (enough for `complete -p`).
fn shell_words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = s.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match quote {
            Some('\'') => {
                if c == '\'' {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            Some(_) => {
                if c == '"' {
                    quote = None;
                } else if c == '\\' {
                    if let Some(n) = chars.next() {
                        if !matches!(n, '"' | '$' | '`' | '\\') {
                            cur.push('\\');
                        }
                        cur.push(n);
                    }
                } else {
                    cur.push(c);
                }
            }
            None => {
                if c == '\'' || c == '"' {
                    quote = Some(c);
                    in_word = true;
                } else if c == '\\' {
                    if let Some(n) = chars.next() {
                        cur.push(n);
                        in_word = true;
                    }
                } else if c.is_whitespace() {
                    if in_word {
                        out.push(std::mem::take(&mut cur));
                        in_word = false;
                    }
                } else {
                    cur.push(c);
                    in_word = true;
                }
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    out
}

fn parse_spec(line: &str) -> Option<Spec> {
    let words = shell_words(line.trim());
    if words.first().map(|w| w.as_str()) != Some("complete") {
        return None;
    }
    let mut sp = Spec::default();
    let mut i = 1;
    while i < words.len() {
        let w = words[i].as_str();
        let arg = words.get(i + 1).cloned();
        match w {
            "-o" => {
                if let Some(a) = arg {
                    sp.opts.push(a);
                }
                i += 1;
            }
            "-A" => {
                if let Some(a) = arg {
                    sp.actions.push(a);
                }
                i += 1;
            }
            "-F" => {
                sp.func = arg;
                i += 1;
            }
            "-W" => {
                sp.wordlist = arg;
                i += 1;
            }
            "-C" => {
                sp.command = arg;
                i += 1;
            }
            "-G" => {
                sp.glob = arg;
                i += 1;
            }
            "-P" => {
                sp.prefix = arg.unwrap_or_default();
                i += 1;
            }
            "-S" => {
                sp.suffix = arg.unwrap_or_default();
                i += 1;
            }
            "-X" => {
                i += 1;
            }
            "-a" => sp.actions.push("alias".into()),
            "-b" => sp.actions.push("builtin".into()),
            "-c" => sp.actions.push("command".into()),
            "-d" => sp.actions.push("directory".into()),
            "-e" => sp.actions.push("export".into()),
            "-f" => sp.actions.push("file".into()),
            "-g" => sp.actions.push("group".into()),
            "-j" => sp.actions.push("job".into()),
            "-k" => sp.actions.push("keyword".into()),
            "-s" => sp.actions.push("service".into()),
            "-u" => sp.actions.push("user".into()),
            "-v" => sp.actions.push("variable".into()),
            _ => {}
        }
        i += 1;
    }
    Some(sp)
}

/// Split a line into `COMP_WORDS` the way bash does: whitespace separates,
/// other `COMP_WORDBREAKS` characters are words of their own, quoted text
/// stays together. Returns the words and the index of the word at `point`.
fn comp_words(line: &str, point: usize, breaks: &str) -> (Vec<String>, usize) {
    let chars: Vec<char> = line.chars().collect();
    let point = point.min(chars.len());
    let mut words: Vec<(usize, String)> = Vec::new(); // (start, text)
    let mut cur = String::new();
    let mut start = 0;
    let mut i = 0;
    let mut quote: Option<char> = None;
    let mut in_word = false;
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    quote = None;
                } else if q == '"' && c == '\\' && i + 1 < chars.len() {
                    i += 1;
                    cur.push(chars[i]);
                }
            }
            None => {
                if c == '\\' && i + 1 < chars.len() {
                    if !in_word {
                        start = i;
                        in_word = true;
                    }
                    cur.push(c);
                    i += 1;
                    cur.push(chars[i]);
                } else if c == '\'' || c == '"' {
                    if !in_word {
                        start = i;
                        in_word = true;
                    }
                    quote = Some(c);
                    cur.push(c);
                } else if c == ' ' || c == '\t' || c == '\n' {
                    if in_word {
                        words.push((start, std::mem::take(&mut cur)));
                        in_word = false;
                    }
                } else if breaks.contains(c) {
                    if in_word {
                        words.push((start, std::mem::take(&mut cur)));
                        in_word = false;
                    }
                    words.push((i, c.to_string()));
                } else {
                    if !in_word {
                        start = i;
                        in_word = true;
                    }
                    cur.push(c);
                }
            }
        }
        i += 1;
    }
    if in_word {
        words.push((start, cur));
    }
    // the word containing point, or a new empty word at point
    let mut cword = words.len();
    for (idx, (st, text)) in words.iter().enumerate() {
        let end = st + text.chars().count();
        if point >= *st && point <= end {
            cword = idx;
            break;
        }
    }
    if cword == words.len() {
        words.push((point, String::new()));
    }
    (words.into_iter().map(|(_, t)| t).collect(), cword)
}

impl Daemon {
    /// Ask bash for the compspec of `cmd`, run it, and turn the results
    /// into candidates. `None` when there is no compspec.
    fn compspec_candidates(&mut self, cmd: &str, wstart: usize, word: &str) -> Option<Vec<Cand>> {
        let (words, cword) = comp_words(&self.line, self.point, &self.wordbreaks);
        let mut spec = self.fetch_spec(cmd)?;
        let mut items: Vec<String> = Vec::new();
        let mut opts: Vec<String> = spec.opts.clone();
        for attempt in 0..2 {
            items.clear();
            let mut retry = false;
            let mut parts: Vec<(char, String)> = Vec::new();
            for a in &spec.actions {
                parts.push(('A', a.clone()));
            }
            if let Some(g) = &spec.glob {
                parts.push(('G', g.clone()));
            }
            if let Some(w) = &spec.wordlist {
                parts.push(('W', w.clone()));
            }
            if let Some(f) = &spec.func {
                parts.push(('F', f.clone()));
            }
            if let Some(c) = &spec.command {
                parts.push(('C', c.clone()));
            }
            if parts.is_empty() {
                break;
            }
            for (kind, arg) in parts {
                let (status, copts, lines) = self.run_spec_part(kind, &arg, cmd, &words, cword)?;
                if kind == 'F' && status == 124 && attempt == 0 {
                    retry = true;
                    break;
                }
                opts.extend(copts.split_whitespace().map(|s| s.to_string()));
                items.extend(lines);
            }
            if !retry {
                break;
            }
            spec = self.fetch_spec(cmd)?;
            opts = spec.opts.clone();
        }
        let has = |o: &str| opts.iter().any(|x| x == o);
        let filenames = has("filenames");
        let nospace = has("nospace");
        if !spec.prefix.is_empty() || !spec.suffix.is_empty() {
            for it in &mut items {
                *it = format!("{}{}{}", spec.prefix, it, spec.suffix);
            }
        }
        if !has("nosort") {
            items.sort();
        }
        items.dedup();
        items.retain(|s| !s.is_empty());
        let mut cands: Vec<Cand> = Vec::new();
        let (dir_part, _) = complete::split_dir(word);
        for it in items {
            let is_dir = (filenames || it.ends_with('/')) && {
                let p = if it.starts_with('/') || it.starts_with('~') { it.clone() } else { format!("{dir_part}{it}") };
                complete::path_exists(&p, &self.cwd, &self.home) && complete::resolve(&p, &self.cwd, &self.home).is_dir()
            };
            let mut insert = it.clone();
            if is_dir && !insert.ends_with('/') {
                insert.push('/');
            }
            let display = if filenames { complete::split_dir(&it).1.to_string() } else { it.clone() };
            let display = if is_dir && !display.ends_with('/') { format!("{display}/") } else { display };
            let space = !nospace && !is_dir && !insert.ends_with('=') && !insert.ends_with(':');
            cands.push(Cand { display, insert, whole_line: false, is_dir, space });
        }
        if cands.is_empty() {
            let fallback_files = has("default") || has("bashdefault") || has("dirnames") || has("plusdirs");
            if !fallback_files {
                return Some(Vec::new());
            }
            let only_dirs = has("dirnames") && !has("default") && !has("bashdefault");
            let _ = wstart;
            for f in self.dirs.complete(word, &self.cwd, &self.home, self.rlvars.ignore_case, only_dirs, 400) {
                cands.push(Cand {
                    display: f.name.clone(),
                    insert: format!("{dir_part}{}", f.name),
                    whole_line: false,
                    is_dir: f.is_dir,
                    space: true,
                });
            }
        }
        Some(cands)
    }

    fn fetch_spec(&mut self, cmd: &str) -> Option<Spec> {
        let mut req = Vec::new();
        req.extend_from_slice(b"C\0");
        req.extend_from_slice(cmd.as_bytes());
        req.push(0);
        self.reply(&req);
        let ty = self.chan.field()?;
        if ty.first() != Some(&b'S') {
            // something else arrived (user interrupted?); process it and give up
            let t = ty.first().copied().unwrap_or(0);
            self.dispatch(t);
            return None;
        }
        let text = self.chan.field_str()?;
        let line = text.lines().next().unwrap_or("");
        if line.is_empty() {
            return None;
        }
        parse_spec(line)
    }

    fn run_spec_part(&mut self, kind: char, arg: &str, cmd: &str, words: &[String], cword: usize) -> Option<(i32, String, Vec<String>)> {
        let mut req = Vec::new();
        req.extend_from_slice(b"R\0");
        req.push(kind as u8);
        req.push(0);
        req.extend_from_slice(arg.as_bytes());
        req.push(0);
        req.extend_from_slice(cmd.as_bytes());
        req.push(0);
        req.extend_from_slice(cword.to_string().as_bytes());
        req.push(0);
        req.extend_from_slice(self.line.as_bytes());
        req.push(0);
        req.extend_from_slice(self.point.to_string().as_bytes());
        req.push(0);
        req.extend_from_slice(words.len().to_string().as_bytes());
        req.push(0);
        for w in words {
            req.extend_from_slice(w.as_bytes());
            req.push(0);
        }
        self.reply(&req);
        let ty = self.chan.field()?;
        if ty.first() != Some(&b'Q') {
            let t = ty.first().copied().unwrap_or(0);
            self.dispatch(t);
            return None;
        }
        let status: i32 = self.chan.field_str()?.trim().parse().unwrap_or(1);
        let copts = self.chan.field_str()?;
        let out = self.chan.field_str()?;
        let lines = out.lines().filter(|l| !l.is_empty()).map(|l| l.to_string()).collect();
        Some((status, copts, lines))
    }
}

fn cands_to_groups(cands: &[Cand], match_len: usize, line_len: usize) -> Vec<ListGroup> {
    let mut words = ListGroup { header: String::new(), match_len, entries: Vec::new() };
    let mut hist = ListGroup { header: "history".into(), match_len: line_len, entries: Vec::new() };
    for c in cands {
        // control characters (multi-line history entries) would break the list
        let text: String = c.display.chars().map(|ch| if ch == '\n' { '\u{23ce}' } else if ch.is_control() { ' ' } else { ch }).collect();
        let e = ListEntry { text, is_dir: c.is_dir, desc: String::new() };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_parsing() {
        let sp = parse_spec("complete -o default -o nospace -F _git git").unwrap();
        assert_eq!(sp.func.as_deref(), Some("_git"));
        assert_eq!(sp.opts, vec!["default", "nospace"]);
        let sp = parse_spec("complete -W 'a b  c' -P '--' -S '=' foo").unwrap();
        assert_eq!(sp.wordlist.as_deref(), Some("a b  c"));
        assert_eq!(sp.prefix, "--");
        assert_eq!(sp.suffix, "=");
        let sp = parse_spec("complete -o bashdefault -o default -F _comp_complete_load -D").unwrap();
        assert_eq!(sp.func.as_deref(), Some("_comp_complete_load"));
        let sp = parse_spec("complete -d -A user x").unwrap();
        assert_eq!(sp.actions, vec!["directory", "user"]);
        assert!(parse_spec("").is_none());
        assert_eq!(shell_words(r#"a "b c" 'd e' f\ g"#), vec!["a", "b c", "d e", "f g"]);
    }

    #[test]
    fn trap_parsing() {
        assert_eq!(user_trap_command("trap -- 'echo resized' SIGWINCH\n"), "echo resized");
        assert_eq!(user_trap_command("trap -- 'a '\\''b'\\''' SIGWINCH"), "a 'b'");
        assert_eq!(user_trap_command(""), "");
        assert_eq!(user_trap_command("trap -- '' SIGWINCH"), "");
    }

    #[test]
    fn comp_words_split() {
        let wb = " \t\n\"'><=;|&(:";
        assert_eq!(comp_words("git ch", 6, wb), (vec!["git".to_string(), "ch".into()], 1));
        assert_eq!(comp_words("git ", 4, wb), (vec!["git".to_string(), "".into()], 1));
        assert_eq!(comp_words("git --opt=va", 12, wb), (vec!["git".to_string(), "--opt".into(), "=".into(), "va".into()], 3));
        assert_eq!(comp_words("ssh host:pa", 11, wb).1, 3);
        // bash-completion removes `:` from COMP_WORDBREAKS
        assert_eq!(comp_words("ssh host:pa", 11, " \t\n\"'><=;|&(").1, 1);
        assert_eq!(comp_words("echo 'a b' c", 12, wb).0, vec!["echo", "'a b'", "c"]);
        assert_eq!(comp_words("ls x", 2, wb), (vec!["ls".to_string(), "x".into()], 0));
    }
}
