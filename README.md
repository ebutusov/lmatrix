# lmatrix

A Matrix-rain style log viewer for Linux terminals, and for Windows Terminal
when it reads a remote host's journal over ssh. The rain is made of real
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

### Tuning the rain

Each drop writes one character per row as it falls. Every written character
starts bright and loses half its brightness every `fade_half_life`. Below
`min_brightness` it stops getting darker, and it disappears once it would
have become too faint to see. So the length of a trail is roughly `speed` ×
the time a character takes to fade out. With the defaults, a live trail is
about 30 rows and a replayed one about 24. A log line that is shorter than
the screen repeats from its start, so the text never limits the trail.

| To get | Change |
|---|---|
| Longer or shorter trails | `fade_half_life`, for example `"800ms"` or `"1s"` |
| Faster or slower rain | `speed`; faster drops also leave longer trails |
| Brighter rain on a quiet host | `replay_brightness`, since most drops are replays there |
| A smooth fade into black, as in the original design | `min_brightness = 0` |
| More or fewer drops | `fill` |
| Rain without `name[pid]:` headers, or with them | `strip_pattern`; `""` keeps the full line |
| Replays of recent or of older activity | `replay_window`: smaller replays only the latest lines, larger reaches further back |

The color mode matters too. In 256-color mode the darkest shade of each hue
is still fairly bright, so trails end with a step instead of a smooth fade.
Set `color = "truecolor"` if your terminal supports it.

## Configuration reference

lmatrix uses the first config file it finds:

1. `--config <path>`
2. `$XDG_CONFIG_HOME/lmatrix/config.toml`, or `~/.config/lmatrix/config.toml`
   (on Windows: `%APPDATA%\lmatrix\config.toml`)
3. `/etc/lmatrix/config.toml` (not on Windows)
4. If none exists, built-in defaults: one `journald` source and no rules.
   Windows has no local journal, so there this is an error that tells you to
   write a config.

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
| `replay_window` | `100` | Replays pick a random line from the newest N backlog lines, 1..10000000. Set it to `tail.backlog` to replay from the whole backlog |
| `strip_pattern` | `'^[^:]*:\s*'` | Regex whose first match is removed from the text shown in the rain; the default removes everything up to the first `:` (such as `sshd[123]: `). `""` turns it off. The backlog, tail mode, decoded lines and rules always use the full line. If nothing would be left, the full line rains |
| `min_brightness` | `0.3` | Lowest brightness of a visible trail cell, 0..1.0. Trails stay readable to their end and then vanish, instead of fading into black. `0` restores the plain fade |

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
type = "journald"
host = "admin@web1"        # runs journalctl on this host through ssh
ssh_args = ["-C"]          # extra ssh arguments, e.g. ["-C", "-p", "2222"]
label = "web1"

[[source]]
type = "file"
path = "/var/log/myapp/*.log"   # a glob, re-evaluated every 5 seconds

[[source]]
type = "command"
run = "docker logs -f web"      # run through sh -c (PowerShell on Windows)
restart = true                  # start it again when it exits
restart_delay = "5s"            # at least 100ms
```

- **journald.** Lines read `IDENTIFIER[PID]: MESSAGE`. Kernel messages arrive
  this way too. Reading the whole journal usually needs membership in the
  `systemd-journal` group, or root. Without it you only see your own user's
  messages, and journalctl's warning shows up as a `lmatrix:` line in the
  backlog. lmatrix asks journalctl only for the fields it shows, which needs
  systemd 236 or later. When journalctl exits, lmatrix starts it again after
  5 s and continues right after the last entry it received.
- **journald with `host`.** lmatrix runs
  `ssh -T -o BatchMode=yes -o ServerAliveInterval=15 <ssh_args> -- <host> journalctl …`
  and only log text crosses the network, instead of full-screen redraws
  when you run lmatrix on the server over ssh. `"-C"` in `ssh_args`
  compresses it further. `host` can be anything ssh accepts, including a
  `Host` alias from `~/.ssh/config`.
  - ssh cannot ask for a password or confirm a new host key here: use key
    authentication (an agent works too) and connect once by hand first.
  - A dropped connection is retried every 5 s and resumes where it stopped,
    without gaps or repeated lines.
  - ssh's error messages show up as `lmatrix: journalctl on <host>: …`
    lines.
  - The same group or root rules for reading the journal apply to the
    remote user.
- **file.** At startup, the last `preload` lines of every matching file are
  loaded. lmatrix then follows each file from its end, polling every 250 ms.
  When a file is rotated or truncated, it is reopened from the start.
- **command.** Has no preload.

Child processes (`journalctl`, `ssh`, commands, and anything they start) run
in their own process group (a job object on Windows) and are stopped when
lmatrix exits.

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
3. Otherwise, it uses 16 colors.

The rain looks a little different in each mode:

- **24-bit color.** Brightness is shown exactly.
- **256 colors.** Each cell gets the nearest palette color. Faint rain keeps
  its hue: it never goes darker than the darkest palette shade of its color
  (dark green for the default color), so trails don't turn gray.
- **16 colors.** The head is bright white. Other cells use the bright variant
  of the nearest basic color above half brightness, and the normal variant
  below.

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
and on SIGTERM and SIGHUP. When the terminal itself goes away (a dropped SSH
session, a killed tmux pane or window), lmatrix notices and exits. Each frame
is written in one go, wrapped in synchronized-output sequences. Terminals that
don't know these sequences ignore them.

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
- **Windows.** Build it with `cargo build --release` on Windows, then run
  `lmatrix.exe` in Windows Terminal (PowerShell or cmd). Windows Terminal
  sets `COLORTERM=truecolor`, so colors are detected.
  - There is no local journal: every `journald` source needs `host`.
    `ssh.exe` comes from the OpenSSH client that ships with Windows 10 and
    later. For key passphrases, use the `ssh-agent` service.
  - `command` sources run through PowerShell
    (`powershell -NoProfile -NonInteractive -Command`).
  - `file` sources work. Rotation is detected by file identity, as on Linux.
  - Children run in a job object, so `ssh.exe` also ends when lmatrix is
    killed or its tab is closed.
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

To check that the Windows code still compiles from Linux (no linker
needed):

```sh
rustup target add x86_64-pc-windows-gnu
cargo clippy --target x86_64-pc-windows-gnu --all-targets -- -D warnings
```

The `sim` module never touches the terminal, and its random generator is
seedable, so it is tested deterministically. The `term` module renders frames
to any `std::io::Write`.

`tests/e2e_tmux.sh` runs the binary in a detached tmux session and inspects
the screen with `tmux capture-pane -p`. It needs tmux and takes about a
minute:

```sh
./tests/e2e_tmux.sh
```

It checks:

- **Screen:** the help bar, the backlog window and the rain.
- **Modes and keys:** switching to tail mode and back, and an `ack` rule with
  the `a` key.
- **Restore:** the terminal is usable again after `q`, Ctrl-C, SIGTERM, SIGHUP
  and a panic. The panic case uses `examples/panic_restore.rs`.
- **Cleanup:** child processes are stopped.
- **Untrusted input:** escape sequences in log lines are stripped.
- **Size:** the too-small message, and resizing.
- **Lost terminal:** the process exits when its terminal is closed.
- **Remote journald:** a fake `ssh` runs the remote command locally. Backlog
  lines are not decoded, live lines are, and journalctl is stopped on quit.
  It needs a readable journal and `logger`; otherwise it is skipped.

### Code map

lmatrix is a single crate: a library (used by the tests) and a binary. Each
source runs on its own thread and sends lines over a channel. The main thread
holds all state. Each frame it feeds the lines it received to the simulation,
advances the simulation, draws the frame for the current mode and writes only
the cells that changed.

| Path | Role |
|---|---|
| `src/main.rs` | Command-line flags, config loading, `--check`, starting the sources and signal handlers, entering the terminal and running the UI |
| `src/lib.rs` | Library root; declares the modules |
| `src/config.rs` | Config file lookup, serde structs, defaults, validation, conversion to simulation settings |
| `src/line.rs` | `LogLine`, `SourceMsg` (preload / preload done / live), sanitizing untrusted text, truncation |
| `src/rules.rs` | Highlight rules and `RuleSet`: one regex-set pass per line, first match wins |
| `src/backlog.rs` | Bounded ring buffer of received lines with their matched rule |
| `src/source/mod.rs` | Starts a thread per source; `SourceContext` for sending lines and errors; line splitting |
| `src/source/process.rs` | Child processes: process groups (Unix) or job objects (Windows), stopping them all on exit |
| `src/source/journald.rs` | `journald` source: runs journalctl locally or through ssh, reads the backlog, resumes by cursor, formats entries |
| `src/source/file.rs` | `file` source: follows every file matching a glob, handles rotation and truncation |
| `src/source/command.rs` | `command` source: runs a shell command (sh or PowerShell), reads stdout and stderr, restarts it |
| `src/source/demo.rs` | Fake line generator and demo rules for `--demo` |
| `src/sim/mod.rs` | `Sim`: push, tick, resize, ack; ties the backlog, rules, rain and caught lines together; replay and `strip_pattern`. Never touches the terminal |
| `src/sim/rain.rs` | Falling drops, fading cells, fill factor |
| `src/sim/caught.rs` | Caught lines: decode, hold, dissolve, eviction, pending queue |
| `src/ui/mod.rs` | Main loop, layout, modes, key handling, palette, backlog window |
| `src/ui/rain_view.rs` | Draws rain mode: help bar, rain, caught lines, backlog window |
| `src/ui/tail_view.rs` | Tail mode: scrollable backlog, following, regex filter |
| `src/term/mod.rs` | Terminal guard (setup and restore, panic hook), color mode detection. Knows nothing about logs |
| `src/term/frame.rs` | Cell buffer and diff renderer that writes to any `io::Write` |
| `src/term/color.rs` | RGB colors and quantizing to truecolor, 256 or 16 colors |
| `src/term/input.rs` | Input reader thread and detection of a closed terminal |
| `examples/panic_restore.rs` | Panics in full-screen mode to check terminal restore (used by the e2e script) |
| `tests/e2e_tmux.sh` | End-to-end checks in a detached tmux session |
| `lmatrix.example.toml` | Commented default config, embedded in the binary for `--print-default-config` |
| `specs.md` | Design specification |

## License

MIT. See [LICENSE](LICENSE).
