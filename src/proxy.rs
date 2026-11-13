//! Runs the program on a pty of its own and copies the bytes between that pty and
//! the terminal the shim runs in (the pane of tmux), through the filter.
//!
//! One thread and `poll()`: the work is a copy, so there is nothing to
//! synchronize. The input (the keys) is never parsed. Both sides are
//! non-blocking, and the shim stops reading from the program while the terminal
//! side is full, so the memory stays bounded and one direction never waits for
//! the other.

use crate::filter::{Filter, Wrap};
use libc::{c_int, pollfd, POLLERR, POLLHUP, POLLIN, POLLOUT};
use std::ffi::OsString;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// The most that is kept for one side (the bytes that could not be written yet).
const BUF_LIMIT: usize = 256 * 1024;
/// The size of one read.
const READ_SIZE: usize = 64 * 1024;
/// How long something undecided (an ESC at the end of a read) is held back
/// while the program is quiet.
const UNDECIDED_MS: c_int = 10;
/// After a request to stop (SIGHUP, SIGTERM), the program has this long to end.
const KILL_AFTER: Duration = Duration::from_secs(2);

pub struct Options {
    pub wrap: Wrap,
    pub answer_query: bool,
}

// ---- Signals ---------------------------------------------------------------

/// The write end of the pipe that wakes up `poll()`.
static SIGNAL_PIPE: AtomicI32 = AtomicI32::new(-1);
/// A bit for each signal that came (the numbers are below 32).
static PENDING: AtomicU32 = AtomicU32::new(0);

#[cfg(any(target_os = "linux", target_os = "android"))]
unsafe fn errno_location() -> *mut c_int {
    libc::__errno_location()
}
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
unsafe fn errno_location() -> *mut c_int {
    libc::__error()
}

extern "C" fn on_signal(sig: c_int) {
    unsafe {
        // The code that was interrupted may be about to read errno.
        let saved = *errno_location();
        PENDING.fetch_or(1 << sig, Ordering::SeqCst);
        let fd = SIGNAL_PIPE.load(Ordering::Relaxed);
        if fd >= 0 {
            let byte = 0u8;
            libc::write(fd, (&byte as *const u8).cast(), 1);
        }
        *errno_location() = saved;
    }
}

const FORWARDED: [c_int; 4] = [libc::SIGHUP, libc::SIGTERM, libc::SIGINT, libc::SIGQUIT];

/// Makes the pipe and the handlers. Returns the read end of the pipe.
fn install_signals() -> io::Result<RawFd> {
    let mut fds = [0 as c_int; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    for fd in fds {
        set_cloexec(fd);
        set_nonblocking(fd)?;
    }
    SIGNAL_PIPE.store(fds[1], Ordering::SeqCst);

    let mut signals = vec![libc::SIGWINCH, libc::SIGCHLD];
    signals.extend(FORWARDED);
    for sig in signals {
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_signal as *const () as usize;
            libc::sigemptyset(&mut action.sa_mask);
            // No SA_RESTART: poll() returns, and the loop looks at the signal.
            action.sa_flags = if sig == libc::SIGCHLD { libc::SA_NOCLDSTOP } else { 0 };
            if libc::sigaction(sig, &action, std::ptr::null_mut()) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    Ok(fds[0])
}

// ---- Small helpers around file descriptors ------------------------------------

fn set_cloexec(fd: RawFd) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
}

/// Makes `fd` non-blocking. Returns the flags it had, to restore them.
fn set_nonblocking(fd: RawFd) -> io::Result<c_int> {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(flags)
    }
}

/// Bytes that wait to be written: `data[pos..]`.
#[derive(Default)]
struct Buf {
    data: Vec<u8>,
    pos: usize,
}

impl Buf {
    fn len(&self) -> usize {
        self.data.len() - self.pos
    }
    fn is_empty(&self) -> bool {
        self.pos == self.data.len()
    }
    /// For appending.
    fn vec(&mut self) -> &mut Vec<u8> {
        if self.is_empty() {
            self.data.clear();
            self.pos = 0;
        }
        &mut self.data
    }
    fn pending(&self) -> &[u8] {
        &self.data[self.pos..]
    }
    fn consume(&mut self, n: usize) {
        self.pos += n;
        if self.is_empty() {
            self.data.clear();
            self.pos = 0;
        } else if self.pos > 1 << 20 {
            self.data.drain(..self.pos);
            self.pos = 0;
        }
    }
    fn clear(&mut self) {
        self.data.clear();
        self.pos = 0;
    }
}

/// Writes as much of `buf` as `fd` takes now.
fn write_some(fd: RawFd, buf: &mut Buf) -> io::Result<()> {
    while !buf.is_empty() {
        let pending = buf.pending();
        let n = unsafe { libc::write(fd, pending.as_ptr().cast(), pending.len()) };
        if n > 0 {
            buf.consume(n as usize);
            continue;
        }
        if n == 0 {
            break;
        }
        let err = io::Error::last_os_error();
        match err.kind() {
            io::ErrorKind::Interrupted => continue,
            io::ErrorKind::WouldBlock => break,
            _ => return Err(err),
        }
    }
    Ok(())
}

enum Read {
    Data(usize),
    /// Nothing now.
    Empty,
    /// The other side is gone.
    Closed,
}

fn read_some(fd: RawFd, buf: &mut [u8]) -> Read {
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 {
            return Read::Data(n as usize);
        }
        if n == 0 {
            return Read::Closed;
        }
        match io::Error::last_os_error().kind() {
            io::ErrorKind::Interrupted => continue,
            io::ErrorKind::WouldBlock => return Read::Empty,
            // Linux: the end of a pty whose other side was closed.
            _ => return Read::Closed,
        }
    }
}

// ---- The terminal the shim runs in -----------------------------------------------

/// What to restore when the shim ends: the mode of the terminal, and the flags
/// of the descriptors that were made non-blocking.
struct TerminalGuard {
    termios: Option<libc::termios>,
    flags: Vec<(RawFd, c_int)>,
}

impl TerminalGuard {
    /// Puts the terminal (stdin) in raw mode: the keys go to the program as they
    /// are, and what the shim writes is not changed.
    fn new(original: Option<libc::termios>) -> io::Result<Self> {
        let mut guard = TerminalGuard { termios: original, flags: Vec::new() };
        if let Some(original) = &guard.termios {
            let mut raw = *original;
            unsafe {
                libc::cfmakeraw(&mut raw);
                if libc::tcsetattr(0, libc::TCSANOW, &raw) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
        }
        // Standard input and output are often one open file: it is made
        // non-blocking for the shim, and restored for whatever runs after it.
        for fd in [0, 1] {
            guard.flags.push((fd, set_nonblocking(fd)?));
        }
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        unsafe {
            for &(fd, flags) in &self.flags {
                libc::fcntl(fd, libc::F_SETFL, flags);
            }
            if let Some(original) = &self.termios {
                libc::tcsetattr(0, libc::TCSANOW, original);
            }
        }
    }
}

fn terminal_attributes() -> (Option<libc::termios>, Option<libc::winsize>) {
    unsafe {
        let mut termios: libc::termios = std::mem::zeroed();
        let mut size: libc::winsize = std::mem::zeroed();
        let t = (libc::tcgetattr(0, &mut termios) == 0).then_some(termios);
        let s = (libc::ioctl(0, libc::TIOCGWINSZ, &mut size) == 0).then_some(size);
        (t, s)
    }
}

// ---- The program ------------------------------------------------------------------

/// Opens a pty. Returns (master, slave).
fn open_pty(
    termios: Option<&libc::termios>,
    size: Option<&libc::winsize>,
) -> io::Result<(OwnedFd, OwnedFd)> {
    let (mut master, mut slave) = (0 as c_int, 0 as c_int);
    let termios_ptr = termios.map_or(std::ptr::null(), |t| t as *const libc::termios);
    let size_ptr = size.map_or(std::ptr::null(), |s| s as *const libc::winsize);
    let rc = unsafe {
        libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), termios_ptr as _, size_ptr as _)
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    set_cloexec(master);
    set_cloexec(slave);
    unsafe { Ok((OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave))) }
}

/// Starts the program with the slave as its terminal: its standard input,
/// output and error, and its controlling terminal (in a session of its own, so
/// that its Ctrl+C and its window size work). Returns its process id.
fn spawn(command: &[OsString], slave: OwnedFd) -> io::Result<libc::pid_t> {
    let mut cmd = Command::new(&command[0]);
    cmd.args(&command[1..]);
    cmd.stdin(Stdio::from(slave.try_clone()?));
    cmd.stdout(Stdio::from(slave.try_clone()?));
    cmd.stderr(Stdio::from(slave));
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    // `cmd` holds the other copies of the slave: it is dropped when this ends,
    // so that the shim has none of them, and the master sees the end.
    let child = cmd.spawn()?;
    Ok(child.id() as libc::pid_t)
}

/// The exit code of a child: its own, or 128 + the signal, as a shell reports it.
fn exit_code(status: c_int) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        1
    }
}

fn try_wait(pid: libc::pid_t) -> Option<i32> {
    let mut status: c_int = 0;
    loop {
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if r == pid {
            return Some(exit_code(status));
        }
        if r < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return None;
    }
}

fn kill_group(pid: libc::pid_t, sig: c_int) {
    // The program leads its session, so its process group is its pid.
    unsafe {
        libc::killpg(pid, sig);
    }
}

fn copy_window_size(master: RawFd) {
    unsafe {
        let mut size: libc::winsize = std::mem::zeroed();
        if libc::ioctl(0, libc::TIOCGWINSZ, &mut size) == 0 {
            libc::ioctl(master, libc::TIOCSWINSZ, &size);
        }
    }
}

fn drain_pipe(fd: RawFd) {
    let mut buf = [0u8; 64];
    while let Read::Data(_) = read_some(fd, &mut buf) {}
}

/// Runs `command` and proxies it. Returns the exit code of the command.
pub fn run(command: &[OsString], options: &Options) -> io::Result<i32> {
    let (termios, size) = terminal_attributes();
    let (master, slave) = open_pty(termios.as_ref(), size.as_ref())?;
    let master_fd = {
        use std::os::fd::AsRawFd;
        master.as_raw_fd()
    };
    set_nonblocking(master_fd)?;

    let signal_fd = install_signals()?;
    let _guard = TerminalGuard::new(termios)?;
    let pid = spawn(command, slave)?;

    let mut filter = Filter::new(options.wrap, options.answer_query);
    let mut to_parent = Buf::default(); // To the terminal (tmux)
    let mut to_child = Buf::default(); // To the program
    let mut replies = Vec::new();
    let mut read_buf = vec![0u8; READ_SIZE];
    let (mut master_closed, mut stdin_closed, mut parent_gone) = (false, false, false);
    let mut status: Option<i32> = None;
    let mut kill_at: Option<Instant> = None;

    loop {
        // ---- What to wait for
        let mut fds = [
            pollfd { fd: 0, events: 0, revents: 0 },
            pollfd { fd: 1, events: 0, revents: 0 },
            pollfd { fd: master_fd, events: 0, revents: 0 },
            pollfd { fd: signal_fd, events: POLLIN, revents: 0 },
        ];
        if !stdin_closed && to_child.len() < BUF_LIMIT {
            fds[0].events = POLLIN;
        }
        if !to_parent.is_empty() {
            fds[1].events = POLLOUT;
        }
        if !master_closed {
            if to_parent.len() < BUF_LIMIT {
                fds[2].events |= POLLIN;
            }
            if !to_child.is_empty() {
                fds[2].events |= POLLOUT;
            }
        }
        let mut timeout: c_int = -1;
        if filter.has_undecided() {
            timeout = UNDECIDED_MS;
        }
        if master_closed && status.is_none() {
            timeout = 50; // The pty is closed: the program is about to end.
        }
        if let Some(at) = kill_at {
            let ms = at.saturating_duration_since(Instant::now()).as_millis().min(1 << 30) as c_int;
            timeout = if timeout < 0 { ms } else { timeout.min(ms) };
        }

        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout) };
        if ready < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(io::Error::last_os_error());
        }

        // ---- Signals
        if fds[3].revents != 0 {
            drain_pipe(signal_fd);
        }
        let signals = PENDING.swap(0, Ordering::SeqCst);
        if signals & (1 << libc::SIGWINCH) != 0 {
            copy_window_size(master_fd);
        }
        for sig in FORWARDED {
            if signals & (1 << sig) != 0 {
                kill_group(pid, sig);
                if (sig == libc::SIGHUP || sig == libc::SIGTERM) && kill_at.is_none() {
                    kill_at = Some(Instant::now() + KILL_AFTER);
                }
            }
        }
        if status.is_none() && (signals & (1 << libc::SIGCHLD) != 0 || master_closed) {
            status = try_wait(pid);
        }
        if let Some(at) = kill_at {
            if status.is_none() && Instant::now() >= at {
                kill_group(pid, libc::SIGKILL);
                kill_at = None;
            }
        }

        // ---- The program wrote something
        if !master_closed && fds[2].revents & (POLLIN | POLLHUP | POLLERR) != 0 {
            for _ in 0..8 {
                if to_parent.len() >= BUF_LIMIT {
                    break;
                }
                match read_some(master_fd, &mut read_buf) {
                    Read::Data(n) => {
                        let out = if parent_gone { &mut Vec::new() } else { to_parent.vec() };
                        filter.feed(&read_buf[..n], out, &mut replies);
                        if !replies.is_empty() {
                            to_child.vec().extend_from_slice(&replies);
                            replies.clear();
                        }
                    }
                    Read::Empty => break,
                    Read::Closed => {
                        master_closed = true;
                        break;
                    }
                }
            }
        }

        // ---- The user typed something (or the terminal went away)
        if !stdin_closed && fds[0].revents & (POLLIN | POLLHUP | POLLERR) != 0 {
            match read_some(0, &mut read_buf) {
                Read::Data(n) => to_child.vec().extend_from_slice(&read_buf[..n]),
                Read::Empty => {}
                Read::Closed => {
                    // The pane was closed: the program gets what a terminal that
                    // goes away sends it.
                    stdin_closed = true;
                    kill_group(pid, libc::SIGHUP);
                    kill_at.get_or_insert(Instant::now() + KILL_AFTER);
                }
            }
        }

        // ---- Send on what is waiting
        if filter.has_undecided() && ready == 0 && !parent_gone {
            filter.flush_undecided(to_parent.vec());
        }
        if !parent_gone && write_some(1, &mut to_parent).is_err() {
            // Nobody reads the output any more: keep the program from blocking,
            // and ask it to end.
            parent_gone = true;
            to_parent.clear();
            kill_group(pid, libc::SIGHUP);
            kill_at.get_or_insert(Instant::now() + KILL_AFTER);
        }
        if !master_closed && write_some(master_fd, &mut to_child).is_err() {
            master_closed = true;
        }

        // ---- The end
        if let Some(code) = status {
            // The program ended, but what it wrote may still be on its way
            // through the pty (an empty read is not the end): read until the pty
            // says it is closed, or until nothing came for a while.
            if !master_closed {
                let give_up = Instant::now() + Duration::from_secs(2);
                let mut quiet = 0;
                while quiet < 5 && Instant::now() < give_up {
                    match read_some(master_fd, &mut read_buf) {
                        Read::Data(n) => {
                            quiet = 0;
                            let out = if parent_gone { &mut Vec::new() } else { to_parent.vec() };
                            filter.feed(&read_buf[..n], out, &mut replies);
                            if !parent_gone && to_parent.len() >= BUF_LIMIT {
                                if write_some(1, &mut to_parent).is_err() {
                                    parent_gone = true;
                                    to_parent.clear();
                                } else if to_parent.len() >= BUF_LIMIT {
                                    let mut out = [pollfd { fd: 1, events: POLLOUT, revents: 0 }];
                                    unsafe { libc::poll(out.as_mut_ptr(), 1, 10) };
                                }
                            }
                        }
                        Read::Closed => break,
                        Read::Empty => {
                            let mut wait = [pollfd { fd: master_fd, events: POLLIN, revents: 0 }];
                            if unsafe { libc::poll(wait.as_mut_ptr(), 1, 20) } == 0 {
                                quiet += 1;
                            }
                        }
                    }
                }
            }
            if !parent_gone {
                filter.finish(to_parent.vec());
                for _ in 0..20 {
                    if write_some(1, &mut to_parent).is_err() || to_parent.is_empty() {
                        break;
                    }
                    let mut out = [pollfd { fd: 1, events: POLLOUT, revents: 0 }];
                    unsafe { libc::poll(out.as_mut_ptr(), 1, 100) };
                }
            }
            return Ok(code);
        }
    }
}
