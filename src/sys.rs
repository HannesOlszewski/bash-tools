//! Thin platform layer: tty access, window size, FIFOs, signals, process
//! state inspection (to decide when bash is idle) and `poll`.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::unix::io::{FromRawFd, RawFd};

pub fn open_tty() -> Option<File> {
    use std::fs::OpenOptions;
    if let Ok(f) = OpenOptions::new().write(true).open("/dev/tty") {
        return Some(f);
    }
    // fall back to stderr (what readline writes to)
    let fd = unsafe { libc::dup(2) };
    if fd >= 0 {
        Some(unsafe { File::from_raw_fd(fd) })
    } else {
        None
    }
}

/// (columns, rows) of the terminal behind `fd`.
pub fn winsize(fd: RawFd) -> Option<(usize, usize)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws as *mut libc::winsize) };
    if r == 0 && ws.ws_col > 0 {
        Some((ws.ws_col as usize, ws.ws_row.max(1) as usize))
    } else {
        None
    }
}

pub fn mkfifo(path: &str) -> io::Result<()> {
    let c = CString::new(path).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "nul in path"))?;
    let r = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Open a FIFO for reading without blocking for a writer.
pub fn open_fifo_reader(path: &str) -> io::Result<File> {
    let c = CString::new(path).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "nul in path"))?;
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub fn kill(pid: i32, sig: i32) -> bool {
    unsafe { libc::kill(pid, sig) == 0 }
}

pub fn getppid() -> i32 {
    unsafe { libc::getppid() }
}

pub fn getpid() -> i32 {
    unsafe { libc::getpid() }
}

pub fn ignore_signal(sig: i32) {
    unsafe {
        libc::signal(sig, libc::SIG_IGN);
    }
}

/// Ask the kernel to send us `sig` when the parent dies (Linux only).
pub fn die_with_parent() {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
    }
}

/// Wait until `fd` is readable or `timeout_ms` elapsed. Returns true when
/// readable (or hung up), false on timeout.
pub fn poll_readable(fd: RawFd, timeout_ms: i32) -> bool {
    let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    loop {
        let r = unsafe { libc::poll(&mut p as *mut libc::pollfd, 1, timeout_ms) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return true;
        }
        return r > 0;
    }
}

/// Whether the process is blocked in a system call (state "S" on Linux,
/// no running threads on macOS). `None` if unknown.
pub fn is_asleep(pid: i32) -> Option<bool> {
    #[cfg(target_os = "linux")]
    {
        let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // state is the first field after the closing paren of the comm
        let rest = &s[s.rfind(')')? + 1..];
        let state = rest.trim_start().chars().next()?;
        return Some(matches!(state, 'S' | 'D'));
    }
    #[cfg(target_os = "macos")]
    {
        let mut info: libc::proc_taskallinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_taskallinfo>() as i32;
        let r = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTASKALLINFO,
                0,
                &mut info as *mut libc::proc_taskallinfo as *mut libc::c_void,
                size,
            )
        };
        if r != size {
            return None;
        }
        return Some(info.ptinfo.pti_numrunning == 0);
    }
    #[allow(unreachable_code)]
    None
}

/// Write all bytes to a raw fd, retrying on EINTR / partial writes.
pub fn write_all(fd: RawFd, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        let n = unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        buf = &buf[n as usize..];
    }
    Ok(())
}

pub fn read_some(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        return Ok(n as usize);
    }
}

/// Sleep for a few microseconds (used while polling bash's state).
pub fn nap_us(us: u32) {
    std::thread::sleep(std::time::Duration::from_micros(us as u64));
}

/// Runtime directory for our FIFO.
pub fn runtime_dir() -> String {
    if let Ok(d) = std::env::var("XDG_RUNTIME_DIR") {
        if !d.is_empty() {
            return d;
        }
    }
    if let Ok(d) = std::env::var("TMPDIR") {
        if !d.is_empty() {
            return d.trim_end_matches('/').to_string();
        }
    }
    "/tmp".to_string()
}
