# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

lmatrix is a Matrix-rain style log viewer for Linux terminals (Rust, edition 2024). `specs.md` is the design spec and `README.md` is the user-facing reference. Check both before you change any behavior.

## Commands

```sh
cargo build --release
cargo run -- --demo                       # fake log generator, shows every feature
cargo run -- --config lmatrix.toml --check   # validate a config and exit 0/1
cargo test
cargo test <name_substring>               # single test, e.g. cargo test default_config_text_matches_defaults
cargo test --lib sim::                    # tests of one module
cargo clippy --all-targets -- -D warnings
cargo clippy --target x86_64-pc-windows-gnu --all-targets -- -D warnings   # Windows code (cfg(windows)) compiles
./tests/e2e_tmux.sh                       # end-to-end in detached tmux (~1 min, needs tmux)
```

Formatting uses `rustfmt.toml` (`hard_tabs = true`). Run `cargo fmt` after editing.

## Architecture

A single crate with a library (`src/lib.rs`, for tests) and a binary (`src/main.rs`). Data flows like this:

1. **Sources** (`source/`): each configured source (`journald`, `file`, `command`, or `demo` with `--demo`) runs on its own thread. It sends `SourceMsg::{Preload, PreloadDone, Live}` over a bounded `mpsc::sync_channel`. There is no async runtime. Raw text is sanitized in `line::sanitize` (untrusted input: escape sequences and control chars are stripped, the line is truncated) before it becomes a `LogLine`.
   - Child processes are spawned through `GroupChild` in their own process group and registered globally, so `source::terminate_children()` can kill them on every exit path.
   - A failing source never stops the app. It reports its errors as ordinary `lmatrix: ...` lines through `SourceContext::report_error`.
2. **Main loop** (`ui/mod.rs::run`): all state lives on the main thread, with no locks. Each frame it drains the channel into `Sim::push` within a time and line budget, calls `Sim::tick(dt)` unless frozen, composes a `Frame` for the current mode (`rain_view` or `tail_view`), renders the diff, then waits for input until the next frame deadline. At startup it waits up to 2 s for every source to send `PreloadDone`.
3. **Simulation** (`sim/`): owns the backlog ring buffer, rule matching (`rules::RuleSet`, first match wins, one `RegexSet` pass), rain drops and replay (`rain.rs`), and caught lines that decode, hold, dissolve and wait for ack (`caught.rs`). **`sim` never touches the terminal.** Time only advances through `tick`, and it takes a seedable `fastrand::Rng`, so tests are deterministic.
4. **Terminal** (`term/`): **knows nothing about logs.** `Frame` is a cell buffer and `Renderer` writes diffs to any `io::Write`, so tests render into byte buffers. `TerminalGuard` sets up and restores the alternate screen, raw mode, cursor and autowrap. Restore must also work on errors, panics, SIGTERM and SIGHUP, and on terminal hang-up. `color.rs` does quantizing for truecolor, 256 and 16 colors.

Platform code is behind `cfg(unix)` / `cfg(windows)`. It covers child processes (`source/process.rs`: process groups or job objects), the shell used by `command` sources, signals (`main.rs`), hang-up detection (`term/input.rs`), file identity for rotation (`source/file.rs`), and the config lookup path. Run the Windows clippy line above after touching any of these.

Remote journald (`host` on a journald source) runs journalctl through ssh with every argument shell-quoted (`journald::command_line`). Its backlog comes from a separate run without `-f`, not from the timestamp heuristic `PreloadGate`, which is only for local journald.

Keep these boundaries: no `ratatui` and no async runtime (specs.md 10.3).

## Config changes

`config.rs` uses serde structs with `deny_unknown_fields`, followed by `validate()`, which turns range, color, duration and regex errors into messages naming the key. `Config::sim_settings()` maps the config to `SimSettings`.

`config::DEFAULT_CONFIG` is `include_str!("../lmatrix.example.toml")`. The test `default_config_text_matches_defaults` requires the example file to agree with `Config::default()`. Adding or changing a setting therefore usually touches all of these:
- `src/config.rs`: the field, its default, validation and tests
- `lmatrix.example.toml`
- `SimSettings` and its `Default` in `src/sim/mod.rs`, if the setting affects the simulation
- the README tables (and the "Tuning the rain" section, for rain keys)

`lmatrix.toml` and `test_conf.toml` in the repo root are untracked local test configs.

## E2E test

`tests/e2e_tmux.sh` builds the binary and `examples/panic_restore.rs`, runs them in an 80×24 tmux session, and checks the screen with `tmux capture-pane -p`. It covers the help bar on row 1, the backlog window on the bottom rows, tail mode (row 1 starting with ` TAIL`), terminal restore after q, Ctrl-C, signals and a panic, child cleanup, escape stripping, resizing, and terminal loss. If you change screen layout or help-bar text, update the script's assertions.
