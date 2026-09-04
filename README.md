# bash-tools

Syntax highlighting and live autocompletion for **bash**, in the spirit of
`fast-syntax-highlighting` and `zsh-autocomplete`, implemented in Rust and
built for latency.

* **Highlighting as you type**: commands turn green when they exist (red when
  they don't); options, quotes, variables, expansions, redirections, globs,
  comments, heredocs, existing paths and the bracket under the cursor all get
  their own style.
* **Live completion list** below the prompt: commands, files/directories and
  matching history lines appear while you type; `Tab`/`Shift-Tab` cycle
  through them. Bash's programmable completion (`complete -F`, e.g. from
  `bash-completion` or `git`) is used on `Tab` for arguments.
* **History suggestions** as dimmed ghost text after the cursor.
* Works with the editing you already have: readline keeps handling every key,
  so undo, the kill ring, `C-r` search, vi mode and bracketed paste all keep
  working.

Requires bash 5.0 or newer on Linux or macOS.

## Install

```sh
cargo install --path .        # or: cargo build --release && cp target/release/bash-tools ~/.local/bin/
```

Then, at the **end** of `~/.bashrc`:

```sh
eval "$(bash-tools init)"
```

For the fastest possible startup, cache the snippet (it is static per
version) and source the file instead of running the binary at every start:

```sh
mkdir -p ~/.cache/bash-tools && bash-tools init > ~/.cache/bash-tools/init.bash
# in ~/.bashrc:
source ~/.cache/bash-tools/init.bash
```

Turn it off in a running shell with `bash_tools_off`.

## Configuration

Set these **before** sourcing the snippet.

| variable | meaning | default |
|---|---|---|
| `BASH_TOOLS_STYLES` | style overrides, `name=style;name=style` (see below) | none |
| `BASH_TOOLS_LIST_ROWS` | rows reserved below the prompt for the completion list (`0` disables the list) | `8` |
| `BASH_TOOLS_OPTS` | comma-separated: `nolist` (no live list), `nosuggest` (no history ghost text) | none |

Style names follow `zsh-syntax-highlighting` where they exist:
`command`, `unknown-command`, `builtin`, `alias`, `function`,
`reserved-word`, `precommand`, `option`, `path`, `single-quoted`,
`double-quoted`, `dollar-quoted`, `variable`, `variable-in-quotes`, `escape`,
`command-substitution`, `arithmetic-delimiter`, `process-substitution`,
`redirection`, `comment`, `globbing`, `brace-expansion`,
`history-expansion`, `heredoc-delimiter`, `heredoc-body`, `assign`,
`bracket-level-1` to `bracket-level-5`, `bracket-error`,
`cursor-matchingbracket`, `list-selected`, `list-header`, `list-match`,
`list-directory`, `suggestion` (`bash-tools styles` prints the full list).
A style is a comma-separated list of `fg=COLOR`, `bg=COLOR`, `bold`, `dim`,
`italic`, `underline`, `standout`, `strike`, `none`; colors are names (`red`,
`brightblue`), numbers (`0` to `255`) or `#rrggbb`.

```sh
BASH_TOOLS_STYLES='command=fg=#a6e3a1;unknown-command=fg=red;path=fg=blue,underline;comment=fg=244'
```

Try a theme without a shell: `bash-tools highlight 'ls -la | grep "$HOME" # x'`.

## How it works (and why it is fast)

bash has no hook for "the line changed", and anything a `bind -x` command
prints is overwritten because readline clears and redraws the line around it.
bash-tools works around both:

1. Every printable key is bound (via a single generated `inputrc`) to a
   readline *macro*: a hidden `self-insert` followed by a hidden `bind -x`
   hook. Readline does the editing; the hook just writes the line to a FIFO
   (fire-and-forget, one `printf`). Editing keys (backspace, kill/yank,
   history navigation, ...) are wrapped the same way.
2. A tiny per-shell daemon (`bash-tools daemon`, started with a single fork
   from the init snippet) lexes the line, computes the highlighting, the
   completion candidates and the suggestion, and then sends bash `SIGWINCH`
   once bash is idle in `read()`. Readline handles that signal by redrawing
   the line, *then* bash runs our `WINCH` trap, which asks the daemon to paint.
   The daemon writes the colored text (and the list below) directly to the
   terminal, wrapped in save/restore-cursor so readline's idea of the cursor
   never changes.
3. `PS0` (expanded once per executed command) erases the list and bumps a
   counter, so the daemon never paints while a command is running.

Costs measured on an M1 MacBook (Linux forks are cheaper):

| what | cost |
|---|---|
| sourcing the init snippet (including the daemon fork) | about 1.5 ms |
| per prompt (`PROMPT_COMMAND` hook, `${PS1@P}`, history sync) | about 0.2 ms |
| keystroke to highlighted text on screen | 0.3 to 0.5 ms |

No polling, no timers, no forks after startup. The binary has no runtime
dependencies.

## Limitations

* A few things do not trigger a repaint until the next keystroke: text
  inserted with `C-v`, `C-d` (it is not wrapped so that `C-d` on an empty line
  still exits), and readline functions bound to non-default keys in your own
  `.inputrc` (the default keys are wrapped; custom bindings keep working, they
  just don't repaint).
* Digit arguments (`M-3 a`) insert the character once.
* The completion list needs space below the prompt; `BASH_TOOLS_LIST_ROWS`
  rows are reserved at each prompt (like zsh-autocomplete does), which
  scrolls the screen when the prompt is near the bottom.
* Switching between emacs and vi editing mode after startup leaves the other
  mode without bindings (re-source the snippet).
* `compopt` inside completion functions is emulated; the `-X` filter option of
  `complete` is ignored.

## Development

```sh
cargo test                              # unit tests (+ the pty suite when python3 is available)
cargo build --release
python3 tests/pty/test_e2e.py           # end-to-end tests in a pseudo terminal
python3 tests/pty/perf.py               # latency measurements
docker/run-tests.sh fedora|ubuntu|arch  # the same inside Linux containers
BASH_TOOLS_DEBUG=/tmp/bt.log bash       # daemon trace log
```
