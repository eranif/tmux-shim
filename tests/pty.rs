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

/// A tmux server of its own (a private socket) with one pane that runs the
/// shim with `shim_args`, and the answers of tmux to `queries` (each one is the
/// arguments of a tmux command) once the shim has started. Each answer is
/// trimmed. `None` when there is no tmux on this machine.
fn in_tmux(shim_args: &[&str], queries: &[&[&str]]) -> Option<Vec<String>> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    let tmux = |args: &[&str]| {
        Command::new("tmux")
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .args(args)
            .output()
    };
    let socket = format!(
        "shim-test-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    );
    let shim = env!("CARGO_BIN_EXE_tmux-shim");
    let command = format!("{shim} {} -- sleep 5", shim_args.join(" "));
    let started = tmux(&[
        "-L",
        &socket,
        "-f",
        "/dev/null",
        "new-session",
        "-d",
        &command,
    ])
    .ok();
    if !started.is_some_and(|o| o.status.success()) {
        // CI sets the variable: there a missing tmux is an error, not a skip.
        assert!(
            std::env::var_os("TMUX_SHIM_REQUIRE_TMUX").is_none(),
            "tmux does not start, and TMUX_SHIM_REQUIRE_TMUX is set"
        );
        return None;
    }
    // The shim does its part as the first thing, but in its own time.
    std::thread::sleep(Duration::from_millis(1000));
    let answers = queries
        .iter()
        .map(|query| {
            let mut args = vec!["-L", socket.as_str()];
            args.extend_from_slice(query);
            tmux(&args).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        })
        .collect::<Result<Vec<_>, _>>();
    let _ = tmux(&["-L", &socket, "kill-server"]);
    answers.ok()
}

/// `allow-passthrough` that the pane of the shim has (its own value, or the one
/// of the server).
const PASSTHROUGH: &[&str] = &["show-options", "-pAv", "allow-passthrough"];

/// A file for `--config`, in a directory of its own.
fn config_file(name: &str, text: &str) -> String {
    let dir = std::env::temp_dir().join(format!("tmux-shim-test-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("tmux.conf");
    std::fs::write(&file, text).unwrap();
    file.to_string_lossy().into_owned()
}

#[test]
fn tmux_is_told_to_let_the_reports_through() {
    // Skipped when there is no tmux (or when it is too old for a pane option).
    if let Some(answers) = in_tmux(&[], &[PASSTHROUGH]) {
        assert_eq!(answers, ["on"]);
    }
}

#[test]
fn the_setup_of_tmux_can_be_turned_off() {
    if let Some(answers) = in_tmux(&["--no-tmux-setup"], &[PASSTHROUGH]) {
        assert_eq!(answers, ["off"]);
    }
}

#[test]
fn the_config_file_is_run_by_tmux() {
    let conf = config_file(
        "run",
        "set -g mouse on\nsetw -g mode-keys vi\nset -g set-clipboard on\nset -g allow-passthrough all\n",
    );
    let queries: &[&[&str]] = &[
        &["show-options", "-gv", "mouse"],
        &["show-window-options", "-gv", "mode-keys"],
        &["show-options", "-gv", "set-clipboard"],
        PASSTHROUGH,
    ];
    if let Some(answers) = in_tmux(&["-c", &conf], queries) {
        // "all" from the file is not replaced by the "on" of the shim.
        assert_eq!(answers, ["on", "vi", "on", "all"]);
    }
}

#[test]
fn the_config_file_may_be_given_with_an_equals_sign() {
    let conf = config_file("equals", "set -g mouse on\n");
    let option = format!("--config={conf}");
    if let Some(answers) = in_tmux(&[&option], &[&["show-options", "-gv", "mouse"]]) {
        assert_eq!(answers, ["on"]);
    }
}

#[test]
fn a_config_file_that_is_not_there_is_an_error() {
    let r = run(&["-c", "/no/such/file", "true"], Some("/tmp/x,1,0"));
    assert_eq!(r.code, 2);
    assert!(contains(&r.output, b"/no/such/file"));
}

#[test]
fn a_string_of_commands_is_run_by_tmux() {
    // The string of the user, a `;` inside quotes, and a file before it: they run in
    // the order of the command line, and allow-passthrough "all" is kept.
    let conf = config_file("string", "set -g mouse off\nset -g mode-keys emacs\n");
    let commands = "set -g mouse on; setw -g mode-keys vi;set -g set-clipboard on ; \
                    set -g allow-passthrough all; set -g status-left 'a; b'";
    let option = format!("--config-string={commands}");
    let queries: &[&[&str]] = &[
        &["show-options", "-gv", "mouse"],
        &["show-window-options", "-gv", "mode-keys"],
        &["show-options", "-gv", "set-clipboard"],
        &["show-options", "-gv", "status-left"],
        PASSTHROUGH,
    ];
    // The shell of tmux takes the string, so it is quoted for it.
    let quoted = format!("'{}'", option.replace('\'', "'\\''"));
    if let Some(answers) = in_tmux(&["-c", &conf, &quoted], queries) {
        assert_eq!(answers, ["on", "vi", "on", "a; b", "all"]);
    }
}

#[test]
fn a_string_of_commands_may_be_a_separate_argument() {
    let queries: &[&[&str]] = &[&["show-options", "-gv", "mouse"]];
    if let Some(answers) = in_tmux(&["--config-string", "'set -g mouse on'"], queries) {
        assert_eq!(answers, ["on"]);
    }
}

#[test]
fn an_empty_string_of_commands_is_an_error() {
    let r = run(&["--config-string", "  ", "true"], Some("/tmp/x,1,0"));
    assert_eq!(r.code, 2);
    assert!(contains(&r.output, b"--config-string"));
}
