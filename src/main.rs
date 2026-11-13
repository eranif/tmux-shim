//! tmux-shim: runs a program so that what it reports to its terminal reaches the
//! terminal around tmux (or screen).
//!
//! ```text
//! tmux-shim [--wrap auto|tmux|screen|none] [--no-reply] [-c FILE] [--config-string COMMANDS] [--no-tmux-setup] -- COMMAND [ARGS...]
//! ```
//!
//! See the README.

#[cfg(unix)]
mod config;
mod filter;
#[cfg(unix)]
mod proxy;

#[cfg(unix)]
fn main() {
    use filter::Wrap;
    use std::ffi::OsString;
    use std::os::unix::process::CommandExt;

    const USAGE: &str = "\
Usage: tmux-shim [OPTIONS] [--] COMMAND [ARGS...]

Runs COMMAND on a terminal of its own, and passes what it writes on. The
sequences that tmux and screen drop (OSC 7501 program status, OSC 9;4 progress,
OSC 9;9 and OSC 7 working folder) are wrapped, so that they reach the terminal
around the multiplexer. OSC 7501;? (does the terminal support the status
protocol?) is answered by the shim.

Outside tmux and screen (no $TMUX, no $STY) the shim just starts COMMAND.

Inside tmux, tmux forwards the wrapped sequences only when allow-passthrough
is on. When it is off, the shim runs `tmux set-option -p allow-passthrough on`
for its own pane (tmux 3.3 or later; the other panes are not touched; `on` and
`all` are left as they are). If that fails, nothing is lost but the status icon.

Options:
  --wrap MODE   auto (default: tmux if $TMUX is set, screen if $STY is set,
                otherwise none), tmux, screen, or none
  --no-reply    do not answer OSC 7501;? (pass it on instead)
  -c, --config FILE
                inside tmux, apply the tmux commands of FILE (one per line,
                `#` for comments) before the program starts
  --config-string COMMANDS
                the same, for commands in the argument, separated by `;`:
                'set -g mouse on; setw -g mode-keys vi'
  --no-tmux-setup
                do not turn on allow-passthrough for the pane (see below)
  -h, --help    show this text
  -V, --version show the version
";

    #[derive(PartialEq)]
    enum Mode {
        Auto,
        Fixed(Wrap),
    }

    let mut mode = Mode::Auto;
    let mut answer_query = true;
    let mut tmux_setup = true;
    // -c and --config-string, in the order of the command line (they can be
    // repeated).
    let mut setup: Vec<Setup> = Vec::new();
    let mut command: Vec<OsString> = Vec::new();

    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        let text = arg.to_string_lossy().into_owned();
        let parse_mode = |value: &str| -> Option<Mode> {
            match value {
                "auto" => Some(Mode::Auto),
                "tmux" => Some(Mode::Fixed(Wrap::Tmux)),
                "screen" => Some(Mode::Fixed(Wrap::Screen)),
                "none" => Some(Mode::Fixed(Wrap::None)),
                _ => None,
            }
        };
        match text.as_str() {
            "--" => {
                command.extend(args.by_ref());
                break;
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                return;
            }
            "-V" | "--version" => {
                println!("tmux-shim {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--no-reply" => answer_query = false,
            "--no-tmux-setup" => tmux_setup = false,
            "-c" | "--config" => match args.next() {
                Some(file) => setup.push(Setup::from_file(&file)),
                None => usage_error("--config needs a file"),
            },
            _ if text.starts_with("--config=") => {
                let file = OsString::from(&text["--config=".len()..]);
                setup.push(Setup::from_file(&file));
            }
            "--config-string" => match args.next() {
                Some(commands) => setup.push(Setup::from_string(&commands)),
                None => usage_error("--config-string needs tmux commands"),
            },
            _ if text.starts_with("--config-string=") => {
                let commands = OsString::from(&text["--config-string=".len()..]);
                setup.push(Setup::from_string(&commands));
            }
            "--wrap" => {
                let value = args.next().map(|v| v.to_string_lossy().into_owned());
                match value.as_deref().and_then(parse_mode) {
                    Some(m) => mode = m,
                    None => usage_error("--wrap needs one of: auto, tmux, screen, none"),
                }
            }
            _ if text.starts_with("--wrap=") => match parse_mode(&text["--wrap=".len()..]) {
                Some(m) => mode = m,
                None => usage_error("--wrap needs one of: auto, tmux, screen, none"),
            },
            _ if text.starts_with('-') && text.len() > 1 => {
                usage_error(&format!("unknown option {text}"));
            }
            _ => {
                command.push(arg);
                command.extend(args.by_ref());
                break;
            }
        }
    }
    if command.is_empty() {
        usage_error("no command");
    }

    let in_multiplexer = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
    let wrap = match mode {
        Mode::Fixed(wrap) => wrap,
        Mode::Auto if in_multiplexer("TMUX") => Wrap::Tmux,
        Mode::Auto if in_multiplexer("STY") => Wrap::Screen,
        Mode::Auto => {
            // Nothing drops the sequences here: there is nothing to do. The
            // program takes the place of the shim.
            let err = std::process::Command::new(&command[0])
                .args(&command[1..])
                .exec();
            fail_to_start(&command[0], &err);
        }
    };

    if wrap == Wrap::Tmux && in_multiplexer("TMUX") {
        // Before allow_passthrough(): they may set allow-passthrough themselves.
        for item in &setup {
            run_setup(item);
        }
        if tmux_setup {
            allow_passthrough();
        }
    }

    let options = proxy::Options { wrap, answer_query };
    match proxy::run(&command, &options) {
        Ok(code) => std::process::exit(code),
        Err(err) => fail_to_start(&command[0], &err),
    }

    /// Gives every command of `--config` and `--config-string` to tmux, one by
    /// one, in the order of the command line. A command is a `source-file -`
    /// that gets it on stdin: tmux does not take a string of commands as one
    /// argument, and `source-file` knows the quotes of its own syntax. One by
    /// one, so that a command that tmux does not accept does not take the others
    /// with it. What tmux says about it goes to stderr with the file and the
    /// line, and the program is started anyway.
    fn run_setup(item: &Setup) {
        use std::io::Write;
        use std::process::{Command, Stdio};
        for (line, command) in &item.commands {
            let Ok(mut child) = Command::new("tmux")
                .args(["source-file", "-"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
            else {
                // No tmux: the next ones would not start either.
                return;
            };
            if let Some(mut stdin) = child.stdin.take() {
                // One command: it fits in the pipe. The end of the input is
                // the drop.
                let _ = writeln!(stdin, "{command}");
            }
            let Ok(output) = child.wait_with_output() else {
                continue;
            };
            if !output.status.success() {
                // tmux says it on stdout (or on stderr), and names the line of
                // its own input: "-:1: unknown command".
                let mut said = String::from_utf8_lossy(&output.stdout).into_owned();
                said.push_str(&String::from_utf8_lossy(&output.stderr));
                let said = said.trim();
                let said = said.strip_prefix("-:1: ").unwrap_or(said);
                eprintln!("tmux-shim: {}:{line}: {said}", item.name);
            }
        }
    }

    /// Turns `allow-passthrough` on for the pane of the shim, when it is off.
    /// It is a pane option since tmux 3.3, and tmux picks the pane from
    /// `$TMUX_PANE`, so a program in a pane can do it for itself and for nobody
    /// else. A pane value overrides the one of the whole server, so an `all`
    /// that the user (or `--config`) set must not be replaced by `on`: it is
    /// asked for first (`-A`: the value the pane inherits too). It runs before
    /// the program starts: the first report is not lost. Every failure (no
    /// `tmux` on the PATH, an old tmux, a server that is gone) is ignored:
    /// without it the reports are dropped by tmux, as they are without the shim.
    fn allow_passthrough() {
        use std::process::{Command, Stdio};
        let current = Command::new("tmux")
            .args(["show-options", "-pAv", "allow-passthrough"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output();
        let is_off = current.is_ok_and(|o| o.status.success() && o.stdout.trim_ascii() == b"off");
        if is_off {
            let _ = Command::new("tmux")
                .args(["set-option", "-p", "allow-passthrough", "on"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }

    /// What `--config` or `--config-string` brings: the text, and the name that
    /// the errors use for it. The file is read here, in the command line: a file
    /// that is not there is a mistake, better told now than ignored.
    struct Setup {
        name: String,
        commands: Vec<(usize, String)>,
    }

    impl Setup {
        fn from_file(file: &std::ffi::OsStr) -> Setup {
            let name = file.to_string_lossy().into_owned();
            match std::fs::read_to_string(file) {
                Ok(text) => Setup {
                    name,
                    commands: config::file_commands(&text),
                },
                Err(err) => usage_error(&format!("--config {name}: {err}")),
            }
        }

        /// Commands that are only blanks are a mistake in the command line.
        fn from_string(commands: &std::ffi::OsStr) -> Setup {
            let commands = config::string_commands(&commands.to_string_lossy());
            if commands.is_empty() {
                usage_error("--config-string needs tmux commands");
            }
            Setup {
                name: "--config-string".to_string(),
                commands,
            }
        }
    }

    fn usage_error(message: &str) -> ! {
        eprintln!("tmux-shim: {message}\nTry 'tmux-shim --help'.");
        std::process::exit(2);
    }

    fn fail_to_start(program: &OsString, err: &std::io::Error) -> ! {
        eprintln!("tmux-shim: {}: {err}", program.to_string_lossy());
        let code = if err.kind() == std::io::ErrorKind::NotFound {
            127
        } else {
            126
        };
        std::process::exit(code);
    }
}

#[cfg(not(unix))]
fn main() {
    eprintln!("tmux-shim runs on Unix systems only");
    std::process::exit(1);
}
