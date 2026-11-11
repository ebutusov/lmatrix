//! Main loop, modes and key handling.

pub mod rain_view;

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal;
use fastrand::Rng;

use crate::line::{LogLine, SourceMsg};
use crate::sim::Sim;
use crate::term::{ColorMode, Frame, Renderer, Rgb, TermColor};

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

/// Display options for the UI.
#[derive(Debug, Clone)]
pub struct UiOptions {
	pub fps: u32,
	pub color_mode: ColorMode,
	pub window_lines: u16,
	pub ascii_only: bool,
}

/// All colors, converted for the active color mode.
#[derive(Debug, Clone)]
pub struct Palette {
	pub mode: ColorMode,
	pub head_rgb: Rgb,
	pub help_bg: TermColor,
	pub help_fg: TermColor,
	pub ack: TermColor,
	pub marker: TermColor,
	pub glyph_dim: TermColor,
	pub error: TermColor,
}

impl Palette {
	pub fn new(mode: ColorMode) -> Self {
		Self {
			mode,
			head_rgb: Rgb(0xe8, 0xff, 0xf0),
			help_bg: mode.solid(Rgb(0x0d, 0x24, 0x14)),
			help_fg: mode.solid(Rgb(0x4f, 0xbf, 0x78)),
			ack: mode.solid(Rgb(0xff, 0xb4, 0x28)),
			marker: mode.solid(Rgb::WHITE),
			glyph_dim: mode.solid(Rgb(0x1e, 0x96, 0x46)),
			error: mode.solid(Rgb(0xff, 0x46, 0x3c)),
		}
	}

	/// Rounds a brightness level to a fixed number of steps, so a fading
	/// cell only changes color (and needs output) when the step changes.
	pub fn quantize_level(&self, level: f32) -> f32 {
		const STEPS: f32 = 32.0;
		(level * STEPS).round() / STEPS
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
	Rain,
}

/// Screen geometry.
#[derive(Debug, Clone, Copy)]
struct Layout {
	cols: u16,
	rows: u16,
	too_small: bool,
	rain_top: u16,
	rain_rows: u16,
}

impl Layout {
	fn new(cols: u16, rows: u16) -> Self {
		let too_small = cols < MIN_COLS || rows < MIN_ROWS;
		let rain_rows = rows.saturating_sub(1);
		Self {
			cols,
			rows,
			too_small,
			rain_top: 1,
			rain_rows,
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
	rng: Rng,
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
	let layout = Layout::new(cols, rows);
	let mut app = App {
		sim,
		palette: Palette::new(options.color_mode),
		renderer: Renderer::new(),
		frame: Frame::new(cols, rows),
		layout,
		mode: Mode::Rain,
		rng: Rng::new(),
		quit: false,
	};
	app.sim.resize(layout.cols, layout.rain_rows);

	let early_live = app.preload(&receiver, source_count, shutdown)?;
	for line in early_live {
		app.sim.push(line);
	}

	let frame_interval = Duration::from_secs_f64(1.0 / options.fps.clamp(5, 60) as f64);
	let mut stdout = io::stdout();
	let mut last_tick = Instant::now();
	let mut receiver_open = true;

	while !app.quit && !shutdown.load(Ordering::SeqCst) {
		let frame_start = Instant::now();
		if receiver_open {
			receiver_open = app.drain(&receiver);
		}
		let now = Instant::now();
		let dt = (now - last_tick).min(MAX_TICK);
		last_tick = now;
		app.sim.tick(dt);
		app.compose();
		app.renderer.render(&app.frame, &mut stdout)?;

		let deadline = frame_start + frame_interval;
		loop {
			let remaining = deadline.saturating_duration_since(Instant::now());
			if app.quit || shutdown.load(Ordering::SeqCst) {
				break;
			}
			if !event::poll(remaining)? {
				break;
			}
			let event = event::read()?;
			app.handle_event(event)?;
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
			while event::poll(Duration::ZERO)? {
				if let Event::Key(key) = event::read()?
					&& is_quit_key(&key)
				{
					self.quit = true;
					return Ok(early_live);
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
		if is_quit_key(&key) {
			self.quit = true;
			return;
		}
		match self.mode {
			Mode::Rain => match key.code {
				KeyCode::Char('a') => self.sim.ack_oldest(),
				KeyCode::Char('A') => self.sim.ack_all(),
				_ => {}
			},
		}
	}

	fn resize(&mut self) -> Result<()> {
		let (cols, rows) = terminal::size()?;
		self.layout = Layout::new(cols, rows);
		self.frame = Frame::new(cols, rows);
		self.renderer.invalidate();
		self.sim.resize(self.layout.cols, self.layout.rain_rows);
		Ok(())
	}

	fn compose(&mut self) {
		self.frame.clear();
		if self.layout.too_small {
			let message = "terminal too small";
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
		}
	}

	fn compose_rain(&mut self) {
		let ack_count = self.sim.ack_count();
		let ack_text = format!("ack {ack_count}");
		let right = (ack_count > 0).then_some((ack_text.as_str(), self.palette.ack));
		rain_view::draw_help_bar(
			&mut self.frame,
			0,
			rain_view::RAIN_HELP,
			right,
			&self.palette,
		);
		rain_view::draw_rain(
			&mut self.frame,
			&self.sim,
			self.layout.rain_top,
			&self.palette,
		);
		rain_view::draw_caught(
			&mut self.frame,
			&self.sim,
			self.layout.rain_top,
			&self.palette,
			&mut self.rng,
		);
	}
}

fn is_quit_key(key: &KeyEvent) -> bool {
	matches!(key.code, KeyCode::Char('q'))
		|| (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
}
