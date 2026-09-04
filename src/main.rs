//! `bash-tools`: syntax highlighting and live autocompletion for bash.

mod complete;
mod daemon;
mod keys;
mod layout;
mod lexer;
mod render;
mod style;
mod sys;

use std::io::Write;

const USAGE: &str = "\
bash-tools {version} -- syntax highlighting and live completion for bash

USAGE:
    eval \"$(bash-tools init)\"          # in ~/.bashrc (simple)
    bash-tools init > ~/.cache/bash-tools/init.bash && source that file  # fastest

COMMANDS:
    init [--bin PATH]     print the bash snippet to source (also writes the
                          key bindings file into the cache directory)
    inputrc               print the generated readline bindings
    highlight [LINE...]   highlight a line (or stdin) with ANSI colors
    styles                list style names and their defaults
    daemon                (internal) the per-shell helper process
    --version, -V         print the version
    --help, -h            this text

CONFIGURATION (shell variables, set before sourcing):
    BASH_TOOLS_STYLES='command=fg=green,bold;path=underline;...'
    BASH_TOOLS_LIST_ROWS=8      rows reserved below the prompt for the list
    BASH_TOOLS_OPTS=nolist,nosuggest
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("--help");
    match cmd {
        "daemon" => daemon::run(),
        "init" => cmd_init(&args[1..]),
        "inputrc" => {
            print!("{}", keys::inputrc());
        }
        "highlight" => cmd_highlight(&args[1..]),
        "styles" => {
            let t = style::Theme::default();
            let _ = t;
            for n in style::Theme::names() {
                println!("{n}");
            }
        }
        "--version" | "-V" | "version" => println!("bash-tools {}", env!("CARGO_PKG_VERSION")),
        _ => {
            print!("{}", USAGE.replace("{version}", env!("CARGO_PKG_VERSION")));
        }
    }
}

fn cache_dir() -> std::path::PathBuf {
    if let Ok(d) = std::env::var("XDG_CACHE_HOME") {
        if !d.is_empty() {
            return std::path::PathBuf::from(d).join("bash-tools");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    std::path::PathBuf::from(home).join(".cache").join("bash-tools")
}

fn cmd_init(args: &[String]) {
    let mut bin: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--bin" if i + 1 < args.len() => {
                bin = Some(args[i + 1].clone());
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    let bin = bin.unwrap_or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.canonicalize().ok())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "bash-tools".into())
    });
    let dir = cache_dir();
    let inputrc_path = dir.join(format!("keys-{}.inputrc", env!("CARGO_PKG_VERSION")));
    let content = keys::inputrc();
    let current = std::fs::read_to_string(&inputrc_path).ok();
    if current.as_deref() != Some(content.as_str()) {
        if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&inputrc_path, &content)) {
            eprintln!("bash-tools: cannot write {}: {e}", inputrc_path.display());
            std::process::exit(1);
        }
    }
    let script = keys::init_script(&bin, &inputrc_path.to_string_lossy());
    let _ = std::io::stdout().write_all(script.as_bytes());
}

fn cmd_highlight(args: &[String]) {
    let mut lines: Vec<String> = Vec::new();
    let mut theme = style::Theme::default();
    if let Ok(s) = std::env::var("BASH_TOOLS_STYLES") {
        theme.apply(&s);
    }
    let mut spans_only = false;
    let mut rest: Vec<&String> = Vec::new();
    for a in args {
        if a == "--spans" {
            spans_only = true;
        } else {
            rest.push(a);
        }
    }
    if rest.is_empty() {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut s);
        lines.push(s.trim_end_matches('\n').to_string());
    } else {
        lines.push(rest.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" "));
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| "/".into());
    let home = std::env::var("HOME").unwrap_or_default();
    let mut cmds = complete::CommandIndex::default();
    cmds.refresh_path(&std::env::var("PATH").unwrap_or_default(), true);
    struct L<'a> {
        cmds: &'a complete::CommandIndex,
        cwd: &'a std::path::Path,
        home: &'a str,
    }
    impl<'a> lexer::Lookup for L<'a> {
        fn command_kind(&self, name: &str) -> Option<lexer::Kind> {
            if name.contains('/') {
                return if complete::is_executable(name, self.cwd, self.home) { Some(lexer::Kind::Command) } else { None };
            }
            self.cmds.kind_of(name)
        }
        fn path_exists(&self, p: &str) -> bool {
            complete::path_exists(p, self.cwd, self.home)
        }
    }
    let lk = L { cmds: &cmds, cwd: &cwd, home: &home };
    let mut out = std::io::stdout().lock();
    for line in lines {
        let spans = lexer::lex(&line, &lk);
        if spans_only {
            for sp in &spans {
                let _ = writeln!(out, "{:?}\t{:?}", sp.kind, &line[sp.start..sp.end]);
            }
        } else {
            let _ = out.write_all(&render::render_inline(&line, &spans, &theme));
            let _ = out.write_all(b"\n");
        }
    }
}
