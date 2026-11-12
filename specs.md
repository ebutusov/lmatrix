# lmatrix — specification

A Matrix-rain style log viewer for Linux terminals, written in Rust. The rain is made of real log text. Lines that match user-defined regex rules "decode" out of the rain into readable text and stay on screen for a configured time or until acknowledged. A tail mode shows the complete backlog as a plain log view.

This document is the complete implementation brief. Where it gives a number, that number comes from an approved interactive prototype and is the required default. Sections marked **(assumption)** were not explicitly approved by the project owner; implement them as written, and list them in the final report so they can be reviewed.

---

## 1. Goals and non-goals

### Goals

- Single binary, `lmatrix`, for a single Linux host, or for one or more remote hosts whose journals are read over ssh (see 6.1).
- Also builds for Windows, to run in Windows Terminal against remote journals (see 6.6).
- Runs in terminal emulators, typically over SSH: Windows Terminal, PuTTY, and inside tmux must all work.
- Sources, rules and visual parameters come from a TOML config file.
- Looks like classic Matrix rain at all times, on a busy host and on an idle one.
- Never loses a line from the backlog (up to its configured size), even though not every line is shown in the rain.

### Non-goals

- Any network listener, or remote collection other than running `journalctl` over ssh.
- CPU, memory, network or other system metrics. The screen shows logs only.
- Mouse support.
- Linux virtual console (`TERM=linux`) specific tricks such as palette redefinition.
- macOS builds.
- Persisting state (backlog, acknowledgements) across restarts.
- Reading `/dev/kmsg` directly. Kernel messages arrive through journald.

---

## 2. Screen layout

### 2.1 Rain mode (default)

```
┌──────────────────────────────────────────────────────────────┐
│ [enter] tail  [space] freeze  [a] ack  [A] ack all  [q] quit  ack 2 │  row 0: help bar
│  s      k   n                d                                 │
│  y      e   g       19:58:02 kernel: Out of memory: killed ... │  rain area, with a
│  s      r   i                o                                 │  caught line decoded
│  t      n   n                c                                 │  in place
│  e      e   x                k                                 │
│ 19:58:01 systemd[1]: Started nginx.service                     │  backlog window
│ 19:58:02 kernel: Out of memory: killed process 4312 (java)     │  (2 rows, green bg)
└──────────────────────────────────────────────────────────────┘
```

- **Row 0, help bar.** Key hints on the left. On the right, `ack N` in amber when N > 0, where N is the number of caught lines waiting for acknowledgement (on screen plus pending, see 5.4). When frozen, the bar starts with `FROZEN`.
- **Rain area.** All rows between the help bar and the backlog window.
- **Backlog window.** The bottom `window_lines` rows (default 2) on a dark green background. Shows the newest backlog entries, one per row, newest at the bottom, formatted `HH:MM:SS text`, truncated with `…` if too long. Message text uses the matching rule's color, otherwise light green. The window content refreshes at most 5 times per second so it stays readable under load. With `window_lines = 0` the window is hidden and the rain area extends to the bottom.

### 2.2 Tail mode

Entered and left with Enter. Tail mode **replaces** the rain.

- Row 0: help bar `TAIL  [enter] rain  [up/down] scroll  [/] filter  [q] quit`, with `N lines` (backlog size, or matching lines when filtered) on the right.
- All remaining rows: backlog entries, one per row, oldest at top, newest at bottom, same line format and colors as the backlog window.
- Follows new lines by default. Scrolling up stops following; End (or `G`) jumps to the newest line and resumes following.
- The rain simulation keeps running while tail mode is shown, so switching back is seamless.

### 2.3 Colors

| Element | Color |
|---|---|
| Default rain color | `#28ff6e` (configurable) |
| Rain head (newest cell of a drop) | `#e8fff0` |
| Help bar background / text | `#0d2414` / `#4fbf78` |
| `ack N` indicator | `#ffb428` |
| Backlog window background | `#0f3319` |
| Timestamp in window / tail | `#46a064` / `#28824a` |
| Unmatched message in window / tail | `#96f0b4` / `#3cd76e` |
| Undecoded glyphs of a caught line | `#1e9646` |

The rain area and tail view use the terminal's default background; do not paint it.

---

## 3. Line pipeline

```
sources (threads) ──LogLine──▶ channel ──▶ main loop ──▶ Sim::push ──▶ backlog (always)
                                                                  ├──▶ rule match ──▶ caught line (if hold)
                                                                  └──▶ rain drop (if a slot is free, or if matched)
```

Every received line:

1. Is sanitized (section 7.4) at the source boundary.
2. Is appended to the backlog. This never fails; when the backlog is full the oldest entry is discarded.
3. Is tested against the rules in config order. The first match wins.
4. Becomes a rain drop if the fill factor allows it, or unconditionally if a rule matched (section 4.3).
5. If the matched rule has a hold, becomes a caught line (section 5).

---

## 4. Rain

### 4.1 Drops

A drop is one log line falling down one column.

- A new drop starts at the top row of the rain area in a **random column that has no active drop**.
- Its speed is `speed × uniform(1 − speed_jitter, 1 + speed_jitter)` rows per second. Defaults: `speed = 16`, `speed_jitter = 0.35`.
- Each time the drop advances one row, it writes the next character of its text into that cell.
  - A space in the text is written as a space, so words stay readable. It overwrites the cell like any other character.
  - If the text is shorter than the rain area, it wraps around and continues from its first character.
  - If the text is longer, the drop simply ends at the bottom; the rest is not shown.
- The drop ends when it leaves the bottom of the rain area. Its column is then free again.

### 4.2 Brightness and fading

- Each cell has a color and a brightness level from 0 to 1. A newly written cell has level 1.
- Levels decay exponentially with a half-life of 480 ms (`fade_half_life`). This must be time-based, not frame-based, so the look is identical at any frame rate.
- A cell with level above 0.95 is drawn in the head color (near white). Otherwise it is drawn as `color × level`. Cells below 0.06 are not drawn.

### 4.3 Fill factor

- `fill` (default 0.35) is the share of columns that may carry an active drop at once. `max_drops = max(1, round(fill × columns))`.
- A live line that arrives when `max_drops` is reached is **not rained**. It is still in the backlog and still goes through the rules.
- A line that matches a rule **always** rains, in the rule's color, even above the cap. If no column is free, use any random column.

### 4.4 Replay

When live lines do not keep the rain filled, old lines are replayed so the screen always looks alive.

- Active only when `replay = true` (default) and the backlog is not empty.
- While the number of active drops is below `floor(max_drops × (1 − replay_reserve))` (`replay_reserve` default 0.15), spawn replay drops at an average rate of 14 per second. The reserve keeps slots free for live lines on a quiet host.
- A replay drop takes a random entry from the newest `replay_window` backlog lines (all of them if the backlog is shorter). With `replay_window = "auto"` (default) the window is `max(100, 4 × floor(max_drops × (1 − replay_reserve)))`, recomputed on every pick, so it follows resizes. With a small fixed window, a wide screen with many replay slots would show the same few lines in many columns at once.
- It starts at a **random character position** in its text, wrapping around, so columns do not all begin with the same prefix.
- Replay drops are always in the default color, even if the original line matched a rule, and never trigger rules again. Color in the rain therefore always means a live event.
- Replay drops are dimmer: the head is drawn normally, and every cell behind the head is clamped to `replay_brightness` (default 0.55) as soon as the head moves on.
- Replays are not added to the backlog and never appear in tail mode or the backlog window.

---

## 5. Rules and caught lines

### 5.1 Rule definition

Each rule has a `name`, a `pattern` (Rust `regex` crate syntax, case-sensitive unless the pattern says `(?i)`), a `color`, and a `hold`:

| `hold` value | Meaning |
|---|---|
| `"0s"` or omitted | Color only: the line rains in the rule's color, nothing is decoded |
| a duration, e.g. `"30s"`, `"5m"` | Decoded line stays for that long, then dissolves |
| `"ack"` | Decoded line stays until the user acknowledges it |

Rules are evaluated in config order; the first match wins. An invalid regex is a startup error that names the rule.

### 5.2 Decode, hold, dissolve

When a line matches a rule with a hold, a caught line appears over the rain:

- **Text:** `HH:MM:SS ` followed by the line, truncated with `…` so that it fits the screen width with two cells of margin on each side. For `ack` rules the text ends with ` [a]`, and room for that marker is reserved before truncating.
- **Position:** a random free row of the rain area, excluding its first and last row, and a random column at which the whole text plus one padding cell on each side fits. One caught line per row.
- **Background:** the text extent plus one padding cell on each side is cleared, hiding the rain behind it. The rain continues underneath.
- **Decode:** characters are revealed left to right at 32 characters per second. Positions not yet revealed show a random glyph (same glyph set as in 4.1) that changes every frame, in dim green. Revealed characters use the rule's color.
- **Hold:** the line stays fully readable for the rule's hold time, measured from its appearance.
- **Dissolve:** over 1.5 seconds, characters disappear one by one in a fixed pseudo-random order (not left to right), until the line is gone.
- **Ack marker:** on lines waiting for acknowledgement, the `[a]` at the end is white and blinks, 0.5 s on and 0.5 s off.

### 5.3 Acknowledging

- `a` acknowledges the oldest waiting line, `A` acknowledges all. An acknowledged line starts its dissolve immediately.
- Acknowledgement is only possible in rain mode.

### 5.4 When the screen is full of caught lines

- If no row is free, the oldest caught line that is **not** waiting for acknowledgement is removed at once and its row reused.
- If every row holds a line waiting for acknowledgement, the new caught line goes to a pending queue (maximum 50; beyond that the oldest pending is discarded) and is placed as soon as a row frees up.

### 5.5 Preloaded lines

Lines loaded at startup (section 6.4) are stored with their matching rule so they are colored in tail mode and the backlog window, but they do not rain as live lines, do not decode, and do not ask for acknowledgement.

---

## 6. Sources

Each source runs in its own thread and sends lines over a bounded channel (capacity 8192; a full channel blocks the source). Three types:

### 6.1 `journald`

- Runs `journalctl -f -o json -n <preload>` as a child process, adding `-u <unit>` for each configured unit.
- Line text: `<SYSLOG_IDENTIFIER>[<_PID>]: <MESSAGE>`; omit the `[pid]` part when there is no PID, and fall back to `_COMM` when there is no identifier. `MESSAGE` can be a JSON array of byte values when it is not valid UTF-8; decode it lossily.
- Timestamp: `__REALTIME_TIMESTAMP` (microseconds since the epoch).
- Do not link against libsystemd.
- Only `MESSAGE`, `SYSLOG_IDENTIFIER`, `_PID` and `_COMM` are requested (`--output-fields`, systemd 236+); journalctl adds the cursor and timestamps by itself. About half the output size.
- When journalctl exits, it is started again after 5 s with `--after-cursor=<last cursor>`, so no entry is lost or repeated.
- **Remote (`host`).** With `host = "<ssh destination>"`, journalctl runs on that host through `ssh -T -o BatchMode=yes -o ServerAliveInterval=15 -o ServerAliveCountMax=2 <ssh_args> -- <host> <command>`. Each journalctl argument is single-quoted for the remote shell. Authentication must work without a prompt (keys or an agent). The backlog is read by a first run without `-f` that ends after `-n <preload>` entries, then a second run follows with `--after-cursor`. The local heuristic of 6.4 is not used, because clock skew and network latency break it. ssh's stderr is reported as `lmatrix: journalctl on <host>: …` lines.

### 6.2 `file`

- `path` is a glob. All matching files are followed. The glob is re-evaluated every 5 seconds to pick up new files.
- At startup, the last `preload` lines of each file are read as preloaded lines; then the file is followed from its end.
- Poll for new data every 250 ms. Detect rotation (a different file identity: device and inode on Unix, volume serial and file index on Windows) and truncation (size shrinks) and reopen from the start of the new file.
- Timestamp: time of reception. Do not try to parse timestamps out of the text.

### 6.3 `command`

- Runs `run` through `sh -c` (`powershell -NoProfile -NonInteractive -Command` on Windows) and reads lines from its stdout and stderr.
- If the command exits and `restart = true` (default), it is started again after `restart_delay` (default 5 s).
- No preload.

### 6.4 Preload and ordering **(assumption)**

- Sources flag their preloaded lines and signal when preload is complete.
- At startup the app waits until every source has finished preloading, or 2 seconds at most, sorts the preloaded lines by timestamp, and inserts them into the backlog before starting the rain.

### 6.5 Labels and failures **(assumption)**

- A source may have an optional `label`; when set, each of its lines is prefixed with `(label) `.
- Source errors (child process fails to start, `journalctl` reports a permission problem, a file disappears) are reported as ordinary lines with the text `lmatrix: <description>`, so they show up in the backlog and can be matched by rules. A failing source never terminates the app.
- All child processes are terminated when the app exits.

### 6.6 Windows

- No local journal: a journald source must set `host`. Otherwise, including when no config file exists, startup fails with an error that says so (`--demo` excepted).
- Config file: `%APPDATA%\lmatrix\config.toml`.
- Each child runs in a job object with kill-on-close, and without a console of its own. Children are stopped on exit, and also when lmatrix is killed or its window is closed.
- SIGHUP and terminal hang-up detection do not exist there; closing the console window ends the process.

---

## 7. Terminal handling

### 7.1 Setup and teardown

- Use the alternate screen, raw mode, a hidden cursor, and disabled auto-wrap. Restore all four on exit.
- Restoration must happen on normal exit, on error, on panic (panic hook), and on SIGTERM and SIGHUP (a dropped SSH session). The terminal or tmux pane must never be left in raw mode.
- `q` and Ctrl-C quit.

### 7.2 Color modes

`display.color` selects the mode; `auto` (default) detects it:

1. `COLORTERM` is `truecolor` or `24bit` → 24-bit color.
2. Otherwise `TERM` contains `256color` → 256 colors.
3. Otherwise → 16 colors.

`COLORTERM` is often not forwarded over SSH, so the config override matters; document it in the README.

- **256 colors:** quantize the final cell color (after applying brightness) to the nearest entry of the 6×6×6 cube or the grayscale ramp.
- **16 colors:** map brightness to three steps of the nearest basic color: bright white for the head, the bright variant above level 0.5, the normal variant below.

### 7.3 Rendering

- Keep a buffer of the previous frame and write only cells that changed. Compare **after** color quantization, so that brightness changes that land on the same palette entry cause no output.
- Build each frame's output in memory and write it with a single flush, wrapped in the synchronized-output sequences (`CSI ? 2026 h` / `l`).
- Default 20 frames per second (`display.fps`, allowed range 5 to 60).
- On resize: rebuild the buffers, redraw fully, drop any active drop whose column no longer exists, and re-place caught lines that no longer fit.
- If the terminal is smaller than 20 columns by 6 rows, show only the message `terminal too small` until it grows.

### 7.4 Sanitizing input

Log text is untrusted and must never reach the terminal raw.

- Remove ANSI escape sequences and all control characters; convert tabs to a single space.
- Replace every character that is not exactly one column wide (zero-width, double-width) with `?`.
- With `display.ascii_only = true`, also replace every non-ASCII character with `?`, and use `...` in place of `…` for truncation.
- Truncate lines longer than 4096 characters.

---

## 8. Keys

| Key | Rain mode | Tail mode |
|---|---|---|
| Enter | Switch to tail mode | Switch to rain mode |
| Space | Freeze / unfreeze | — |
| `a` | Acknowledge oldest waiting line | — |
| `A` | Acknowledge all waiting lines | — |
| Up / Down, PgUp / PgDn, Home | — | Scroll |
| End or `G` | — | Jump to newest, resume following |
| `/` | — | Enter a filter regex; Enter applies, Esc cancels |
| Esc | — | Clear the active filter |
| `q`, Ctrl-C | Quit | Quit |

**Freeze (assumption):** while frozen the animation stops completely. Incoming lines are still stored in the backlog and matched against rules; their drops and caught lines appear when the animation resumes.

**Filter (assumption):** the filter applies to tail mode only. It is a regex typed on the help bar row; an invalid regex shows an error there and is not applied. While a filter is active the help bar shows it.

---

## 9. Configuration

### 9.1 Location

1. `--config <path>` if given.
2. `$XDG_CONFIG_HOME/lmatrix/config.toml`, or `~/.config/lmatrix/config.toml`.
3. `/etc/lmatrix/config.toml`.
4. If none exists: built-in defaults with a single `journald` source and no rules.

### 9.2 Full schema with defaults

```toml
[display]
fps = 20                  # 5..60
color = "auto"            # auto | truecolor | 256 | 16
default_color = "#28ff6e"
ascii_only = false
window_lines = 2          # backlog window rows at the bottom, 0 hides it

[rain]
speed = 16                # rows per second
speed_jitter = 0.35       # 0..0.9, random variation per drop
fill = 0.35               # 0.01..1.0, share of columns with an active drop
fade_half_life = "480ms"
replay = true
replay_brightness = 0.55  # 0.1..1.0
replay_reserve = 0.15     # share of slots kept free for live lines
replay_window = "auto"    # newest N backlog lines replays pick from, or "auto"

[tail]
backlog = 10000           # lines kept in memory
preload = 200             # lines read at startup per journald/file source

[[source]]
type = "journald"
units = []                # empty = everything
# label = "sys"

[[source]]
type = "file"
path = "/var/log/myapp/*.log"

[[source]]
type = "command"
run = "docker logs -f web"
restart = true
restart_delay = "5s"

[[rule]]
name = "oom"
pattern = "(?i)out of memory|oom-killer"
color = "#ff463c"
hold = "ack"

[[rule]]
name = "auth-fail"
pattern = "Failed password for \\S+"
color = "#ffb428"
hold = "30s"

[[rule]]
name = "login"
pattern = "Accepted publickey"
color = "#3cc8ff"
# no hold: color only
```

### 9.3 Validation

Unknown keys, out-of-range values, invalid colors, invalid durations and invalid regexes are startup errors with a message that names the offending key or rule. Nothing is written to the terminal in raw mode before validation has passed.

### 9.4 Command line

| Flag | Effect |
|---|---|
| `--config <path>` | Use this config file |
| `--check` | Validate the config, print the result, exit with 0 or 1 |
| `--print-default-config` | Print the commented default config to stdout |
| `--demo` | Ignore configured sources and use a built-in fake line generator (see 12) |
| `--version`, `--help` | As usual |

---

## 10. Architecture

Single crate with a library target (for tests) and a binary.

```
lmatrix/
├── Cargo.toml
├── README.md
├── specs.md                 # this file
├── lmatrix.example.toml
└── src/
    ├── main.rs          # args, load config, start sources, run the app
    ├── lib.rs
    ├── config.rs        # serde structs, validation
    ├── line.rs          # LogLine, SourceId, sanitizing
    ├── rules.rs         # Rule, Hold, RuleSet (first match wins)
    ├── backlog.rs       # ring buffer
    ├── source/
    │   ├── mod.rs       # spawn(source_cfg, Sender<SourceMsg>)
    │   ├── journald.rs
    │   ├── file.rs
    │   ├── command.rs
    │   ├── process.rs   # child processes: process groups / job objects
    │   └── demo.rs      # fake generator for --demo
    ├── sim/
    │   ├── mod.rs       # Sim: push, tick, resize, ack
    │   ├── rain.rs      # drops, fill factor, replay
    │   └── caught.rs    # decode, hold, dissolve, pending queue
    ├── ui/
    │   ├── mod.rs       # main loop, Mode, key handling
    │   ├── rain_view.rs # help bar + rain + caught lines + backlog window
    │   └── tail_view.rs # scrollable backlog, filter
    └── term/
        ├── mod.rs       # terminal guard, capability detection
        ├── frame.rs     # cell buffer and diff
        └── color.rs     # Rgb, quantizing
```

### 10.1 Core types (indicative)

```rust
pub struct LogLine { pub ts: DateTime<Local>, pub source: SourceId, pub text: Box<str> }
pub enum SourceMsg { Preload(LogLine), PreloadDone(SourceId), Live(LogLine) }

pub enum Hold { None, For(Duration), UntilAck }
pub struct Rule { pub name: String, pub regex: Regex, pub color: Rgb, pub hold: Hold }

pub struct Entry { pub line: LogLine, pub rule: Option<RuleId> }
pub struct Backlog { /* VecDeque<Entry>, capacity */ }

pub struct Sim { /* grid, drops, caught, pending, backlog, rules, rng, settings */ }
impl Sim {
    pub fn preload(&mut self, lines: Vec<LogLine>);
    pub fn push(&mut self, line: LogLine);
    pub fn tick(&mut self, dt: Duration);
    pub fn resize(&mut self, cols: u16, rows: u16);
    pub fn ack_oldest(&mut self);
    pub fn ack_all(&mut self);
}
```

### 10.2 Rules for the structure

- **Threads:** one per source, plus the main thread. No async runtime.
- **Main loop:** per frame, drain the channel into `Sim::push`, handle input, call `Sim::tick(dt)` unless frozen, compose the frame for the current mode, write the diff. All state lives on the main thread; no locks.
- **`sim` never touches the terminal** and takes its random generator as a seedable value, so it is deterministic under test.
- **`term` knows nothing about logs.** It renders a frame of cells to any `std::io::Write`, so tests can render into a byte buffer.
- Use a `RegexSet` (or equivalent) so matching many rules costs one pass per line.

### 10.3 Dependencies

Use the latest stable release of each at implementation time: `crossterm`, `regex`, `serde`, `toml`, `serde_json`, `humantime`, `chrono`, `glob`, `signal-hook`, `unicode-width`, `fastrand`, `anyhow`, `clap`. Do not add `ratatui` or an async runtime. Additional small crates are acceptable if justified in the final report.

---

## 11. Milestones

Commit after each milestone. Each must build without warnings, pass `cargo clippy -- -D warnings`, and pass `cargo test`.

1. **Rain core.** `term`, `sim`, and `--demo`. The rain, fill factor, fading, replay and the help bar work with generated lines. Clean terminal restore in all exit paths.
2. **Config and rules.** Config loading, validation, `--check`, `--print-default-config`. Colored drops, caught lines with decode, hold, dissolve, ack and the pending queue.
3. **Sources.** `journald`, `file`, `command`, preload, labels, failure reporting.
4. **Backlog views.** Backlog window, tail mode with scrolling and filter, freeze.
5. **Compatibility.** 256 and 16 color modes, resize, too-small screen, `ascii_only`. README with configuration reference and notes for SSH, tmux and PuTTY.

---

## 12. Testing

### 12.1 Demo generator

`--demo` produces realistic fake log lines (systemd, sshd, kernel, nginx, cron) at about 5 lines per second, with roughly one line in 20 matching a typical error pattern. When no config file exists, `--demo` also installs three built-in rules (an `ack` rule, a 10-second rule, and a color-only rule) so every feature is visible without any setup.

### 12.2 Unit tests (required)

- **Sanitizing:** escape sequences, control characters, tabs, wide characters, over-long lines.
- **Rules:** first match wins; all three hold forms parse; invalid regex is rejected with the rule's name.
- **Config:** defaults; each validation error; unknown keys rejected.
- **Backlog:** capacity and discard order.
- **Sim, with a fixed seed:**
  - active drops never exceed `max_drops` for unmatched lines;
  - a matched line always produces a drop;
  - no two drops start in the same column while a free one exists;
  - replay tops up to the reserve limit and not beyond, and stops when live lines fill the rain;
  - replay drops use the default color and a non-zero start offset is possible;
  - a caught line with a timed hold is gone after hold + 1.5 s;
  - an `ack` line stays until acknowledged, and `ack_oldest` picks the oldest;
  - eviction and the pending queue behave as in 5.4;
  - preloaded lines produce no live drops and no caught lines.
- **Color:** quantization to 256 and 16 colors for known inputs.
- **Frame diff:** an unchanged frame produces no output; a one-cell change produces output for that cell only.

### 12.3 End-to-end check without a human

Run the binary in a detached tmux session and inspect it with `tmux capture-pane -p`:

- `lmatrix --demo` shows the help bar on the first row and the backlog window on the last two.
- Sending Enter switches to tail mode (help bar starts with `TAIL`); Enter again switches back.
- A `command` source that echoes a line matching an `ack` rule makes `ack 1` appear in the help bar; sending `a` clears it.
- After sending `q`, and separately after `kill -TERM`, the pane is back at a usable shell prompt with a visible cursor.

---

## 13. Acceptance criteria

1. With `--demo` on an 80×24 terminal the screen matches section 2.1 and the rain is never empty.
2. With an event rate far above what the fill factor allows (for example a `command` source running `yes`), the rain density stays at the fill factor, the app stays responsive, memory stays bounded, and tail mode still shows every received line up to the backlog size.
3. With no incoming lines at all, the rain continues through dimmed replays.
4. A line matching a rule with a hold decodes in place in the rule's color, holds for the configured time, and dissolves. A line matching an `ack` rule stays until `a` or `A` is pressed.
5. Tail mode shows the complete backlog including lines that were never rained, follows new lines, scrolls, and filters.
6. At startup with a `journald` or `file` source, the backlog window and tail mode are populated immediately from preloaded lines.
7. Works in 24-bit, 256-color and 16-color modes, inside tmux, and survives terminal resizes.
8. The terminal is restored correctly after quit, Ctrl-C, SIGTERM, SIGHUP and a panic.
9. A log line containing escape sequences cannot affect the terminal.
10. CPU use at 20 fps on an idle 200×50 terminal stays below 5% of one core on ordinary server hardware.

---

## 14. Notes for the implementing agent

- Check that a Rust toolchain is available (`cargo --version`). If it is not, stop and ask before installing anything.
- Reading the journal usually requires membership in the `systemd-journal` group or root. If `journalctl` reports a permission problem, surface it as described in 6.5 and continue; do not change system permissions.
- Do not install services, modify system configuration, or write outside the project directory.
- Initialize a git repository in the project directory if there is none, and commit per milestone.
- The license is to be decided by the project owner; do not add one.
- In the final report, list every **(assumption)** section, any deviation from this document, and any dependency added beyond section 10.3.
