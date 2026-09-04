"""Minimal VT100/xterm screen emulator and a pty driver for bash.

Only what readline and bash-tools emit is modelled: cursor movement (CUU, CUD,
CUF, CUB, CHA, CUP), erase (EL, ED), SGR, DECSC/DECRC, CR/LF/BS/TAB, wide
characters and the pending-wrap state at the last column.
"""
import os, pty, select, time, re, signal, fcntl, termios, struct, unicodedata

def wcwidth(c):
    if ord(c) < 32 or ord(c) == 0x7f:
        return 0
    if unicodedata.combining(c):
        return 0
    return 2 if unicodedata.east_asian_width(c) in ('W', 'F') else 1

class Screen:
    def __init__(self, cols=80, rows=24):
        self.cols, self.rows = cols, rows
        self.cells = [[(' ', '') for _ in range(cols)] for _ in range(rows)]
        self.cx = self.cy = 0
        self.sgr = ''
        self.saved = (0, 0, '')
        self.pending_wrap = False
        self.log = []
        self.scrolled = 0

    def blank_row(self):
        return [(' ', '') for _ in range(self.cols)]

    def feed(self, data):
        s = data.decode('utf-8', 'replace')
        i = 0
        while i < len(s):
            c = s[i]
            if c == '\x1b':
                m = re.match(r'\x1b\[([\?>]?[0-9;]*)([A-Za-z@`])', s[i:])
                if m:
                    self.csi(m.group(1), m.group(2)); i += m.end(); continue
                m = re.match(r'\x1b\]([^\x07\x1b]*)(\x07|\x1b\\)', s[i:])
                if m:
                    i += m.end(); continue
                nxt = s[i+1:i+2]
                if nxt == '7':
                    self.saved = (self.cx, self.cy, self.sgr); i += 2; continue
                if nxt == '8':
                    self.cx, self.cy, self.sgr = self.saved; self.pending_wrap = False; i += 2; continue
                if nxt in ('=', '>', 'M', 'c'):
                    if nxt == 'M':
                        self.cy = max(0, self.cy - 1)
                    i += 2; continue
                self.log.append('unknown esc: %r' % s[i:i+8]); i += 1; continue
            if c == '\r':
                self.cx = 0; self.pending_wrap = False
            elif c == '\n':
                self.lf()
            elif c == '\b':
                self.cx = max(0, self.cx - 1); self.pending_wrap = False
            elif c == '\x07':
                pass
            elif c == '\t':
                self.cx = min(self.cols - 1, (self.cx // 8 + 1) * 8)
            elif ord(c) < 32:
                pass
            else:
                w = wcwidth(c)
                if w == 0:
                    # combining: attach to previous cell
                    x = max(0, self.cx - 1)
                    ch, st = self.cells[self.cy][x]
                    self.cells[self.cy][x] = (ch + c, st)
                    i += 1; continue
                if self.pending_wrap or (w == 2 and self.cx == self.cols - 1):
                    self.cx = 0; self.lf(); self.pending_wrap = False
                self.cells[self.cy][self.cx] = (c, self.sgr)
                if w == 2:
                    self.cells[self.cy][self.cx + 1] = ('', self.sgr)
                if self.cx + w >= self.cols:
                    self.cx = self.cols - 1
                    self.pending_wrap = True
                else:
                    self.cx += w
            i += 1

    def lf(self):
        if self.cy == self.rows - 1:
            self.cells.pop(0); self.cells.append(self.blank_row()); self.scrolled += 1
        else:
            self.cy += 1

    def csi(self, p, f):
        self.pending_wrap = False
        ps = p.lstrip('?>')
        args = [int(x) if x else 0 for x in ps.split(';')] if ps else []
        n = args[0] if args else 0
        if f == 'A':
            self.cy = max(0, self.cy - max(n, 1))
        elif f == 'B':
            self.cy = min(self.rows - 1, self.cy + max(n, 1))
        elif f == 'C':
            self.cx = min(self.cols - 1, self.cx + max(n, 1))
        elif f == 'D':
            self.cx = max(0, self.cx - max(n, 1))
        elif f == 'G':
            self.cx = max(0, min(self.cols - 1, max(n, 1) - 1))
        elif f in ('H', 'f'):
            r = args[0] if len(args) > 0 else 1
            c = args[1] if len(args) > 1 else 1
            self.cy = max(0, min(self.rows - 1, max(r, 1) - 1))
            self.cx = max(0, min(self.cols - 1, max(c, 1) - 1))
        elif f == 'J':
            if n == 0:
                for x in range(self.cx, self.cols):
                    self.cells[self.cy][x] = (' ', '')
                for y in range(self.cy + 1, self.rows):
                    self.cells[y] = self.blank_row()
            elif n == 2 or n == 3:
                self.cells = [self.blank_row() for _ in range(self.rows)]
        elif f == 'K':
            if n == 0:
                for x in range(self.cx, self.cols):
                    self.cells[self.cy][x] = (' ', '')
            elif n == 1:
                for x in range(0, self.cx + 1):
                    self.cells[self.cy][x] = (' ', '')
            elif n == 2:
                self.cells[self.cy] = self.blank_row()
        elif f == 'm':
            self.sgr = '' if p in ('', '0') else p
        elif f == 'P':
            k = max(n, 1); row = self.cells[self.cy]
            del row[self.cx:self.cx + k]; row.extend([(' ', '')] * k)
        elif f == '@':
            k = max(n, 1); row = self.cells[self.cy]
            for _ in range(k):
                row.insert(self.cx, (' ', ''))
            del row[self.cols:]
        elif f in ('h', 'l', 'r', 'c', 'n', 't', 's', 'u', 'd', 'X', 'L', 'M', 'S', 'T'):
            pass
        else:
            self.log.append('unknown csi %r %r' % (p, f))

    def text(self, y):
        return ''.join(c for c, _ in self.cells[y]).rstrip()

    def dump(self):
        return '\n'.join('%2d|%s' % (y, self.text(y)) for y in range(self.rows) if self.text(y))

    def styled(self, y):
        out, cur = [], None
        for c, s in self.cells[y]:
            if s != cur:
                out.append('{%s}' % s); cur = s
            out.append(c)
        return ''.join(out).rstrip()

    def style_at(self, y, x):
        return self.cells[y][x][1]

    def find(self, needle):
        """(row, col) of the first occurrence of needle on screen."""
        for y in range(self.rows):
            t = ''.join(c for c, _ in self.cells[y])
            i = t.find(needle)
            if i >= 0:
                return (y, i)
        return None


class Bash:
    """An interactive bash running in a pty with the given rcfile."""
    BASH = os.environ.get('BASH_TOOLS_TEST_BASH') or '/opt/homebrew/bin/bash'
    if not os.path.exists(BASH):
        BASH = 'bash'

    def __init__(self, rc, cols=80, rows=24, env=None):
        self.scr = Screen(cols, rows)
        pid, fd = pty.fork()
        if pid == 0:
            e = {
                'TERM': 'xterm-256color',
                'HOME': os.environ.get('HOME', '/tmp'),
                'PATH': os.environ['PATH'],
                'LANG': 'en_US.UTF-8',
                'LC_ALL': 'en_US.UTF-8',
                'INPUTRC': '/dev/null',
                'HISTFILE': '/dev/null',
                'PS1': '$ ',
            }
            if env:
                e.update(env)
            os.execvpe(self.BASH, [self.BASH, '--noprofile', '--rcfile', rc, '-i'], e)
        self.pid, self.fd = pid, fd
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', rows, cols, 0, 0))
        self.chunks = []
        self.raw = b''

    def resize(self, cols, rows):
        self.scr = Screen(cols, rows)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack('HHHH', rows, cols, 0, 0))
        os.kill(self.pid, signal.SIGWINCH)

    def drain(self, timeout=0.2, settle=0.05):
        """Read output until nothing arrives for `settle` seconds (or timeout)."""
        end = time.time() + timeout
        got = b''
        last = time.time()
        while True:
            rem = min(end - time.time(), settle)
            if end - time.time() <= 0:
                break
            r, _, _ = select.select([self.fd], [], [], max(rem, 0))
            if not r:
                if time.time() - last >= settle:
                    break
                continue
            try:
                d = os.read(self.fd, 65536)
            except OSError:
                break
            if not d:
                break
            last = time.time()
            self.chunks.append(d); got += d; self.raw += d; self.scr.feed(d)
        return got

    def send(self, s, wait=0.3):
        os.write(self.fd, s.encode() if isinstance(s, str) else s)
        return self.drain(wait)

    def type(self, s, wait=0.3, per_key=0.02):
        for ch in s:
            os.write(self.fd, ch.encode())
            self.drain(per_key, settle=0.01)
        return self.drain(wait)

    def wait_for(self, needle, timeout=3.0):
        end = time.time() + timeout
        while time.time() < end:
            self.drain(0.1)
            if self.scr.find(needle) is not None:
                return True
        return False

    def close(self):
        try:
            os.kill(self.pid, signal.SIGKILL)
        except OSError:
            pass
        try:
            os.waitpid(self.pid, 0)
        except OSError:
            pass
        os.close(self.fd)
