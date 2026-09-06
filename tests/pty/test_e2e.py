#!/usr/bin/env python3
"""End-to-end tests: drive a real interactive bash with bash-tools loaded in
a pseudo terminal and check what ends up on the (emulated) screen.

Run: python3 tests/pty/test_e2e.py   (uses target/release/bash-tools, or
$BASH_TOOLS_BIN)
"""
import os, re, sys, time, signal, subprocess, tempfile, unittest, shutil

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, HERE)
from vt import Bash

BIN = os.environ.get('BASH_TOOLS_BIN') or os.path.join(ROOT, 'target', 'release', 'bash-tools')

GREEN, RED, CYAN, YELLOW, MAGENTA, UNDERLINE, BOLD_BLUE = '0;32', '0;1;31', '0;36', '0;33', '0;35', '0;4', '0;1;34'


class Env:
    """Temp HOME with the generated init script and a cache dir."""
    def __init__(self):
        self.dir = tempfile.mkdtemp(prefix='bt-test-')
        self.cache = os.path.join(self.dir, 'cache')
        env = dict(os.environ, XDG_CACHE_HOME=self.cache, HOME=self.dir)
        out = subprocess.run([BIN, 'init', '--bin', BIN], env=env, capture_output=True, text=True, check=True).stdout
        self.init = os.path.join(self.dir, 'init.bash')
        with open(self.init, 'w') as f:
            f.write(out)
        self.hist = os.path.join(self.dir, 'hist')
        with open(self.hist, 'w') as f:
            f.write('echo hello world\ngit status\ngit commit -m x\n')

    def rc(self, extra='', ps1='$ '):
        p = os.path.join(self.dir, 'rc-%d.bash' % int(time.time() * 1e6))
        q = "'" + ps1.replace("'", "'\\''") + "'"   # bash single-quoting
        with open(p, 'w') as f:
            f.write("PS1=%s\n%s\nsource %s\n" % (q, extra, self.init))
        return p

    def bash(self, extra='', ps1='$ ', cols=80, rows=24, env=None, wait=None):
        e = {'HOME': self.dir, 'XDG_CACHE_HOME': self.cache, 'HISTFILE': self.hist,
             'BASH_TOOLS_DEBUG': os.path.join(self.dir, 'daemon.log')}
        if env:
            e.update(env)
        b = Bash(self.rc(extra, ps1), cols=cols, rows=rows, env=e)
        if wait is None:
            wait = ps1.split('\n')[-1].strip()
            if not wait or '\\' in wait:
                wait = '$'
        assert b.wait_for(wait), 'no prompt: %r' % b.raw
        b.drain(0.3)
        return b

    def cleanup(self):
        shutil.rmtree(self.dir, ignore_errors=True)


ENV = None


def setUpModule():
    global ENV
    ENV = Env()


def tearDownModule():
    ENV.cleanup()


class T(unittest.TestCase):
    def assertStyle(self, b, needle, style, msg=''):
        pos = b.scr.find(needle)
        self.assertIsNotNone(pos, 'text %r not on screen:\n%s' % (needle, b.scr.dump()))
        y, x = pos
        got = b.scr.style_at(y, x)
        self.assertEqual(got, style, 'style of %r is %r, expected %r; row: %s %s' % (needle, got, style, b.scr.styled(y), msg))

    def test_basic_highlight_and_cursor(self):
        b = ENV.bash(cols=60, rows=16)
        try:
            b.type('ls -ld /etc')
            self.assertStyle(b, 'ls', GREEN)
            self.assertStyle(b, '-ld', CYAN)
            self.assertStyle(b, '/etc', UNDERLINE)
            self.assertEqual((b.scr.cx, b.scr.cy), (13, 0))
            self.assertEqual(b.scr.text(0), '$ ls -ld /etc')
            # completion list below
            self.assertIsNotNone(b.scr.find('etc/'))
            b.send('\r', 0.6)
            # list erased, output printed right below the command line
            self.assertEqual(b.scr.text(0), '$ ls -ld /etc')
            self.assertTrue(b.scr.text(1).startswith(('l', 'd', '-', 't')), b.scr.dump())
            self.assertEqual(b.scr.log, [])
        finally:
            b.close()

    def test_unknown_command_turns_valid(self):
        b = ENV.bash()
        try:
            b.type('l')
            self.assertStyle(b, 'l', RED)
            b.type('s')
            self.assertStyle(b, 'ls', GREEN)
            b.type(' "a $HOME" \'x\' 2>&1 | grep -v foo # c')
            self.assertStyle(b, '"a ', YELLOW)
            self.assertStyle(b, '$HOME', CYAN)
            self.assertStyle(b, "'x'", YELLOW)
            self.assertStyle(b, '2>&1', MAGENTA)
            self.assertStyle(b, 'grep', GREEN)
            self.assertStyle(b, '# c', '0;90')
            self.assertEqual(b.scr.text(0), '$ ls "a $HOME" \'x\' 2>&1 | grep -v foo # c')
        finally:
            b.close()

    def test_backspace_and_editing_keys(self):
        b = ENV.bash()
        try:
            b.type('lsx')
            self.assertStyle(b, 'lsx', RED)
            b.send('\x7f')          # backspace
            self.assertStyle(b, 'ls', GREEN)
            self.assertEqual(b.scr.text(0), '$ ls')
            b.send('\x15')          # C-u: unix-line-discard
            self.assertEqual(b.scr.text(0), '$')
            b.type('echo foo bar')
            b.send('\x17')          # C-w
            self.assertEqual(b.scr.text(0), '$ echo foo')
            b.send('\x01')          # C-a
            b.send('\x0b')          # C-k
            self.assertEqual(b.scr.text(0), '$')
            b.send('\x19')          # C-y yank back
            self.assertEqual(b.scr.text(0), '$ echo foo')
            self.assertStyle(b, 'echo', GREEN)
            self.assertEqual((b.scr.cx, b.scr.cy), (11, 0))
        finally:
            b.close()

    def test_wrapping_long_line(self):
        b = ENV.bash(cols=30, rows=12)
        try:
            line = 'echo ' + ' '.join('w%d' % i for i in range(12))
            b.type(line)
            full = '$ ' + line
            rows = [full[i:i + 30] for i in range(0, len(full), 30)]
            for i, r in enumerate(rows):
                self.assertEqual(b.scr.text(i), r.rstrip())
            self.assertEqual((b.scr.cx, b.scr.cy), (len(full) % 30, len(full) // 30))
            self.assertStyle(b, 'echo', GREEN)
            # go to the beginning and insert: readline reflows, we repaint
            b.send('\x01')
            b.type('x')
            self.assertEqual(b.scr.text(0), ('$ x' + line)[:30])
            self.assertStyle(b, 'xecho', RED)
            self.assertEqual((b.scr.cx, b.scr.cy), (3, 0))
            b.send('\x05')  # C-e
            b.send('\r', 0.5)
            self.assertIsNotNone(b.scr.find('w0 w1'))
            self.assertEqual(b.scr.log, [])
        finally:
            b.close()

    def test_multiline_prompt_and_reservation(self):
        b = ENV.bash(ps1='line1\n$ ', cols=50, rows=10)
        try:
            self.assertEqual(b.scr.text(0), 'line1')
            self.assertEqual(b.scr.text(1), '$')
            b.type('ls /et')
            self.assertStyle(b, 'ls', GREEN)
            self.assertEqual((b.scr.cx, b.scr.cy), (8, 1))
            self.assertIsNotNone(b.scr.find('etc/'))
            b.send('\r', 0.5)
            self.assertIsNotNone(b.scr.find('line1'))
            self.assertEqual(b.scr.log, [])
        finally:
            b.close()

    def test_tab_completion(self):
        b = ENV.bash()
        try:
            b.type('cd /et')
            b.send('\t', 0.5)
            self.assertEqual(b.scr.text(0), '$ cd /etc/')
            self.assertEqual((b.scr.cx, b.scr.cy), (10, 0))
            b.send('\x15')
            b.type('ls /usr/l')
            b.send('\t', 0.5)
            first = b.scr.text(0)
            self.assertTrue(first.startswith('$ ls /usr/l'), first)
            self.assertIsNotNone(b.scr.find('local/'))
            b.send('\t', 0.5)
            second = b.scr.text(0)
            self.assertNotEqual(first, second)
            b.send('\x1b[Z', 0.5)   # shift-tab goes back
            self.assertEqual(b.scr.text(0), first)
            self.assertEqual(b.scr.log, [])
        finally:
            b.close()

    def test_ps2_continuation(self):
        b = ENV.bash(cols=40, rows=12)
        try:
            b.type('echo "a')
            b.send('\r', 0.4)
            self.assertEqual(b.scr.text(1), '>')
            b.type('b"')
            self.assertStyle(b, 'b"', YELLOW)
            self.assertEqual((b.scr.cx, b.scr.cy), (4, 1))
            b.send('\r', 0.5)
            y = b.scr.find('> b"')[0]
            self.assertEqual(b.scr.text(y + 1), 'a')
            self.assertEqual(b.scr.text(y + 2), 'b')
            self.assertEqual(b.scr.text(y + 3), '$')
            self.assertEqual(b.scr.log, [])
        finally:
            b.close()

    def test_history_suggestion_and_list(self):
        b = ENV.bash()
        try:
            b.type('git st')
            self.assertEqual(b.scr.text(0), '$ git status')
            self.assertStyle(b, 'atus', '0;90')
            self.assertEqual((b.scr.cx, b.scr.cy), (8, 0))
            self.assertIsNotNone(b.scr.find('history'))
            b.send('\x15')
            b.type('echo hi')
            b.send('\r', 0.4)
            b.type('echo h')
            self.assertEqual(b.scr.text(2), '$ echo hi')
        finally:
            b.close()

    def test_wide_chars(self):
        b = ENV.bash(cols=40)
        try:
            b.type('echo 漢字 x')
            self.assertEqual(b.scr.text(0), '$ echo 漢字 x')
            self.assertEqual((b.scr.cx, b.scr.cy), (13, 0))
            b.send('\x1b[D\x1b[D')  # left twice
            b.type('y')
            self.assertEqual(b.scr.text(0), '$ echo 漢字y x')
            self.assertEqual((b.scr.cx, b.scr.cy), (12, 0))
        finally:
            b.close()

    def test_ctrl_c_and_clear(self):
        b = ENV.bash(cols=40, rows=10)
        try:
            b.type('ls /et')
            self.assertIsNotNone(b.scr.find('etc/'))
            b.send('\x03', 0.5)
            self.assertIsNone(b.scr.find('etc/'))
            self.assertEqual(b.scr.text(b.scr.cy), '$')
            b.type('ls /et')
            b.send('\x0c', 0.5)   # C-l
            self.assertEqual(b.scr.text(0), '$ ls /et')
            self.assertStyle(b, 'ls', GREEN)
            self.assertEqual(b.scr.cy, 0)
        finally:
            b.close()

    def test_isearch_still_works(self):
        b = ENV.bash()
        try:
            b.type('echo abc')
            b.send('\r', 0.4)
            b.send('\x12')      # C-r
            b.type('ab')
            self.assertIsNotNone(b.scr.find('reverse-i-search'))
            b.send('\x05')      # C-e accepts
            self.assertEqual(b.scr.text(b.scr.cy), '$ echo abc')
        finally:
            b.close()

    def test_resize_repaints(self):
        b = ENV.bash(cols=40, rows=12)
        try:
            b.type('ls -la')
            b.resize(50, 12)
            b.drain(0.5)
            self.assertStyle(b, 'ls', GREEN)
            self.assertEqual(b.scr.text(0), '$ ls -la')
        finally:
            b.close()

    def test_paste(self):
        b = ENV.bash()
        try:
            b.send('\x1b[200~ls -la /tmp\x1b[201~', 0.4)
            b.type(' ')
            self.assertStyle(b, 'ls', GREEN)
            self.assertStyle(b, '/tmp', UNDERLINE)
        finally:
            b.close()

    def test_programmable_completion(self):
        extra = '''
_foo() { COMPREPLY=(); case $prev in -x) COMPREPLY=(xa xb);; *) COMPREPLY=($(compgen -W "alpha beta gamma --x" -- "$cur"));; esac; }
_foo_wrap() { local cur=${COMP_WORDS[COMP_CWORD]} prev=${COMP_WORDS[COMP_CWORD-1]}; _foo; }
complete -F _foo_wrap foo
complete -W "red green blue" -o nospace bar
_lazy() { complete -F _foo_wrap lazycmd; return 124; }
complete -F _lazy -D
foo() { :; }; bar() { :; }; lazycmd() { :; }
'''
        b = ENV.bash(extra=extra)
        try:
            b.type('foo al')
            b.send('\t', 0.6)
            self.assertEqual(b.scr.text(0), '$ foo alpha')
            self.assertEqual((b.scr.cx, b.scr.cy), (12, 0))
            b.send('\x15')
            b.type('foo -x ')
            b.send('\t', 0.6)
            self.assertEqual(b.scr.text(0), '$ foo -x x')     # common prefix first
            b.send('\t', 0.6)
            self.assertEqual(b.scr.text(0), '$ foo -x xa')
            b.send('\t', 0.6)
            self.assertEqual(b.scr.text(0), '$ foo -x xb')
            b.send('\x15')
            b.type('bar g')
            b.send('\t', 0.6)
            self.assertEqual(b.scr.text(0), '$ bar green')
            self.assertEqual((b.scr.cx, b.scr.cy), (11, 0))   # nospace
            b.send('\x15')
            b.type('lazycmd be')
            b.send('\t', 0.6)
            self.assertEqual(b.scr.text(0), '$ lazycmd beta')
            self.assertEqual(b.scr.log, [])
        finally:
            b.close()

    def test_read_e_in_script_is_left_alone(self):
        b = ENV.bash(extra='f() { local v; read -e -p "name: " v; echo "got $v"; }')
        try:
            b.type('f')
            self.assertStyle(b, 'f', GREEN)
            b.send('\r', 0.4)
            b.type('ls /etc')
            # no highlighting, no completion list inside read -e
            self.assertEqual(b.scr.styled(1), '{}name: ls /etc')
            self.assertIsNone(b.scr.find('etc/'))
            b.send('\r', 0.4)
            self.assertEqual(b.scr.text(2), 'got ls /etc')
            # and the prompt works again afterwards
            b.type('ls')
            self.assertEqual(b.scr.styled(b.scr.cy), '{}$ {0;32}ls{}')
        finally:
            b.close()

    def test_typeahead_burst(self):
        b = ENV.bash(cols=50)
        try:
            # everything arrives in one write: hooks, signals and paints must
            # settle to the correct final state
            b.send('ls -la /etc/hosts | grep -v x\x7f\x7f y', 0.6)
            self.assertEqual(b.scr.text(0), '$ ls -la /etc/hosts | grep -v y')
            self.assertStyle(b, 'ls', GREEN)
            self.assertStyle(b, 'grep', GREEN)
            self.assertStyle(b, '/etc/hosts', UNDERLINE)
            self.assertEqual((b.scr.cx, b.scr.cy), (31, 0))
            b.send('\x15')
            b.send('echo one\recho two\r', 0.8)
            y = b.scr.find('$ echo one')[0]
            self.assertEqual(b.scr.text(y + 1), 'one')
            self.assertEqual(b.scr.text(y + 2), '$ echo two')
            self.assertEqual(b.scr.text(y + 3), 'two')
            self.assertEqual(b.scr.log, [])
        finally:
            b.close()

    def test_colored_prompt(self):
        b = ENV.bash(ps1='\\[\\e[1;32m\\]user@host\\[\\e[0m\\]:\\W\\$ ', cols=60, wait='user@host')
        try:
            b.type('ls -la')
            row = b.scr.text(0)
            self.assertTrue(row.startswith('user@host:'), row)
            self.assertTrue(row.endswith(('$ ls -la', '# ls -la')), row)
            self.assertStyle(b, 'ls -la', GREEN)
            self.assertStyle(b, '-la', CYAN)
            self.assertEqual((b.scr.cx, b.scr.cy), (len(row), 0))
        finally:
            b.close()

    def test_vi_mode(self):
        b = ENV.bash(extra='set -o vi')
        try:
            b.type('lsx -la')
            self.assertStyle(b, 'lsx', RED)
            b.send('\x1b', 0.4)     # ESC: command mode
            b.send('0', 0.2)        # start of line
            b.send('ll', 0.2)       # move right twice
            b.send('x', 0.4)        # delete the 'x'
            self.assertEqual(b.scr.text(0), '$ ls -la')
            self.assertStyle(b, 'ls', GREEN)
            b.send('A', 0.3)        # append at end
            b.type(' /etc')
            self.assertStyle(b, '/etc', UNDERLINE)
        finally:
            b.close()

    def test_user_winch_trap_is_chained(self):
        marker = os.path.join(ENV.dir, 'winch-%d' % os.getpid())
        b = ENV.bash(extra="trap 'echo RESIZED >> %s' WINCH" % marker, cols=40, rows=12)
        try:
            b.type('ls -la')
            self.assertStyle(b, 'ls', GREEN)
            # our paints use SIGWINCH too, but the user's trap must only run
            # for real resizes
            self.assertFalse(os.path.exists(marker))
            b.resize(50, 12)
            b.drain(0.5)
            self.assertTrue(os.path.exists(marker) and open(marker).read().count('RESIZED') == 1, 'marker missing')
            self.assertEqual(b.scr.text(0), '$ ls -la')
            self.assertStyle(b, 'ls', GREEN)
        finally:
            b.close()

    def test_unknown_term_bails_out(self):
        # readline without terminfo redraws on a new line around every
        # bind -x hook; we must detect that and stay out of the way
        b = ENV.bash(cols=60, rows=12, env={'TERM': 'xterm-doesnotexist'}, wait='bash-tools:')
        try:
            y0 = b.scr.cy
            b.type('ls -la')
            # still on the same row: no redraw on a new line per keystroke
            self.assertEqual(b.scr.cy, y0)
            self.assertEqual(b.scr.text(y0), '$ ls -la')
            self.assertEqual(b.scr.styled(y0), '{}$ ls -la')
        finally:
            b.close()

    def test_right_arrow_accepts_suggestion(self):
        b = ENV.bash()
        try:
            b.type('echo h')
            self.assertEqual(b.scr.text(0), '$ echo hello world')   # ghost text
            b.send('\x1b[C', 0.4)                                   # Right
            self.assertEqual(b.scr.text(0), '$ echo hello world')
            self.assertEqual((b.scr.cx, b.scr.cy), (18, 0))
            self.assertStyle(b, 'hello world', '')                   # real text now
            self.assertStyle(b, 'echo', GREEN)
            b.send('\r', 0.4)
            self.assertEqual(b.scr.text(1), 'hello world')
            # one word at a time with M-f, then End takes the rest
            b.type('git ')
            self.assertEqual(b.scr.text(2), '$ git commit -m x')
            b.send('\x1bf', 0.4)
            self.assertEqual((b.scr.cx, b.scr.cy), (12, 2))
            b.send('\x1b[F', 0.4)
            self.assertEqual((b.scr.cx, b.scr.cy), (17, 2))
            self.assertStyle(b, '-m', CYAN)
            b.send('\x15')
            # Right in the middle of a line just moves the cursor
            b.type('ls -la')
            b.send('\x01\x1b[C', 0.4)
            self.assertEqual((b.scr.cx, b.scr.cy), (3, 2))
            self.assertEqual(b.scr.text(2), '$ ls -la')
        finally:
            b.close()

    def snapshot(self):
        """`bash-tools init > file`: stdout is a real file, so the snapshot
        gets the staleness guard baked in (the piped form does not)."""
        d = tempfile.mkdtemp(prefix='bt-snap-', dir=ENV.dir)
        p = os.path.join(d, 'init.bash')
        env = dict(os.environ, XDG_CACHE_HOME=ENV.cache, HOME=ENV.dir)
        with open(p, 'w') as f:
            subprocess.run([BIN, 'init', '--bin', BIN], env=env, stdout=f, check=True)
        return p

    def test_snapshot_has_guard_and_piped_does_not(self):
        with open(self.snapshot()) as f:
            snap = f.read()
        self.assertIn('-nt', snap)
        self.assertNotIn('@STALE_CHECK@', snap)
        # the form ENV uses (captured stdout = a pipe) must stay guard-free
        with open(ENV.init) as f:
            self.assertNotIn('-nt', f.read())

    def test_stale_snapshot_warns(self):
        p = self.snapshot()
        rc = os.path.join(os.path.dirname(p), 'rc.bash')
        with open(rc, 'w') as f:
            f.write("PS1='$ '\nsource %s\n" % p)
        e = {'HOME': ENV.dir, 'XDG_CACHE_HOME': ENV.cache, 'HISTFILE': ENV.hist}

        def out(env):
            # the raw stream, not the screen: the warning wraps at 80 columns
            b = Bash(rc, env=env)
            try:
                self.assertTrue(b.wait_for('$'))
                b.drain(0.3)
                return b.raw.decode('utf-8', 'replace'), b
            except Exception:
                b.close()
                raise

        # fresh snapshot (newer than the binary): silent
        s, b = out(e)
        try:
            self.assertNotIn('stale', s)
        finally:
            b.close()

        # back-date it behind the binary: warns, and the shell still works
        old = os.path.getmtime(BIN) - 60
        os.utime(p, (old, old))
        s, b = out(e)
        try:
            self.assertIn('the cached snippet is stale', s)
            self.assertIn('bash-tools init > %s' % p, s)
            b.type('echo ok')
            b.send('\r', 0.4)
            self.assertIn('ok', b.scr.dump())
        finally:
            b.close()

        # silenced by the documented opt-out
        s, b = out(dict(e, BASH_TOOLS_OPTS='nostalecheck'))
        try:
            self.assertNotIn('stale', s)
        finally:
            b.close()

    def test_inputrc_named_by_content(self):
        env = dict(os.environ, XDG_CACHE_HOME=ENV.cache, HOME=ENV.dir)
        out = subprocess.run([BIN, 'init', '--bin', BIN], env=env,
                             capture_output=True, text=True, check=True).stdout
        m = re.search(r'bind -f (\S+)', out)
        self.assertIsNotNone(m, out)
        path = m.group(1).strip("'")
        self.assertRegex(os.path.basename(path), r'^keys-[0-9a-f]{16}\.inputrc$')
        self.assertTrue(os.path.exists(path))
        # the name is a function of the content
        with open(path) as f:
            self.assertEqual(f.read(), subprocess.run(
                [BIN, 'inputrc'], env=env, capture_output=True, text=True, check=True).stdout)

    def test_daemon_exits_with_shell(self):
        b = ENV.bash()
        try:
            b.type('echo $__bt_pid')
            b.send('\r', 0.4)
            pid = int(b.scr.text(1))
            self.assertTrue(os.path.exists('/proc/%d' % pid) or os.kill(pid, 0) is None)
            b.send('exit\r', 0.3)
            time.sleep(1.5)
            with self.assertRaises(OSError):
                os.kill(pid, 0)
        finally:
            b.close()


if __name__ == '__main__':
    unittest.main(verbosity=2)
