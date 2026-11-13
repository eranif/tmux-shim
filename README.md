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

```
tmux-shim [--wrap auto|tmux|screen|none] [--no-reply] [--no-tmux-setup] [--] COMMAND [ARGS...]
```

| Option | |
|---|---|
| `--wrap MODE` | `auto` (default): tmux if `$TMUX` is set, screen if `$STY` is set, otherwise the program is started directly. `none` still runs the shim, without wrapping. |
| `--no-reply` | Pass `OSC 7501 ; ?` on instead of answering it. |
| `--no-tmux-setup` | Do not run `tmux set-option -p allow-passthrough on` (see below). |

tmux forwards the wrapped sequences only when `allow-passthrough` is on. In tmux 3.3 or
later the shim turns it on for its own pane, before it starts the program (no other pane and
no config is changed; failures are ignored). To turn it on for every pane, or to also reach
the outer terminal from hidden panes (`all`, tmux 3.4), put
`set -g allow-passthrough on` (or `all`) in `~/.tmux.conf`.

The exit code of the program is the exit code of the shim (128 + the signal when it was
killed). The window size follows the pane, and `SIGHUP` / `SIGTERM` go to the program.

## Build

```sh
cargo build --release
cargo test
```

Unix only (Linux, macOS). For a static Linux binary:
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
