//! tmux-shim: runs a program so that what it reports to its terminal reaches the
//! terminal around tmux (or screen).
//!
//! ```text
//! tmux-shim [--wrap auto|tmux|screen|none] [--no-reply] -- COMMAND [ARGS...]
//! ```
//!
//! See the README.

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

Options:
  --wrap MODE   auto (default: tmux if $TMUX is set, screen if $STY is set,
                otherwise none), tmux, screen, or none
  --no-reply    do not answer OSC 7501;? (pass it on instead)
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
            let err = std::process::Command::new(&command[0]).args(&command[1..]).exec();
            fail_to_start(&command[0], &err);
        }
    };

    let options = proxy::Options { wrap, answer_query };
    match proxy::run(&command, &options) {
        Ok(code) => std::process::exit(code),
        Err(err) => fail_to_start(&command[0], &err),
    }

    fn usage_error(message: &str) -> ! {
        eprintln!("tmux-shim: {message}\nTry 'tmux-shim --help'.");
        std::process::exit(2);
    }

    fn fail_to_start(program: &OsString, err: &std::io::Error) -> ! {
        eprintln!("tmux-shim: {}: {err}", program.to_string_lossy());
        let code = if err.kind() == std::io::ErrorKind::NotFound { 127 } else { 126 };
        std::process::exit(code);
    }
}

#[cfg(not(unix))]
fn main() {
    eprintln!("tmux-shim runs on Unix systems only");
    std::process::exit(1);
}
