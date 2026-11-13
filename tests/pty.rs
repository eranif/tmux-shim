//! Runs the real binary on a pty (the pane of tmux, as it is for the shim), with
//! a program that writes the sequences, and looks at what comes out.
#![cfg(unix)]

use libc::{c_int, pollfd, POLLIN};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

struct Run {
    output: Vec<u8>,
    code: i32,
}

/// Runs the shim with `args` on a new pty, with `TMUX` set to `tmux` (or
/// removed), and reads everything it writes.
fn run(args: &[&str], tmux: Option<&str>) -> Run {
    let (mut master, mut slave) = (0 as c_int, 0 as c_int);
    let size = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &size,
        )
    };
    assert_eq!(rc, 0, "openpty");
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tmux-shim"));
    cmd.args(args)
        .env_remove("TMUX")
        .env_remove("STY")
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    if let Some(value) = tmux {
        cmd.env("TMUX", value);
    }
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            libc::ioctl(0, libc::TIOCSCTTY as _, 0);
            Ok(())
        });
    }
    let mut child = cmd.spawn().expect("start the shim");
    drop(cmd);

    let mut output = Vec::new();
    let end = Instant::now() + Duration::from_secs(10);
    let mut buf = [0u8; 4096];
    use std::os::fd::AsRawFd;
    while Instant::now() < end {
        let mut fds = [pollfd {
            fd: master.as_raw_fd(),
            events: POLLIN,
            revents: 0,
        }];
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 1, 100) };
        if ready > 0 {
            let n = unsafe { libc::read(master.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break; // The shim and the program are gone.
            }
            output.extend_from_slice(&buf[..n as usize]);
        }
    }
    let code = child.wait().unwrap().code().unwrap_or(-1);
    Run { output, code }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn a_report_is_wrapped_for_tmux() {
    let r = run(
        &["sh", "-c", r#"printf '\033]7501;state=working\007'"#],
        Some("/tmp/x,1,0"),
    );
    assert!(
        contains(
            &r.output,
            b"\x1bPtmux;\x1b\x1b]7501;state=working\x07\x1b\\"
        ),
        "{:?}",
        String::from_utf8_lossy(&r.output)
    );
    assert_eq!(r.code, 0);
}

#[test]
fn the_exit_code_and_the_text_are_kept() {
    let r = run(&["sh", "-c", "echo hello; exit 7"], Some("/tmp/x,1,0"));
    assert!(contains(&r.output, b"hello"));
    assert_eq!(r.code, 7);
}

#[test]
fn other_sequences_are_not_touched() {
    let r = run(
        &[
            "sh",
            "-c",
            r#"printf '\033]0;a title\007\033[31mred\033[0m'"#,
        ],
        Some("/tmp/x,1,0"),
    );
    assert!(contains(&r.output, b"\x1b]0;a title\x07\x1b[31mred\x1b[0m"));
    assert!(!contains(&r.output, b"Ptmux"));
}

#[test]
fn the_program_has_a_terminal() {
    let r = run(
        &[
            "sh",
            "-c",
            "test -t 0 && test -t 1 && echo tty-ok; stty size",
        ],
        Some("/tmp/x,1,0"),
    );
    assert!(
        contains(&r.output, b"tty-ok"),
        "{:?}",
        String::from_utf8_lossy(&r.output)
    );
    // The size of the pane is the size of the program's terminal.
    assert!(
        contains(&r.output, b"24 80"),
        "{:?}",
        String::from_utf8_lossy(&r.output)
    );
}

#[test]
fn the_query_is_answered_by_the_shim() {
    // The program asks, reads 12 bytes of the answer, and prints them in hex.
    let script =
        r#"stty raw -echo; printf '\033]7501;?\033\\'; dd bs=1 count=12 2>/dev/null | od -An -tx1"#;
    let r = run(&["sh", "-c", script], Some("/tmp/x,1,0"));
    let text = String::from_utf8_lossy(&r.output).to_string();
    assert!(
        text.contains("1b 5d 37 35 30 31 3b 3f 3a 73 74 61"),
        "{text:?}"
    );
    // The question itself did not go on to the terminal.
    assert!(!contains(&r.output, b"\x1b]7501;?"), "{text:?}");
}

#[test]
fn without_tmux_the_program_just_runs() {
    let r = run(
        &[
            "sh",
            "-c",
            r#"printf '\033]7501;state=working\007'; test -t 0 && echo tty"#,
        ],
        None,
    );
    // Not wrapped, and the program has the terminal of the shim itself.
    assert!(contains(&r.output, b"\x1b]7501;state=working\x07"));
    assert!(!contains(&r.output, b"Ptmux"));
    assert!(contains(&r.output, b"tty"));
    assert_eq!(r.code, 0);
}

#[test]
fn a_program_that_is_not_there_is_reported() {
    let r = run(&["no-such-program-for-tmux-shim"], Some("/tmp/x,1,0"));
    assert_eq!(r.code, 127);
    assert!(contains(&r.output, b"no-such-program-for-tmux-shim"));
}

#[test]
fn a_big_output_is_not_lost() {
    // 20 MB through the shim, with a report in the middle. The program waits at
    // the end: when the leader of a session exits, Linux hangs up its terminal and
    // throws away what nobody has read yet, so a program that writes at full speed
    // and ends at once loses its tail (in any terminal, not only here).
    let script = r#"head -c 10000000 /dev/zero | tr '\0' 'a'; printf '\033]7501;state=done\007'; head -c 10000000 /dev/zero | tr '\0' 'b'; sleep 1"#;
    let r = run(&["sh", "-c", script], Some("/tmp/x,1,0"));
    let a = r.output.iter().filter(|&&b| b == b'a').count();
    let b = r.output.iter().filter(|&&b| b == b'b').count();
    // The "a" of "state" is one of the a's.
    assert_eq!((a, b), (10_000_001, 10_000_000));
    assert!(contains(
        &r.output,
        b"\x1bPtmux;\x1b\x1b]7501;state=done\x07\x1b\\"
    ));
}

/// A tmux server of its own (a private socket), and the value of
/// `allow-passthrough` in the pane that runs the shim with `shim_args`.
/// `None` when there is no tmux on this machine.
fn passthrough_in_pane(shim_args: &str) -> Option<String> {
    let tmux = |args: &[&str]| {
        Command::new("tmux")
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .args(args)
            .output()
    };
    let socket = format!("shim-test-{}-{}", std::process::id(), shim_args.len());
    let shim = env!("CARGO_BIN_EXE_tmux-shim");
    let started = tmux(&[
        "-L",
        &socket,
        "-f",
        "/dev/null",
        "new-session",
        "-d",
        &format!("{shim} {shim_args} -- sleep 5"),
    ])
    .ok()?;
    if !started.status.success() {
        return None;
    }
    // The shim sets the option as the first thing it does, but in its own time.
    std::thread::sleep(Duration::from_millis(1000));
    let shown = tmux(&["-L", &socket, "show-options", "-p", "allow-passthrough"]);
    let _ = tmux(&["-L", &socket, "kill-server"]);
    Some(
        String::from_utf8_lossy(&shown.ok()?.stdout)
            .trim()
            .to_string(),
    )
}

#[test]
fn tmux_is_told_to_let_the_reports_through() {
    // Skipped when there is no tmux (or when it is too old for a pane option).
    if let Some(value) = passthrough_in_pane("") {
        assert_eq!(value, "allow-passthrough on");
    }
}

#[test]
fn the_setup_of_tmux_can_be_turned_off() {
    if let Some(value) = passthrough_in_pane("--no-tmux-setup") {
        assert_eq!(value, "");
    }
}
