//! Main loop, modes and key handling.

pub mod rain_view;
pub mod tail_view;

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal;
use fastrand::Rng;

use crate::backlog::Entry;
use crate::line::{LogLine, SourceMsg};
use crate::sim::Sim;
use crate::term::input::{Input, InputReader, terminal_hung_up};
use crate::term::{ColorMode, Frame, Renderer, Rgb, TermColor};
use tail_view::{TailAction, TailView};

/// Longest time spent waiting for sources to finish preloading.
const PRELOAD_TIMEOUT: Duration = Duration::from_secs(2);
/// Longest time spent draining the channel per frame.
const DRAIN_BUDGET: Duration = Duration::from_millis(8);
/// Largest number of lines taken from the channel per frame.
const DRAIN_MAX_LINES: usize = 20_000;
/// Largest simulation step, so a stalled frame does not jump.
const MAX_TICK: Duration = Duration::from_millis(250);
/// Smallest usable terminal.
const MIN_COLS: u16 = 20;
const MIN_ROWS: u16 = 6;
/// Smallest rain area kept when the backlog window is large.
const MIN_RAIN_ROWS: u16 = 3;
/// The backlog window content is refreshed at most this often.
const WINDOW_REFRESH: Duration = Duration::from_millis(200);

/// Display options for the UI.
#[derive(Debug, Clone)]
pub struct UiOptions {
	pub fps: u32,
	/// Lowest brightness a visible rain cell is drawn with.
	pub min_brightness: f32,
	pub color_mode: ColorMode,
	pub window_lines: u16,
	pub ascii_only: bool,
}

/// All colors, converted for the active color mode.
#[derive(Debug, Clone)]
pub struct Palette {
	pub mode: ColorMode,
	/// Lowest brightness a visible rain cell is drawn with.
	pub min_level: f32,
	pub head_rgb: Rgb,
	pub help_bg: TermColor,
	pub help_fg: TermColor,
	pub ack: TermColor,
	pub marker: TermColor,
	pub glyph_dim: TermColor,
	pub error: TermColor,
	pub window_bg: TermColor,
	pub window_timestamp: TermColor,
	pub window_message: TermColor,
	pub tail_timestamp: TermColor,
	pub tail_message: TermColor,
}

impl Palette {
	pub fn new(mode: ColorMode) -> Self {
		Self {
			mode,
			min_level: 0.0,
			head_rgb: Rgb(0xe8, 0xff, 0xf0),
			help_bg: mode.solid(Rgb(0x0d, 0x24, 0x14)),
			help_fg: mode.solid(Rgb(0x4f, 0xbf, 0x78)),
			ack: mode.solid(Rgb(0xff, 0xb4, 0x28)),
			marker: mode.solid(Rgb::WHITE),
			glyph_dim: mode.solid(Rgb(0x1e, 0x96, 0x46)),
			error: mode.solid(Rgb(0xff, 0x46, 0x3c)),
			window_bg: mode.solid(Rgb(0x0f, 0x33, 0x19)),
			window_timestamp: mode.solid(Rgb(0x46, 0xa0, 0x64)),
			window_message: mode.solid(Rgb(0x96, 0xf0, 0xb4)),
			tail_timestamp: mode.solid(Rgb(0x28, 0x82, 0x4a)),
			tail_message: mode.solid(Rgb(0x3c, 0xd7, 0x6e)),
		}
	}

	/// Rounds a brightness level to a fixed number of steps, so a fading
	/// cell only changes color (and needs output) when the step changes.
	pub fn quantize_level(&self, level: f32) -> f32 {
		const STEPS: f32 = 32.0;
		(level * STEPS).round() / STEPS
	}

	/// Brightness a visible rain cell is drawn with: never below the
	/// minimum, so a trail stays readable to its end and then disappears
	/// instead of fading into black.
	pub fn rain_level(&self, level: f32) -> f32 {
		self.quantize_level(level.max(self.min_level))
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
	Rain,
	Tail,
}

/// Screen geometry.
#[derive(Debug, Clone, Copy)]
struct Layout {
	cols: u16,
	rows: u16,
	too_small: bool,
	rain_top: u16,
	rain_rows: u16,
	window_top: u16,
	window_rows: u16,
}

impl Layout {
	fn new(cols: u16, rows: u16, window_lines: u16) -> Self {
		let too_small = cols < MIN_COLS || rows < MIN_ROWS;
		let below_help = rows.saturating_sub(1);
		let window_rows = window_lines.min(below_help.saturating_sub(MIN_RAIN_ROWS));
		let rain_rows = below_help - window_rows;
		Self {
			cols,
			rows,
			too_small,
			rain_top: 1,
			rain_rows,
			window_top: 1 + rain_rows,
			window_rows,
		}
	}
}

struct App {
	sim: Sim,
	palette: Palette,
	renderer: Renderer,
	frame: Frame,
	layout: Layout,
	mode: Mode,
	tail: TailView,
	frozen: bool,
	window_lines: u16,
	ascii_only: bool,
	/// Newest backlog entries shown in the backlog window.
	window_entries: Vec<Entry>,
	window_refreshed: Option<Instant>,
	quit: bool,
}

/// Runs the UI until the user quits or `shutdown` is set.
pub fn run(
	sim: Sim,
	receiver: Receiver<SourceMsg>,
	source_count: usize,
	options: UiOptions,
	shutdown: &AtomicBool,
) -> Result<()> {
	let (cols, rows) = terminal::size()?;
	let layout = Layout::new(cols, rows, options.window_lines);
	let mut app = App {
		sim,
		palette: Palette {
			min_level: options.min_brightness,
			..Palette::new(options.color_mode)
		},
		renderer: Renderer::new(),
		frame: Frame::new(cols, rows),
		layout,
		mode: Mode::Rain,
		tail: TailView::new(),
		frozen: false,
		window_lines: options.window_lines,
		ascii_only: options.ascii_only,
		window_entries: Vec::new(),
		window_refreshed: None,
		quit: false,
	};
	app.sim.resize(layout.cols, layout.rain_rows);
	let input = InputReader::spawn()?;

	let early_live = app.preload(&receiver, &input, source_count, shutdown)?;
	for line in early_live {
		app.sim.push(line);
	}

	let frame_interval = Duration::from_secs_f64(1.0 / options.fps.clamp(5, 60) as f64);
	let mut stdout = io::stdout();
	let mut last_tick = Instant::now();
	let mut receiver_open = true;

	while !app.quit && !shutdown.load(Ordering::SeqCst) {
		let frame_start = Instant::now();
		if terminal_hung_up() {
			break;
		}
		if receiver_open {
			receiver_open = app.drain(&receiver);
		}
		let now = Instant::now();
		let dt = (now - last_tick).min(MAX_TICK);
		last_tick = now;
		if !app.frozen {
			app.sim.tick(dt);
		}
		app.compose();
		app.renderer.render(&app.frame, &mut stdout)?;

		let deadline = frame_start + frame_interval;
		loop {
			let remaining = deadline.saturating_duration_since(Instant::now());
			if app.quit || shutdown.load(Ordering::SeqCst) {
				break;
			}
			match input.next(remaining) {
				Input::Event(event) => app.handle_event(event)?,
				Input::Timeout => break,
				// The terminal can no longer be read: it is gone.
				Input::Closed => app.quit = true,
			}
		}
	}
	stdout.flush()?;
	Ok(())
}

impl App {
	/// Waits until every source has sent its preload, or the timeout, then
	/// inserts the preloaded lines sorted by time. Returns live lines that
	/// arrived meanwhile.
	fn preload(
		&mut self,
		receiver: &Receiver<SourceMsg>,
		input: &InputReader,
		source_count: usize,
		shutdown: &AtomicBool,
	) -> Result<Vec<LogLine>> {
		let deadline = Instant::now() + PRELOAD_TIMEOUT;
		let mut preloaded = Vec::new();
		let mut early_live = Vec::new();
		let mut done_count = 0;
		while done_count < source_count && !shutdown.load(Ordering::SeqCst) {
			let remaining = deadline.saturating_duration_since(Instant::now());
			if remaining.is_zero() {
				break;
			}
			match receiver.recv_timeout(remaining.min(Duration::from_millis(50))) {
				Ok(SourceMsg::Preload(line)) => preloaded.push(line),
				Ok(SourceMsg::PreloadDone(_)) => done_count += 1,
				Ok(SourceMsg::Live(line)) => early_live.push(line),
				Err(RecvTimeoutError::Timeout) => {}
				Err(RecvTimeoutError::Disconnected) => break,
			}
			// Let the user quit while waiting.
			loop {
				match input.next(Duration::ZERO) {
					Input::Event(Event::Key(key)) if is_quit_key(&key) => {
						self.quit = true;
						return Ok(early_live);
					}
					Input::Event(_) => {}
					Input::Timeout => break,
					Input::Closed => {
						self.quit = true;
						return Ok(early_live);
					}
				}
			}
		}
		preloaded.sort_by_key(|line| line.ts);
		self.sim.preload(preloaded);
		Ok(early_live)
	}

	/// Moves waiting lines from the channel into the simulation, within a
	/// time and count budget. Returns `false` once every sender is gone.
	fn drain(&mut self, receiver: &Receiver<SourceMsg>) -> bool {
		let started = Instant::now();
		for count in 0..DRAIN_MAX_LINES {
			if count % 256 == 255 && started.elapsed() > DRAIN_BUDGET {
				break;
			}
			match receiver.try_recv() {
				Ok(SourceMsg::Live(line)) => self.sim.push(line),
				// A late preload line (after the timeout) is kept quietly.
				Ok(SourceMsg::Preload(line)) => self.sim.preload(vec![line]),
				Ok(SourceMsg::PreloadDone(_)) => {}
				Err(TryRecvError::Empty) => break,
				Err(TryRecvError::Disconnected) => return false,
			}
		}
		true
	}

	fn handle_event(&mut self, event: Event) -> Result<()> {
		match event {
			Event::Key(key) if key.kind != KeyEventKind::Release => self.handle_key(key),
			Event::Resize(_, _) => self.resize()?,
			_ => {}
		}
		Ok(())
	}

	fn handle_key(&mut self, key: KeyEvent) {
		let typing_filter = self.mode == Mode::Tail && self.tail.is_editing();
		if is_ctrl_c(&key) || (!typing_filter && is_quit_key(&key)) {
			self.quit = true;
			return;
		}
		match self.mode {
			Mode::Rain => match key.code {
				KeyCode::Enter => {
					self.mode = Mode::Tail;
					self.tail.follow();
				}
				KeyCode::Char(' ') => self.frozen = !self.frozen,
				KeyCode::Char('a') => self.sim.ack_oldest(),
				KeyCode::Char('A') => self.sim.ack_all(),
				_ => {}
			},
			Mode::Tail => {
				let visible_rows = self.layout.rows.saturating_sub(1) as usize;
				self.tail.update(self.sim.backlog());
				if self.tail.handle_key(key, self.sim.backlog(), visible_rows) == TailAction::Leave
				{
					self.mode = Mode::Rain;
				}
			}
		}
	}

	fn resize(&mut self) -> Result<()> {
		let (cols, rows) = terminal::size()?;
		self.layout = Layout::new(cols, rows, self.window_lines);
		self.frame = Frame::new(cols, rows);
		self.window_refreshed = None;
		self.renderer.invalidate();
		self.sim.resize(self.layout.cols, self.layout.rain_rows);
		Ok(())
	}

	fn compose(&mut self) {
		self.frame.clear();
		if self.layout.too_small {
			let message = if self.layout.cols >= 18 {
				"terminal too small"
			} else {
				"too small"
			};
			let start = self.layout.cols.saturating_sub(message.len() as u16) / 2;
			self.frame.put_str(
				start,
				self.layout.rows / 2,
				message,
				TermColor::Default,
				TermColor::Default,
			);
			return;
		}
		match self.mode {
			Mode::Rain => self.compose_rain(),
			Mode::Tail => {
				self.tail.update(self.sim.backlog());
				self.tail.draw(
					&mut self.frame,
					self.sim.backlog(),
					self.sim.rules(),
					&self.palette,
					self.ascii_only,
				);
			}
		}
	}

	fn compose_rain(&mut self) {
		let ack_count = self.sim.ack_count();
		let ack_text = format!("ack {ack_count}");
		let right = (ack_count > 0).then_some((ack_text.as_str(), self.palette.ack));
		let help = if self.frozen {
			format!("FROZEN  {}", rain_view::RAIN_HELP)
		} else {
			rain_view::RAIN_HELP.to_string()
		};
		rain_view::draw_help_bar(&mut self.frame, 0, &help, right, &self.palette);
		rain_view::draw_rain(
			&mut self.frame,
			&self.sim,
			self.layout.rain_top,
			&self.palette,
		);
		// Undecoded glyphs change every frame, but stay put while frozen.
		let mut glyph_rng = Rng::with_seed(self.sim.now().as_nanos() as u64);
		rain_view::draw_caught(
			&mut self.frame,
			&self.sim,
			self.layout.rain_top,
			&self.palette,
			&mut glyph_rng,
		);
		self.draw_window();
	}

	/// Draws the backlog window, refreshing its content at most 5 times per
	/// second so it stays readable under load.
	fn draw_window(&mut self) {
		if self.layout.window_rows == 0 {
			return;
		}
		let rows = self.layout.window_rows as usize;
		let stale = self
			.window_refreshed
			.is_none_or(|refreshed| refreshed.elapsed() >= WINDOW_REFRESH);
		if stale && !self.frozen || self.window_entries.len() != rows.min(self.sim.backlog().len())
		{
			let backlog = self.sim.backlog();
			let skip = backlog.len().saturating_sub(rows);
			self.window_entries = backlog.iter().skip(skip).cloned().collect();
			self.window_refreshed = Some(Instant::now());
		}
		let style = tail_view::RowStyle {
			timestamp: self.palette.window_timestamp,
			message: self.palette.window_message,
			background: self.palette.window_bg,
		};
		let empty_rows = rows - self.window_entries.len();
		for row in 0..rows as u16 {
			let screen_row = self.layout.window_top + row;
			match (row as usize)
				.checked_sub(empty_rows)
				.and_then(|index| self.window_entries.get(index))
			{
				Some(entry) => tail_view::draw_entry(
					&mut self.frame,
					screen_row,
					entry,
					self.sim.rules(),
					&style,
					&self.palette,
					self.ascii_only,
				),
				None => self.frame.fill_row(
					screen_row,
					0,
					crate::term::Cell {
						ch: ' ',
						fg: style.message,
						bg: style.background,
					},
				),
			}
		}
	}
}

fn is_ctrl_c(key: &KeyEvent) -> bool {
	key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn is_quit_key(key: &KeyEvent) -> bool {
	key.code == KeyCode::Char('q') || is_ctrl_c(key)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn rain_level_never_drops_below_minimum() {
		let palette = Palette {
			min_level: 0.3,
			..Palette::new(ColorMode::TrueColor)
		};
		assert!((palette.rain_level(0.07) - 0.3125).abs() < 1e-6);
		assert!((palette.rain_level(0.8) - 0.8125).abs() < 1e-6);
		let plain = Palette::new(ColorMode::TrueColor);
		assert!((plain.rain_level(0.07) - 0.0625).abs() < 1e-6);
	}
}
