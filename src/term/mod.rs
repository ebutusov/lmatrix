//! Terminal setup and teardown, capability detection, rendering.

pub mod color;
pub mod frame;
pub mod input;

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::{cursor, execute, terminal};

pub use color::{ColorMode, Rgb, TermColor};
pub use frame::{Cell, Frame, Renderer};

/// Detects the color mode from `COLORTERM` and `TERM`.
pub fn detect_color_mode(colorterm: Option<&str>, term: Option<&str>) -> ColorMode {
	match colorterm.map(str::to_ascii_lowercase).as_deref() {
		Some("truecolor") | Some("24bit") => return ColorMode::TrueColor,
		_ => {}
	}
	if term.is_some_and(|value| value.contains("256color")) {
		ColorMode::Palette256
	} else {
		ColorMode::Basic16
	}
}

/// Detects the color mode from the process environment.
pub fn detect_color_mode_from_env() -> ColorMode {
	let colorterm = std::env::var("COLORTERM").ok();
	let term = std::env::var("TERM").ok();
	detect_color_mode(colorterm.as_deref(), term.as_deref())
}

static TERMINAL_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Puts the terminal into the full-screen state and restores it on drop.
pub struct TerminalGuard {
	_private: (),
}

impl TerminalGuard {
	/// Enters raw mode, the alternate screen, hides the cursor and disables
	/// auto-wrap. Installs a panic hook that restores the terminal first.
	pub fn enter() -> io::Result<Self> {
		install_panic_hook();
		terminal::enable_raw_mode()?;
		TERMINAL_ACTIVE.store(true, Ordering::SeqCst);
		let mut stdout = io::stdout();
		if let Err(error) = execute!(
			stdout,
			terminal::EnterAlternateScreen,
			cursor::Hide,
			terminal::DisableLineWrap,
			terminal::Clear(terminal::ClearType::All)
		) {
			restore_terminal();
			return Err(error);
		}
		Ok(Self { _private: () })
	}
}

impl Drop for TerminalGuard {
	fn drop(&mut self) {
		restore_terminal();
	}
}

/// Restores the terminal. Safe to call several times and from a panic hook;
/// errors (for example a closed SSH session) are ignored.
pub fn restore_terminal() {
	if !TERMINAL_ACTIVE.swap(false, Ordering::SeqCst) {
		return;
	}
	let mut stdout = io::stdout();
	let _ = stdout.write_all(b"\x1b[?2026l\x1b[0m");
	let _ = execute!(
		stdout,
		terminal::EnableLineWrap,
		cursor::Show,
		terminal::LeaveAlternateScreen
	);
	let _ = stdout.flush();
	let _ = terminal::disable_raw_mode();
}

fn install_panic_hook() {
	static INSTALLED: AtomicBool = AtomicBool::new(false);
	if INSTALLED.swap(true, Ordering::SeqCst) {
		return;
	}
	let previous_hook = std::panic::take_hook();
	std::panic::set_hook(Box::new(move |info| {
		// A panic in another thread (for example a source) only ends that
		// thread; the UI keeps running, so the terminal must stay as it is
		// and nothing may be printed over the screen.
		let on_main_thread = std::thread::current().name() == Some("main");
		if on_main_thread || !TERMINAL_ACTIVE.load(Ordering::SeqCst) {
			restore_terminal();
			previous_hook(info);
		}
	}));
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn color_detection() {
		assert_eq!(
			detect_color_mode(Some("truecolor"), Some("xterm")),
			ColorMode::TrueColor
		);
		assert_eq!(detect_color_mode(Some("24bit"), None), ColorMode::TrueColor);
		assert_eq!(
			detect_color_mode(None, Some("tmux-256color")),
			ColorMode::Palette256
		);
		assert_eq!(
			detect_color_mode(Some(""), Some("xterm-256color")),
			ColorMode::Palette256
		);
		assert_eq!(detect_color_mode(None, Some("xterm")), ColorMode::Basic16);
		assert_eq!(detect_color_mode(None, None), ColorMode::Basic16);
	}
}
