# lmatrix

A Matrix-rain style log viewer for Linux terminals. The rain is made of real
log text. Lines that match your rules "decode" out of the rain into readable
text and stay on screen for a set time or until you acknowledge them. Tail
mode shows the complete backlog as a plain log view.

```
 [enter] tail  [space] freeze  [a] ack  [A] ack all  [q] quit           ack 1
  s      k   n                d
  y      e   g    19:58:02 kernel: Out of memory: killed process 4312 [a]
  s      r   i                o
  t      n   n                c
 19:58:01 systemd[1]: Started nginx.service
 19:58:02 kernel: Out of memory: killed process 4312 (java)
```

## Build and run

Needs a Rust toolchain (edition 2024).

```sh
cargo build --release
./target/release/lmatrix --demo        # fake log lines, every feature visible
./target/release/lmatrix               # uses your config (or the journal by default)
```

| Flag | Effect |
|---|---|
| `--config <path>` | Use this config file |
| `--check` | Validate the config, print the result, exit with 0 or 1 |
| `--print-default-config` | Print the commented default config to stdout |
| `--demo` | Ignore configured sources and use a built-in fake line generator. When no config file exists, it also installs three demo rules (an `ack` rule, a 10-second rule, and a color-only rule) |
| `--version`, `--help` | As usual |

To start your own config:

```sh
mkdir -p ~/.config/lmatrix
lmatrix --print-default-config > ~/.config/lmatrix/config.toml
lmatrix --check
```

## Screen

- **Rain mode** (default). The top row is the help bar. On its right, `ack N`
  in amber counts the caught lines waiting for acknowledgement. The bottom
  `window_lines` rows (default 2) show the newest backlog entries.
  - Every received line goes to the backlog.
  - Lines only rain while the fill factor allows it.
  - Lines matching a rule always rain, in the rule's color.
  - When the host is quiet, old lines are replayed, dimmer and in the default
    color, so color in the rain always means a live event.
- **Tail mode** (Enter). It replaces the rain with the full backlog, oldest
  at the top, and follows new lines.

### Keys

| Key | Rain mode | Tail mode |
|---|---|---|
| Enter | Switch to tail mode | Switch to rain mode |
| Space | Freeze / unfreeze the animation | — |
| `a` | Acknowledge the oldest waiting line | — |
| `A` | Acknowledge all waiting lines | — |
| Up / Down, PgUp / PgDn, Home | — | Scroll (stops following) |
| End or `G` | — | Jump to the newest line and follow again |
| `/` | — | Type a filter regex: Enter applies it, Esc cancels |
| Esc | — | Clear the active filter |
| `q`, Ctrl-C | Quit | Quit |

While frozen, incoming lines are still stored and matched against the rules.
Their drops and caught lines appear when you unfreeze. The filter applies to
tail mode only.

## Configuration reference

lmatrix uses the first config file it finds:

1. `--config <path>`
2. `$XDG_CONFIG_HOME/lmatrix/config.toml`, or `~/.config/lmatrix/config.toml`
3. `/etc/lmatrix/config.toml`
4. If none exists, built-in defaults: one `journald` source and no rules.

Unknown keys, out-of-range values, invalid colors, durations and regexes are
startup errors that name the offending key or rule. Colors are written
`"#rrggbb"`. Durations are written like `"480ms"`, `"30s"` or `"5m"`.

### `[display]`

| Key | Default | Meaning |
|---|---|---|
| `fps` | `20` | Frames per second, 5..60 |
| `color` | `"auto"` | `auto`, `truecolor`, `256` or `16`; see [Color modes](#color-modes) |
| `default_color` | `"#28ff6e"` | Rain color of lines that match no rule |
| `ascii_only` | `false` | Replace every non-ASCII character with `?`, and truncate with `...` instead of `…` |
| `window_lines` | `2` | Rows of the backlog window at the bottom, 0..20; `0` hides it |

### `[rain]`

| Key | Default | Meaning |
|---|---|---|
| `speed` | `16` | Rows per second, 0.5..200 |
| `speed_jitter` | `0.35` | Random speed variation per drop, 0..0.9 |
| `fill` | `0.35` | Share of columns that may carry an active drop, 0.01..1.0 |
| `fade_half_life` | `"480ms"` | Brightness half-life of a written cell, 10ms..60s |
| `replay` | `true` | Replay old lines when live lines do not fill the rain |
| `replay_brightness` | `0.55` | Brightness of replayed trails, 0.1..1.0 |
| `replay_reserve` | `0.15` | Share of drop slots kept free for live lines, 0..1.0 |

### `[tail]`

| Key | Default | Meaning |
|---|---|---|
| `backlog` | `10000` | Lines kept in memory, 1..10000000. When full, the oldest line is discarded |
| `preload` | `200` | Lines read at startup per `journald` or `file` source, 0..100000 |

### `[[source]]`

Each source may set `label = "name"`, which prefixes each of its lines with
`name: `. Without any `[[source]]` section, a single journald source is used.
Write `source = []` to run with no sources at all.

```toml
[[source]]
type = "journald"          # runs journalctl -f -o json -n <preload>
units = ["ssh.service"]    # adds -u <unit> for each; empty = everything

[[source]]
type = "file"
path = "/var/log/myapp/*.log"   # a glob, re-evaluated every 5 seconds

[[source]]
type = "command"
run = "docker logs -f web"      # run through sh -c; stdout and stderr are read
restart = true                  # start it again when it exits
restart_delay = "5s"            # at least 100ms
```

- **journald.** Lines read `IDENTIFIER[PID]: MESSAGE`. Kernel messages arrive
  this way too. Reading the whole journal usually needs membership in the
  `systemd-journal` group, or root. Without it you only see your own user's
  messages, and journalctl's warning shows up as a `lmatrix:` line in the
  backlog.
- **file.** At startup, the last `preload` lines of every matching file are
  loaded. lmatrix then follows each file from its end, polling every 250 ms.
  When a file is rotated or truncated, it is reopened from the start.
- **command.** Has no preload.

Source problems (a command fails to start, a permission error, a file that
disappears) show up as ordinary lines starting with `lmatrix: `, so you can
match them with a rule. A failing source never stops the app.

### `[[rule]]`

Rules are tested in config order, and the first match wins. A pattern uses the
Rust `regex` syntax and is case-sensitive unless it starts with `(?i)`.

```toml
[[rule]]
name = "oom"
pattern = "(?i)out of memory|oom-killer"
color = "#ff463c"
hold = "ack"     # keep it on screen until acknowledged with a / A

[[rule]]
name = "auth-fail"
pattern = "Failed password for \\S+"
color = "#ffb428"
hold = "30s"     # keep it decoded for 30 s, then dissolve

[[rule]]
name = "login"
pattern = "Accepted publickey"
color = "#3cc8ff"
# no hold (or "0s"): color only, nothing is decoded
```

A decoded line appears on a free row of the rain area. It decodes left to
right, stays readable for its hold time, then dissolves over 1.5 s. When every
row is taken, the oldest line that is not waiting for acknowledgement makes
room. If every row is waiting for acknowledgement, new lines queue (up to 50)
until a row frees up. Lines loaded at startup are colored in tail mode and the
backlog window, but they never decode or ask for acknowledgement.

## Color modes

With `color = "auto"`, lmatrix picks the mode from the environment:

1. If `COLORTERM` is `truecolor` or `24bit`, it uses 24-bit color.
2. Otherwise, if `TERM` contains `256color`, it uses the 256-color palette.
3. Otherwise, it uses 16 colors. Rain brightness then maps to the normal or
   bright variant of the nearest basic color.

**`COLORTERM` is usually not forwarded over SSH**, so a truecolor terminal on
the other end is detected as 256 or 16 colors. Either set the mode in the
config:

```toml
[display]
color = "truecolor"
```

or forward the variable. Add `SendEnv COLORTERM` to your local `~/.ssh/config`
and `AcceptEnv COLORTERM` to the server's `sshd_config`.

## Terminal notes

lmatrix uses the alternate screen, raw mode, a hidden cursor and disabled
auto-wrap. All four are restored on quit, on Ctrl-C, on errors, on a panic,
and on SIGTERM and SIGHUP (a dropped SSH session). Each frame is written in one
go, wrapped in synchronized-output sequences. Terminals that don't know these
sequences ignore them.

- **Windows Terminal.** Supports truecolor. It sets `COLORTERM=truecolor`
  locally, but that isn't forwarded over SSH (see above), so set
  `color = "truecolor"`.
- **PuTTY.**
  - PuTTY reports `TERM=xterm` by default, so lmatrix detects 16 colors. Set
    *Connection → Data → Terminal-type string* to `xterm-256color`. PuTTY 0.71
    and later also handle `color = "truecolor"`.
  - Set *Window → Translation → Remote character set* to UTF-8.
  - If your font lacks `…`, use `ascii_only = true`.
- **tmux.**
  - Use a 256-color terminal type, for example
    `set -g default-terminal "tmux-256color"`.
  - tmux doesn't pass `COLORTERM` through. For truecolor, enable RGB for your
    outer terminal and set `color = "truecolor"`:
    `set -as terminal-features ",xterm-256color:RGB"` (tmux 3.2+), or
    `set -ga terminal-overrides ",xterm-256color:Tc"` on older versions.
  - lmatrix restores the pane when the session or pane is killed.
- **Small terminals.** Below 20×6, lmatrix shows only `terminal too small`
  until the window grows.

Log text is untrusted, so it is sanitized before it reaches the screen.
lmatrix removes escape sequences and control characters, turns tabs into
spaces, and replaces characters that are not one column wide with `?`. Lines
are cut at 4096 characters.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
```

The `sim` module never touches the terminal, and its random generator is
seedable, so it is tested deterministically. The `term` module renders frames
to any `std::io::Write`. An end-to-end check runs the binary in a detached tmux
session and inspects it with `tmux capture-pane -p`.
