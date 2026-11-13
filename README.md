# tmux-shim

Runs a program so that what it reports to its terminal still reaches the terminal
**around tmux** (or screen).

tmux drops every escape sequence it does not know. A program in a tmux pane that
reports its status with `OSC 7501` (working / done / blocked), a progress bar with
`OSC 9;4`, or its folder with `OSC 7`, is not heard by the terminal around tmux. tmux
can forward them (`set -g allow-passthrough on`), but only when the program wraps them
in `ESC P tmux ; ... ESC \`, and most programs do not. Programs that ask the terminal
first (`OSC 7501 ; ?`, "do you support this?") never get an answer, because tmux has no
way to carry it back.

`tmux-shim` sits between tmux and the program:

```
tmux pane  <->  tmux-shim  <->  your program (on a terminal of its own)
```

* `OSC 7501 ; ?` is **answered by the shim**, so the program starts to report.
* `OSC 7501 ; ...`, `OSC 9 ; 4 ; ...`, `OSC 9 ; 9 ; ...` and `OSC 7 ; ...` are **wrapped**
  for tmux (or screen), so that they are forwarded.
* Everything else is copied as it is, and the keys you type are never looked at.

Outside tmux and screen (no `$TMUX`, no `$STY`) the shim just starts the program.

## Use

```sh
tmux-shim -- claude --continue          # in a tmux pane
```

With the tmux settings that the program needs, set by the shim (they are the commands of
a tmux config file):

```sh
tmux-shim --config-string='set -g mouse on; setw -g mode-keys vi; set -g set-clipboard on; set -g allow-passthrough all' -- claude
tmux-shim -c ~/.config/tmux-shim.conf -- claude        # or the same, from a file
```

```
tmux-shim [--wrap auto|tmux|screen|none] [--no-reply] [-c FILE] [--config-string COMMANDS] [--no-tmux-setup] [--] COMMAND [ARGS...]
```

| Option | |
|---|---|
| `--wrap MODE` | `auto` (default): tmux if `$TMUX` is set, screen if `$STY` is set, otherwise the program is started directly. `none` still runs the shim, without wrapping. |
| `--no-reply` | Pass `OSC 7501 ; ?` on instead of answering it. |
| `-c`, `--config FILE` | Inside tmux, apply the tmux commands of the file before the program starts: the settings of tmux that the program needs (mouse, vi keys, clipboard, ...). One command per line; blank lines and lines that start with `#` are skipped. The file must exist. |
| `--config-string COMMANDS` | The same, for the commands in the argument, separated by `;`: `--config-string 'set -g mouse on; setw -g mode-keys vi; set -g set-clipboard on; set -g allow-passthrough all'`. |
| `--no-tmux-setup` | Do not run `tmux set-option -p allow-passthrough on` (see below). |

Each command is given to tmux on its own, in the order of the command line (`-c` and
`--config-string` can be repeated). A command that tmux does not accept does not stop the
others: the shim prints `tmux-shim: FILE:LINE: what tmux said` to stderr and starts the
program anyway. The commands change the whole tmux server, not just the pane, and run at
every start of the shim.

tmux forwards the wrapped sequences only when `allow-passthrough` is on. In tmux 3.3 or
later the shim turns it on for its own pane when it is `off`, before it starts the program.
No other pane and no config is changed, failures are ignored, and a value that is already
`on` or `all` is left alone. To turn it on for every pane, or to also reach the outer
terminal from hidden panes (`all`, tmux 3.4), put `set -g allow-passthrough all` in
`~/.tmux.conf`, or in `-c` / `--config-string`.

The exit code of the program is the exit code of the shim (128 + the signal when it was
killed). The window size follows the pane, and `SIGHUP` / `SIGTERM` go to the program.

## Install

Each release has a static Linux binary, `tmux-shim-x86_64-linux`, with a
`tmux-shim-x86_64-linux.sha256` file (there is no aarch64 build). It needs no library. The
shim sets `allow-passthrough` for its pane only in tmux 3.3 or later (the option is per pane
from that version); with an older tmux, turn it on yourself for the reports to be forwarded.
Screen needs nothing.

```sh
f=tmux-shim-x86_64-linux
curl -fLO https://github.com/eranif/tmux-shim/releases/latest/download/$f
curl -fLO https://github.com/eranif/tmux-shim/releases/latest/download/$f.sha256
sha256sum -c $f.sha256 && install -m 755 $f ~/.local/bin/tmux-shim
```

## Build

```sh
cargo build --release
cargo test
```

Unix only (Linux, macOS), Rust 1.80 or later. The tests that start tmux need `tmux` on the
`PATH` and skip themselves when it is missing (`TMUX_SHIM_REQUIRE_TMUX=1` makes that an
error, as in CI). The tests that run programs on a pty use GNU `dd`, `od` and `stty`, so
they run on Linux only. For a static Linux binary:
`rustup target add x86_64-unknown-linux-musl && cargo build --release --target x86_64-unknown-linux-musl`.

## How it works

One thread and `poll()` on two descriptors: the pane's terminal and the master of a pty
that the program runs on (`openpty`, the program is the leader of its own session, so its
job control and window size work). Both sides are non-blocking, with a bounded buffer
each: when tmux is slow, the shim stops reading from the program. The output goes through
a small filter that copies everything except the sequences above (a `memchr` for ESC, so
plain text is copied in one piece). It holds back at most 4 KB, never needs to know where
a read ended (a sequence can be cut anywhere), and never wraps what the program wrapped
itself.

Limits: the kernel throws away what nobody has read yet when the program that leads a
terminal exits, so output written at full speed in the last moment can be cut (it is the
same in every terminal).

## License

MIT
