//! Completion providers: commands (PATH + builtins + keywords + aliases +
//! functions from bash), files, and history.

use crate::lexer::{Kind, BUILTINS, KEYWORDS};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Everything that can appear in command position.
#[derive(Default)]
pub struct CommandIndex {
    aliases: HashSet<String>,
    functions: HashSet<String>,
    /// PATH executables; value = kind (always Command).
    path_cmds: HashSet<String>,
    path_value: String,
    /// (dir, mtime) snapshot to detect changes cheaply.
    dir_mtimes: Vec<(PathBuf, Option<SystemTime>)>,
    /// Sorted list of all names for prefix completion (rebuilt lazily).
    sorted: Vec<String>,
    sorted_valid: bool,
}

impl CommandIndex {
    pub fn set_aliases(&mut self, names: impl Iterator<Item = String>) {
        self.aliases = names.filter(|n| !n.is_empty()).collect();
        self.sorted_valid = false;
    }
    pub fn set_functions(&mut self, names: impl Iterator<Item = String>) {
        self.functions = names.filter(|n| !n.is_empty()).collect();
        self.sorted_valid = false;
    }

    /// Rescan PATH if it changed or any directory's mtime changed.
    /// Returns true when a rescan happened.
    pub fn refresh_path(&mut self, path: &str, force: bool) -> bool {
        let changed = force
            || path != self.path_value
            || self.dir_mtimes.iter().any(|(d, m)| fs::metadata(d).ok().and_then(|md| md.modified().ok()) != *m);
        if !changed {
            return false;
        }
        self.path_value = path.to_string();
        self.path_cmds.clear();
        self.dir_mtimes.clear();
        for dir in path.split(':') {
            let dir = if dir.is_empty() { "." } else { dir };
            let p = PathBuf::from(dir);
            let mtime = fs::metadata(&p).ok().and_then(|m| m.modified().ok());
            self.dir_mtimes.push((p.clone(), mtime));
            if let Ok(rd) = fs::read_dir(&p) {
                for e in rd.flatten() {
                    let name = match e.file_name().into_string() {
                        Ok(n) => n,
                        Err(_) => continue,
                    };
                    // Executable regular file (follow symlinks).
                    let md = match fs::metadata(e.path()) {
                        Ok(m) => m,
                        Err(_) => continue,
                    };
                    if md.is_file() && md.permissions().mode() & 0o111 != 0 {
                        self.path_cmds.insert(name);
                    }
                }
            }
        }
        self.sorted_valid = false;
        true
    }

    pub fn kind_of(&self, name: &str) -> Option<Kind> {
        if self.aliases.contains(name) {
            return Some(Kind::Alias);
        }
        if self.functions.contains(name) {
            return Some(Kind::Function);
        }
        if BUILTINS.contains(&name) {
            return Some(Kind::Builtin);
        }
        if self.path_cmds.contains(name) {
            return Some(Kind::Command);
        }
        None
    }

    fn rebuild_sorted(&mut self) {
        let mut all: Vec<String> = Vec::with_capacity(self.path_cmds.len() + self.aliases.len() + self.functions.len() + 80);
        all.extend(self.path_cmds.iter().cloned());
        all.extend(self.aliases.iter().cloned());
        all.extend(self.functions.iter().cloned());
        all.extend(BUILTINS.iter().map(|s| s.to_string()));
        all.extend(KEYWORDS.iter().filter(|k| k.chars().all(|c| c.is_ascii_alphabetic())).map(|s| s.to_string()));
        all.sort_unstable();
        all.dedup();
        self.sorted = all;
        self.sorted_valid = true;
    }

    /// Command names starting with `prefix` (sorted).
    pub fn complete(&mut self, prefix: &str, ignore_case: bool, limit: usize) -> Vec<String> {
        if !self.sorted_valid {
            self.rebuild_sorted();
        }
        if ignore_case {
            let lp = prefix.to_lowercase();
            return self
                .sorted
                .iter()
                .filter(|n| n.to_lowercase().starts_with(&lp))
                .take(limit)
                .cloned()
                .collect();
        }
        let start = self.sorted.partition_point(|n| n.as_str() < prefix);
        self.sorted[start..]
            .iter()
            .take_while(|n| n.starts_with(prefix))
            .take(limit)
            .cloned()
            .collect()
    }
}

/// A file completion candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileCand {
    /// Display name (basename, dirs end with `/`).
    pub name: String,
    pub is_dir: bool,
}

/// Split a word into (directory part as typed, basename part).
pub fn split_dir(word: &str) -> (&str, &str) {
    match word.rfind('/') {
        Some(i) => (&word[..i + 1], &word[i + 1..]),
        None => ("", word),
    }
}

/// Expand a leading `~` / `~user` in `p`.
pub fn expand_tilde(p: &str, home: &str) -> String {
    if p == "~" {
        return home.to_string();
    }
    if let Some(rest) = p.strip_prefix("~/") {
        return format!("{}/{}", home.trim_end_matches('/'), rest);
    }
    if let Some(rest) = p.strip_prefix('~') {
        // ~user: best effort via /home or /Users
        let (user, tail) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        for base in ["/home", "/Users"] {
            let cand = format!("{base}/{user}");
            if Path::new(&cand).is_dir() {
                return format!("{cand}{tail}");
            }
        }
    }
    p.to_string()
}

/// Resolve a possibly relative path against `cwd`, expanding `~`.
pub fn resolve(p: &str, cwd: &Path, home: &str) -> PathBuf {
    let e = expand_tilde(p, home);
    let pb = PathBuf::from(&e);
    if pb.is_absolute() {
        pb
    } else {
        cwd.join(pb)
    }
}

/// Cached, sorted directory listings keyed by path and validated by mtime,
/// so that typing inside a big directory does not re-read it per keystroke.
#[derive(Default)]
pub struct DirCache {
    map: HashMap<PathBuf, (Option<SystemTime>, Vec<FileCand>)>,
}

impl DirCache {
    fn entries(&mut self, dir: &Path) -> &[FileCand] {
        let mtime = fs::metadata(dir).ok().and_then(|m| m.modified().ok());
        let stale = match self.map.get(dir) {
            Some((m, _)) => *m != mtime || mtime.is_none(),
            None => true,
        };
        if stale {
            let mut list: Vec<FileCand> = Vec::new();
            if let Ok(rd) = fs::read_dir(dir) {
                for e in rd.flatten() {
                    let name = match e.file_name().into_string() {
                        Ok(n) => n,
                        Err(_) => continue,
                    };
                    let ft = e.file_type();
                    let is_dir = ft.as_ref().map(|t| t.is_dir()).unwrap_or(false)
                        || (ft.map(|t| t.is_symlink()).unwrap_or(false) && fs::metadata(e.path()).map(|m| m.is_dir()).unwrap_or(false));
                    list.push(FileCand { name: if is_dir { format!("{name}/") } else { name }, is_dir });
                }
            }
            list.sort_by(|a, b| a.name.cmp(&b.name));
            if self.map.len() > 64 {
                self.map.clear();
            }
            self.map.insert(dir.to_path_buf(), (mtime, list));
        }
        &self.map.get(dir).unwrap().1
    }

    /// Entries under the directory part of `word` whose names start with
    /// the basename part. Hidden files only when the prefix starts with a
    /// dot. `only_dirs` restricts to directories (e.g. after `cd`).
    pub fn complete(&mut self, word: &str, cwd: &Path, home: &str, ignore_case: bool, only_dirs: bool, limit: usize) -> Vec<FileCand> {
        let (dir_part, base) = split_dir(word);
        let dir = if dir_part.is_empty() { cwd.to_path_buf() } else { resolve(dir_part, cwd, home) };
        let show_hidden = base.starts_with('.');
        let lbase = if ignore_case { base.to_lowercase() } else { String::new() };
        let mut out: Vec<FileCand> = Vec::new();
        for e in self.entries(&dir) {
            if !show_hidden && e.name.starts_with('.') {
                continue;
            }
            let matches = if ignore_case { e.name.to_lowercase().starts_with(&lbase) } else { e.name.starts_with(base) };
            if !matches || (only_dirs && !e.is_dir) {
                continue;
            }
            out.push(e.clone());
            if out.len() >= limit {
                break;
            }
        }
        out
    }
}

/// Convenience wrapper without a cache.
pub fn complete_files(word: &str, cwd: &Path, home: &str, ignore_case: bool, only_dirs: bool, limit: usize) -> Vec<FileCand> {
    DirCache::default().complete(word, cwd, home, ignore_case, only_dirs, limit)
}

pub fn path_exists(p: &str, cwd: &Path, home: &str) -> bool {
    if p.is_empty() {
        return false;
    }
    let full = resolve(p, cwd, home);
    // symlink_metadata so dangling symlinks still count as "exists"
    fs::symlink_metadata(full).is_ok()
}

/// Whether `p` names an executable file (for `./script` command words).
pub fn is_executable(p: &str, cwd: &Path, home: &str) -> bool {
    let full = resolve(p, cwd, home);
    match fs::metadata(full) {
        Ok(m) => m.is_file() && m.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

/// Command history, most recent last, deduplicated on insert.
#[derive(Default)]
pub struct History {
    lines: Vec<String>,
    index: HashMap<String, usize>,
    file: Option<PathBuf>,
    file_len: u64,
}

impl History {
    pub fn load(&mut self, path: &str) {
        if path.is_empty() {
            return;
        }
        let p = PathBuf::from(path);
        let data = match fs::read(&p) {
            Ok(d) => d,
            Err(_) => return,
        };
        self.file = Some(p);
        self.file_len = data.len() as u64;
        let text = String::from_utf8_lossy(&data);
        // bash writes multi-line commands as consecutive lines (or joined
        // with literal newlines when lithist is set). Timestamp comments
        // (`#1700000000`) are skipped.
        for line in text.lines() {
            if line.starts_with('#') && line[1..].chars().all(|c| c.is_ascii_digit()) && line.len() > 1 {
                continue;
            }
            self.push(line);
        }
    }

    /// Re-read the tail of the history file if it grew (e.g. `history -a`).
    pub fn reload_if_grown(&mut self) {
        let p = match &self.file {
            Some(p) => p.clone(),
            None => return,
        };
        let len = match fs::metadata(&p) {
            Ok(m) => m.len(),
            Err(_) => return,
        };
        if len <= self.file_len {
            if len < self.file_len {
                // truncated/rewritten: reload fully
                self.lines.clear();
                self.index.clear();
                self.file_len = 0;
                let path = p.to_string_lossy().to_string();
                self.load(&path);
            }
            return;
        }
        use std::io::{Read, Seek, SeekFrom};
        if let Ok(mut f) = fs::File::open(&p) {
            if f.seek(SeekFrom::Start(self.file_len)).is_ok() {
                let mut buf = Vec::new();
                if f.read_to_end(&mut buf).is_ok() {
                    self.file_len = len;
                    for line in String::from_utf8_lossy(&buf).lines() {
                        if line.starts_with('#') && line.len() > 1 && line[1..].chars().all(|c| c.is_ascii_digit()) {
                            continue;
                        }
                        self.push(line);
                    }
                }
            }
        }
    }

    pub fn push(&mut self, line: &str) {
        let line = line.trim_end();
        if line.trim().is_empty() {
            return;
        }
        if let Some(&i) = self.index.get(line) {
            // move to the end: mark old slot empty
            self.lines[i] = String::new();
        }
        self.index.insert(line.to_string(), self.lines.len());
        self.lines.push(line.to_string());
        if self.lines.len() > 200_000 {
            self.compact();
        }
    }

    fn compact(&mut self) {
        let lines: Vec<String> = self.lines.drain(..).filter(|l| !l.is_empty()).collect();
        self.index.clear();
        for (i, l) in lines.iter().enumerate() {
            self.index.insert(l.clone(), i);
        }
        self.lines = lines;
    }

    /// Most recent line that starts with `prefix` and is longer than it.
    pub fn suggest(&self, prefix: &str) -> Option<&str> {
        if prefix.trim().is_empty() {
            return None;
        }
        self.lines.iter().rev().find(|l| l.len() > prefix.len() && l.starts_with(prefix)).map(|s| s.as_str())
    }

    /// Recent lines starting with `prefix` (most recent first).
    pub fn matching(&self, prefix: &str, limit: usize) -> Vec<&str> {
        if prefix.trim().is_empty() {
            return Vec::new();
        }
        self.lines
            .iter()
            .rev()
            .filter(|l| l.len() > prefix.len() && l.starts_with(prefix))
            .take(limit)
            .map(|s| s.as_str())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }
}

/// Characters that end a word for completion purposes (bash's default
/// `COMP_WORDBREAKS` plus whitespace).
pub fn is_wordbreak(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '"' | '\'' | '>' | '<' | '=' | ';' | '|' | '&' | '(' | ':')
}

/// The word being completed: (start char index, text). Backslash-escaped
/// breaks are honored; quotes are stripped from the returned text.
pub fn current_word(line: &str, point: usize) -> (usize, String) {
    let chars: Vec<char> = line.chars().collect();
    let point = point.min(chars.len());
    let mut start = point;
    // walk back over quoted region if point is inside one
    let mut i = 0;
    let mut quote: Option<char> = None;
    let mut word_start = 0;
    while i < point {
        let c = chars[i];
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else if q == '"' && c == '\\' {
                    i += 1;
                }
            }
            None => {
                if c == '\\' {
                    i += 1;
                } else if c == '\'' || c == '"' {
                    quote = Some(c);
                    // a quote opens a new word only if preceded by a break
                    if i == 0 || is_wordbreak(chars[i - 1]) {
                        word_start = i;
                    }
                } else if is_wordbreak(c) {
                    word_start = i + 1;
                }
            }
        }
        i += 1;
    }
    if quote.is_some() {
        start = word_start;
    } else {
        start = word_start.min(start);
    }
    let raw: String = chars[start..point].iter().collect();
    (start, unquote(&raw))
}

/// Remove quotes and backslash escapes from a word.
pub fn unquote(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut it = raw.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = it.next() {
        match quote {
            Some('\'') => {
                if c == '\'' {
                    quote = None;
                } else {
                    out.push(c);
                }
            }
            Some(_) => {
                if c == '"' {
                    quote = None;
                } else if c == '\\' {
                    match it.peek() {
                        Some(&n) if matches!(n, '$' | '`' | '"' | '\\') => {
                            out.push(n);
                            it.next();
                        }
                        _ => out.push(c),
                    }
                } else {
                    out.push(c);
                }
            }
            None => {
                if c == '\'' || c == '"' {
                    quote = Some(c);
                } else if c == '\\' {
                    if let Some(n) = it.next() {
                        out.push(n);
                    }
                } else {
                    out.push(c);
                }
            }
        }
    }
    out
}

/// Quote a completion for insertion into the line (backslash-escape
/// characters special to bash), unless the original word was quoted.
pub fn quote_for_insert(s: &str, quote: Option<char>) -> String {
    match quote {
        Some('\'') => s.replace('\'', "'\\''"),
        Some('"') => {
            let mut out = String::with_capacity(s.len());
            for c in s.chars() {
                if matches!(c, '"' | '$' | '`' | '\\') {
                    out.push('\\');
                }
                out.push(c);
            }
            out
        }
        _ => {
            let mut out = String::with_capacity(s.len());
            for c in s.chars() {
                if matches!(c, ' ' | '\t' | '\n' | '"' | '\'' | '\\' | '$' | '`' | '&' | '|' | ';' | '(' | ')' | '<' | '>' | '*' | '?' | '[' | ']' | '{' | '}' | '~' | '#' | '!')
                    && !(c == '~' && out.is_empty())
                {
                    out.push('\\');
                }
                out.push(c);
            }
            out
        }
    }
}

/// Longest common prefix of a set of strings.
pub fn common_prefix<'a>(items: impl Iterator<Item = &'a str>) -> String {
    let mut it = items;
    let first = match it.next() {
        Some(f) => f,
        None => return String::new(),
    };
    let mut prefix: Vec<char> = first.chars().collect();
    for s in it {
        let mut n = 0;
        for (a, b) in prefix.iter().zip(s.chars()) {
            if *a != b {
                break;
            }
            n += 1;
        }
        prefix.truncate(n);
        if prefix.is_empty() {
            break;
        }
    }
    prefix.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words() {
        assert_eq!(current_word("ls -la /et", 10), (7, "/et".into()));
        assert_eq!(current_word("ls -la /et", 5), (3, "-l".into()));
        assert_eq!(current_word("echo 'a b", 9), (5, "a b".into()));
        assert_eq!(current_word("echo a\\ b", 9), (5, "a b".into()));
        assert_eq!(current_word("", 0), (0, "".into()));
        assert_eq!(current_word("ls ", 3), (3, "".into()));
        assert_eq!(current_word("--x=/e", 6), (4, "/e".into()));
    }

    #[test]
    fn quoting() {
        assert_eq!(quote_for_insert("a b", None), "a\\ b");
        assert_eq!(quote_for_insert("a b", Some('"')), "a b");
        assert_eq!(quote_for_insert("it's", None), "it\\'s");
        assert_eq!(unquote("\"a b\"c"), "a bc");
        assert_eq!(common_prefix(["foobar", "foobaz", "foo"].into_iter()), "foo");
        assert_eq!(common_prefix(["a", "b"].into_iter()), "");
    }

    #[test]
    fn history() {
        let mut h = History::default();
        h.push("git status");
        h.push("git commit");
        h.push("ls");
        h.push("git status");
        assert_eq!(h.suggest("git "), Some("git status"));
        assert_eq!(h.matching("git ", 10), vec!["git status", "git commit"]);
        assert_eq!(h.suggest("ls"), None);
        assert_eq!(h.len(), 3);
    }

    #[test]
    fn commands() {
        let mut ci = CommandIndex::default();
        ci.set_aliases(["ll".to_string()].into_iter());
        ci.refresh_path("/bin:/usr/bin", true);
        assert_eq!(ci.kind_of("ls"), Some(Kind::Command));
        assert_eq!(ci.kind_of("ll"), Some(Kind::Alias));
        assert_eq!(ci.kind_of("cd"), Some(Kind::Builtin));
        assert_eq!(ci.kind_of("definitely-not-a-command"), None);
        let c = ci.complete("l", false, 1000);
        assert!(c.contains(&"ls".to_string()));
        assert!(c.contains(&"ll".to_string()));
        assert!(c.contains(&"local".to_string()));
        assert!(c.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn files() {
        let cwd = Path::new("/");
        let c = complete_files("/et", cwd, "/root", false, false, 100);
        assert!(c.iter().any(|f| f.name == "etc/" && f.is_dir));
        let c = complete_files("et", cwd, "/root", false, true, 100);
        assert!(c.iter().any(|f| f.name == "etc/"));
        assert!(path_exists("/etc", cwd, "/root"));
        assert!(!path_exists("/definitely/not", cwd, "/root"));
        assert_eq!(expand_tilde("~/x", "/home/u"), "/home/u/x");
    }
}
