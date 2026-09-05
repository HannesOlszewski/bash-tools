//! Generates the readline bindings file and the bash init script.
//!
//! Every printable key becomes a macro `<hidden-self-insert><key><hook>` so
//! readline inserts the character itself (keeping all its editing semantics)
//! and then runs our `bind -x` hook, which tells the daemon about the new
//! line. Editing commands that change the line are wrapped the same way.
//!
//! Hidden key sequences use the CSI parameter `9999`, which no terminal
//! emits:
//!
//! * `ESC [ 9999 ; 0 ~ <c>`  self-insert of `<c>`
//! * `ESC [ 9999 ; 1 ~`      insert hook (bind -x)
//! * `ESC [ 9999 ; 2 ~`      edit hook (bind -x)
//! * `ESC [ 9999 ; 3 ~`      accept the whole history suggestion (bind -x)
//! * `ESC [ 9999 ; 4 ~`      accept one word of the suggestion (bind -x)
//! * `ESC [ 9999 ; 5 ~`      accept-line
//! * `ESC [ 9999 ; 6 ~`      post-accept hook (bind -x), runs at the start of
//!                           the next readline call
//! * `ESC [ 9999 ; 10+n ~`   the n-th wrapped readline function

pub const HOOK_INSERT: &str = "\\e[9999;1~";
pub const HOOK_EDIT: &str = "\\e[9999;2~";
pub const HOOK_SUGGEST_ALL: &str = "\\e[9999;3~";
pub const HOOK_SUGGEST_WORD: &str = "\\e[9999;4~";
pub const HOOK_ACCEPT_LINE: &str = "\\e[9999;5~";
pub const HOOK_POST_ACCEPT: &str = "\\e[9999;6~";

/// Readline functions (and their default keys) whose result changes the
/// line, per keymap. After each of them we want a repaint.
const EMACS_WRAPPERS: &[(&[&str], &str)] = &[
    (&["\\C-?", "\\C-h"], "backward-delete-char"),
    (&["\\e[3~"], "delete-char"),
    (&["\\C-k"], "kill-line"),
    (&["\\C-u"], "unix-line-discard"),
    (&["\\C-w"], "unix-word-rubout"),
    (&["\\e\\C-?", "\\e\\C-h"], "backward-kill-word"),
    (&["\\ed"], "kill-word"),
    (&["\\C-y"], "yank"),
    (&["\\ey"], "yank-pop"),
    (&["\\C-t"], "transpose-chars"),
    (&["\\et"], "transpose-words"),
    (&["\\eu"], "upcase-word"),
    (&["\\el"], "downcase-word"),
    (&["\\ec"], "capitalize-word"),
    (&["\\C-p", "\\e[A", "\\eOA"], "previous-history"),
    (&["\\C-n", "\\e[B", "\\eOB"], "next-history"),
    (&["\\e<"], "beginning-of-history"),
    (&["\\e>"], "end-of-history"),
    (&["\\e.", "\\e_"], "yank-last-arg"),
    (&["\\e\\C-y"], "yank-nth-arg"),
    (&["\\C-_", "\\C-x\\C-u"], "undo"),
    (&["\\er"], "revert-line"),
    (&["\\C-l"], "clear-screen"),
    (&["\\e#"], "insert-comment"),
    (&["\\e&"], "tilde-expand"),
];

const VI_INSERT_WRAPPERS: &[(&[&str], &str)] = &[
    (&["\\C-?", "\\C-h"], "backward-delete-char"),
    (&["\\e[3~"], "delete-char"),
    (&["\\C-u"], "unix-line-discard"),
    (&["\\C-w"], "unix-word-rubout"),
    (&["\\C-y"], "yank"),
    (&["\\C-t"], "transpose-chars"),
    (&["\\C-p", "\\e[A", "\\eOA"], "previous-history"),
    (&["\\C-n", "\\e[B", "\\eOB"], "next-history"),
];

const VI_COMMAND_WRAPPERS: &[(&[&str], &str)] = &[
    (&["x"], "vi-delete"),
    (&["X"], "vi-rubout"),
    (&["D"], "kill-line"),
    (&["p", "P"], "vi-put"),
    (&["~"], "vi-change-case"),
    (&["u"], "vi-undo"),
    (&["U"], "revert-line"),
    (&["."], "vi-redo"),
    (&["k", "-"], "previous-history"),
    (&["j", "+"], "next-history"),
    (&["G"], "vi-fetch-history"),
    (&["s"], "vi-subst"),
    (&["S"], "vi-subst"),
    (&["C"], "vi-change-to"),
    (&["\\e[3~"], "delete-char"),
    (&["\\e[A", "\\eOA"], "previous-history"),
    (&["\\e[B", "\\eOB"], "next-history"),
];

/// Cursor-movement functions that accept the history suggestion first when
/// the cursor is at the end of the line (like zsh-autosuggestions): the whole
/// suggestion for forward-char / end-of-line, one word for forward-word.
/// The hidden numbers start at 200 so they never collide with the wrappers.
const SUGGEST_WRAPPERS: &[(&[&str], &str, bool)] = &[
    (&["\\e[C", "\\eOC", "\\C-f"], "forward-char", true),
    (&["\\e[F", "\\eOF", "\\e[4~", "\\C-e"], "end-of-line", true),
    (&["\\ef", "\\e[1;5C", "\\e[1;3C"], "forward-word", false),
];
const VI_SUGGEST_WRAPPERS: &[(&[&str], &str, bool)] = &[
    (&["\\e[C", "\\eOC"], "forward-char", true),
    (&["\\e[F", "\\eOF", "\\e[4~"], "end-of-line", true),
    (&["\\e[1;5C", "\\e[1;3C"], "forward-word", false),
];

fn push_suggest_wrappers(s: &mut String, wrappers: &[(&[&str], &str, bool)]) {
    for (i, (seqs, func, all)) in wrappers.iter().enumerate() {
        let hidden = format!("\\e[9999;{}~", 200 + i);
        let hook = if *all { HOOK_SUGGEST_ALL } else { HOOK_SUGGEST_WORD };
        s.push_str(&format!("\"{hidden}\": {func}\n"));
        for seq in seqs.iter() {
            s.push_str(&format!("\"{seq}\": \"{hook}{hidden}{HOOK_EDIT}\"\n"));
        }
    }
}

fn key_literal(b: u8) -> String {
    match b {
        b'"' => "\\\"".into(),
        b'\\' => "\\\\".into(),
        0x20..=0x7e => (b as char).to_string(),
        _ => format!("\\{:03o}", b),
    }
}

fn push_self_insert_bindings(s: &mut String, byte: u8) {
    let k = key_literal(byte);
    s.push_str(&format!("\"{k}\": \"\\e[9999;0~{k}{HOOK_INSERT}\"\n"));
    s.push_str(&format!("\"\\e[9999;0~{k}\": self-insert\n"));
}

fn push_wrappers(s: &mut String, wrappers: &[(&[&str], &str)], base: usize) {
    for (i, (seqs, func)) in wrappers.iter().enumerate() {
        let hidden = format!("\\e[9999;{}~", base + i);
        s.push_str(&format!("\"{hidden}\": {func}\n"));
        for seq in seqs.iter() {
            s.push_str(&format!("\"{seq}\": \"{hidden}{HOOK_EDIT}\"\n"));
        }
    }
}

fn push_accept(s: &mut String) {
    s.push_str(&format!("\"{HOOK_ACCEPT_LINE}\": accept-line\n"));
    for k in ["\\C-m", "\\C-j"] {
        s.push_str(&format!("\"{k}\": \"{HOOK_ACCEPT_LINE}{HOOK_POST_ACCEPT}\"\n"));
    }
    s.push_str(&format!("\"\\e[9999;7~\": operate-and-get-next\n\"\\C-o\": \"\\e[9999;7~{HOOK_POST_ACCEPT}\"\n"));
}

/// The complete inputrc-syntax bindings file.
pub fn inputrc() -> String {
    let mut s = String::with_capacity(16 * 1024);
    s.push_str("# Generated by bash-tools. Do not edit; regenerate with `bash-tools init`.\n");
    // Only the active editing mode's keymaps are populated: readline skips
    // the other `$if` block quickly, which halves the cost of `bind -f`.
    for keymap in ["emacs", "vi-insert"] {
        s.push_str(if keymap == "emacs" { "$if mode=emacs\n" } else { "$if mode=vi\n" });
        s.push_str(&format!("set keymap {keymap}\n"));
        for b in 0x20u8..=0x7e {
            push_self_insert_bindings(&mut s, b);
        }
        // UTF-8 continuation bytes: the hook runs after each of them; the
        // partial character stays in readline's pending buffer until the
        // sequence is complete, so the line we see is always valid UTF-8.
        for b in 0x80u8..=0xbf {
            push_self_insert_bindings(&mut s, b);
        }
        let w = if keymap == "emacs" { EMACS_WRAPPERS } else { VI_INSERT_WRAPPERS };
        push_wrappers(&mut s, w, 10);
        push_suggest_wrappers(&mut s, if keymap == "emacs" { SUGGEST_WRAPPERS } else { VI_SUGGEST_WRAPPERS });
        push_accept(&mut s);
        if keymap == "vi-insert" {
            s.push_str("set keymap vi-command\n");
            push_wrappers(&mut s, VI_COMMAND_WRAPPERS, 10);
            push_accept(&mut s);
            s.push_str("set keymap vi-insert\n");
        }
        s.push_str("$endif\n");
    }
    s
}

const INIT_TEMPLATE: &str = include_str!("../share/init.bash");

/// The bash snippet to `source`/`eval`. Comment lines (except the header)
/// and blank lines are dropped: bash still has to lex them at every start.
pub fn init_script(bin: &str, inputrc_path: &str) -> String {
    let mut out = String::with_capacity(INIT_TEMPLATE.len());
    for (i, line) in INIT_TEMPLATE.lines().enumerate() {
        let t = line.trim_start();
        if i > 1 && (t.is_empty() || (t.starts_with('#') && !t.starts_with("#!"))) {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.replace("@BIN@", &shell_quote(bin))
        .replace("@INPUTRC@", &shell_quote(inputrc_path))
        .replace("@VERSION@", env!("CARGO_PKG_VERSION"))
}

pub fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+:@%".contains(c)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inputrc_sanity() {
        let s = inputrc();
        assert!(s.contains("\"a\": \"\\e[9999;0~a\\e[9999;1~\"\n"));
        assert!(s.contains("\"\\e[9999;0~a\": self-insert\n"));
        assert!(s.contains("\"\\\"\": \"\\e[9999;0~\\\"\\e[9999;1~\"\n"));
        assert!(s.contains("\"\\\\\": \"\\e[9999;0~\\\\\\e[9999;1~\"\n"));
        assert!(s.contains("\"\\251\": \"\\e[9999;0~\\251\\e[9999;1~\"\n"));
        assert!(s.contains("\"\\C-?\": \"\\e[9999;10~\\e[9999;2~\"\n"));
        assert!(s.contains("\"\\e[9999;10~\": backward-delete-char\n"));
        assert!(s.contains("\"\\C-m\": \"\\e[9999;5~\\e[9999;6~\"\n"));
        assert!(s.contains("\"\\e[C\": \"\\e[9999;3~\\e[9999;200~\\e[9999;2~\"\n"));
        assert!(s.contains("\"\\e[9999;200~\": forward-char\n"));
        assert!(s.contains("\"\\ef\": \"\\e[9999;4~\\e[9999;202~\\e[9999;2~\"\n"));
        assert!(s.ends_with("$endif\n"));
        assert!(s.contains("$if mode=emacs\nset keymap emacs\n"));
        assert!(s.contains("$if mode=vi\nset keymap vi-insert\n"));
        // hidden numbers never collide with hook numbers 0..9
        for w in [EMACS_WRAPPERS, VI_INSERT_WRAPPERS, VI_COMMAND_WRAPPERS] {
            assert!(10 + w.len() < 200);
        }
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("/usr/bin/bash-tools"), "/usr/bin/bash-tools");
        assert_eq!(shell_quote("/a b/c'd"), "'/a b/c'\\''d'");
    }

    #[test]
    fn init_has_no_placeholders() {
        let s = init_script("/x/bash-tools", "/y/keys.inputrc");
        assert!(!s.contains('@') || !s.contains("@BIN@"));
        assert!(s.contains("/x/bash-tools"));
        assert!(s.contains("/y/keys.inputrc"));
    }
}
