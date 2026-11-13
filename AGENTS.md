# AGENTS.md

Guidance for AI coding agents (and people) working in this repository.

## What this project is

`tmux-shim` is a small Unix command line program, written in Rust:

```
tmux-shim [--wrap auto|tmux|screen|none] [--no-reply] [--no-tmux-setup] [--] COMMAND [ARGS...]
```

It runs `COMMAND` on a pty of its own and sits between that program and the terminal it
was started in (normally a tmux or screen pane). The goal is to let a program inside a
terminal multiplexer report to the terminal **around** the multiplexer.

Why it is needed: tmux drops every escape sequence it does not know. A program that
reports its status with `OSC 7501` (the Program Status Protocol: working / done /
blocked), a progress bar with `OSC 9;4`, or its folder with `OSC 7` is not heard outside
tmux, unless the program wraps the sequence in `ESC P tmux ; <ESC doubled> ESC \` and tmux
has `allow-passthrough` on. Most programs do not wrap (Claude Code, for one, sends the
`7501` question and reports plain). Worse, a program learns that the terminal supports
`OSC 7501` by asking `OSC 7501 ; ?` and waiting for the answer, and tmux has no path to
carry the answer back, so such programs never report at all inside tmux.

What the shim does with the output of the program:

* `OSC 7501 ; ?` is removed and **answered by the shim** (written back to the program).
* `OSC 7501 ; ...`, `OSC 9 ; 4 ; ...`, `OSC 9 ; 9 ; ...`, `OSC 7 ; ...` are **wrapped** for
  tmux (or screen).
* Everything else is copied unchanged. A DCS/APC/PM/SOS string is copied whole, so a
  sequence the program wrapped itself is never wrapped twice.
* The input (keys) is copied without being looked at.

Outside tmux and screen (no `$TMUX`, no `$STY`) in `auto` mode the shim just `exec`s the
program.

### Where it is used

It was built for [Kennel](https://github.com/eranif/kennel) (a desktop app that runs AI CLI
agents side by side; its terminal shows the state of a program on its tab, from OSC 7501).
Kennel runs agents inside tmux so they survive a closed window or a dropped ssh connection,
and runs them as `tmux-shim -- <agent>`. Kennel's `RemoteInstaller` downloads the release
binary, checks its SHA-256 (pinned in Kennel) and puts it in `~/.kennel/bin` on the machine
where the agent runs. The shim itself knows nothing about Kennel and must stay generic.

Release assets (see `.github/workflows/release.yml`): a static musl binary named
`tmux-shim-x86_64-linux` (the name matches `uname -m`), with a `.sha256` file. Only x86_64
is built (aarch64 was dropped). Do not rename it: Kennel depends on the name.

## Layout

| File | What |
|---|---|
| `src/filter.rs` | The filter: a state machine over the output bytes. Pure, portable, no I/O. |
| `src/proxy.rs` | The pty, the `poll()` loop, signals, the terminal mode. Unix only, uses `libc`. |
| `src/main.rs` | The command line, and the choice of the wrap mode. |
| `tests/pty.rs` | Integration tests: the real binary on a pty, with `sh` as the program. |

Only `libc` and `memchr` are dependencies. Keep it that way: this is a tiny program that is
installed on other people's machines.

## Build and test

Unix only (Linux and macOS). On Windows use WSL, and put the target directory outside
`/mnt/c` (`CARGO_TARGET_DIR=$HOME/.cache/tmux-shim-target`), it is much faster.

```sh
cargo fmt
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --release --target x86_64-unknown-linux-musl   # the static binary
```

CI runs the same (`.github/workflows/ci.yml`); fmt and clippy warnings fail it.

## Coding guidelines

General:

* Keep it small and boring. Prefer the standard library; do not add a dependency, an
  async runtime or a thread for something that a few lines can do.
* Do the simple thing first, and explain a non-obvious choice in a comment that says
  **why** (what the kernel or tmux does), not what the line does.
* Run `cargo fmt` and fix every clippy warning before a commit.
* Errors are `io::Result`; do not `unwrap()` in the program (tests may). The release
  profile aborts on panic, so a panic ends the program and the pane.
* `unsafe` only for libc calls, in `proxy.rs`, each with the reason it is safe when it is not
  obvious. `filter.rs` has no `unsafe` and no I/O: keep it that way, so it can be tested
  anywhere.

The filter (`filter.rs`):

* **The result must not depend on how the input is cut into reads.** A read can end in the
  middle of a sequence, anywhere. Any change must keep
  `the_result_does_not_depend_on_how_the_input_is_cut` passing, and a new case goes into its
  inputs.
* It never loses or reorders a byte it does not rewrite. When unsure, pass the bytes on
  unchanged: a missed rewrite only costs a status icon, a lost byte corrupts the screen.
* Hold back as little as possible (`MAX_HELD`), and never wait for more input to decide
  about plain text. The fast path for text is one `memchr`.

The proxy (`proxy.rs`):

* One thread and `poll()`. Both sides are non-blocking with a bounded buffer; stop reading
  from one side while the other is full, so memory stays bounded and neither direction blocks
  the other.
* Signal handlers do only async-signal-safe work (an atomic and a `write` to the self-pipe,
  and they restore `errno`). The loop does the rest.
* Restore the terminal (mode and `O_NONBLOCK`) on every way out: `TerminalGuard` does it in
  `Drop`.
* Know the Linux behavior this relies on: when the leader of a session exits, its terminal is
  hung up and the output nobody has read yet is discarded. A program that writes at full
  speed and ends at once can lose its tail (in any terminal). Tests that write a lot end with
  a `sleep` for that reason.

Tests:

* A new rewrite rule needs a unit test in `filter.rs`; a change in how the process is run
  needs one in `tests/pty.rs`.
* The pty tests use `sh`, `dd`, `od`, `stty` with GNU options, so they run on Linux only (CI
  runs only the unit tests on macOS).

## Facts worth knowing (checked with tmux 3.4)

* tmux drops these OSC sequences when they are sent plain, whatever `allow-passthrough` is
  set to. When wrapped in the DCS form and `allow-passthrough on` (or `all`, 3.4+, for panes
  that are not visible), tmux forwards them to the outer terminal as plain sequences. The
  support question wrapped this way reaches the outer terminal too, and the answer comes back
  into the pane as input, but the shim answers by itself: faster, and it does not depend on
  the outer terminal.
* OSC 52 (copy) and the title do not need wrapping: `set -g set-clipboard on` and
  `set -g set-titles on` are enough.
* `allow-passthrough` is a pane option (3.3+), so the shim turns it on for its own pane at
  startup (`tmux set-option -p allow-passthrough on`, errors ignored, `--no-tmux-setup` skips
  it). It cannot do it for other panes or set `all`: whoever starts tmux does that.

## Commits

* Sign off every commit (`git commit -s`).
* Do not add a `Co-Authored-By:` line.
* Do not push, amend or force anything without explicit approval for that action.
* Do not commit editor files (`*.swp`) or build output (`target/`).
* Keep the subject short and say what changed and why in the body.
