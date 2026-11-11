//! The simulation: backlog, rule matching, rain and caught lines.
//!
//! Never touches the terminal. Time only advances through [`Sim::tick`] and
//! randomness comes from a seedable generator, so it is deterministic.

pub mod caught;
pub mod rain;

use std::time::Duration;

use fastrand::Rng;

use crate::backlog::{Backlog, Entry};
use crate::line::LogLine;
use crate::rules::RuleSet;
use crate::term::Rgb;

pub use caught::{CaughtLine, CharView};
pub use rain::{GLYPHS, RainCell, RainDrop, random_glyph};

use caught::{Caught, CaughtSpec};
use rain::{DropSpec, Rain};

/// Average number of replay drops started per second.
pub const REPLAY_RATE: f32 = 14.0;

/// Tunables of the simulation.
#[derive(Debug, Clone)]
pub struct SimSettings {
	pub speed: f32,
	pub speed_jitter: f32,
	pub fill: f32,
	pub fade_half_life: Duration,
	pub replay: bool,
	pub replay_brightness: f32,
	pub replay_reserve: f32,
	pub default_color: Rgb,
	pub backlog: usize,
	pub ascii_only: bool,
}

impl Default for SimSettings {
	fn default() -> Self {
		Self {
			speed: 16.0,
			speed_jitter: 0.35,
			fill: 0.35,
			fade_half_life: Duration::from_millis(480),
			replay: true,
			replay_brightness: 0.55,
			replay_reserve: 0.15,
			default_color: Rgb(0x28, 0xff, 0x6e),
			backlog: 10_000,
			ascii_only: false,
		}
	}
}

pub struct Sim {
	settings: SimSettings,
	rules: RuleSet,
	backlog: Backlog,
	rng: Rng,
	rain: Rain,
	caught: Caught,
	/// Simulation clock.
	now: Duration,
	replay_credit: f32,
}

impl Sim {
	pub fn new(settings: SimSettings, rules: RuleSet, rng: Rng) -> Self {
		let backlog = Backlog::new(settings.backlog);
		Self {
			settings,
			rules,
			backlog,
			rng,
			rain: Rain::new(0, 0),
			caught: Caught::default(),
			now: Duration::ZERO,
			replay_credit: 0.0,
		}
	}

	pub fn settings(&self) -> &SimSettings {
		&self.settings
	}

	pub fn rules(&self) -> &RuleSet {
		&self.rules
	}

	pub fn backlog(&self) -> &Backlog {
		&self.backlog
	}

	pub fn rain(&self) -> &Rain {
		&self.rain
	}

	pub fn caught_lines(&self) -> &[CaughtLine] {
		self.caught.lines()
	}

	pub fn pending_len(&self) -> usize {
		self.caught.pending_len()
	}

	/// Current simulation time.
	pub fn now(&self) -> Duration {
		self.now
	}

	/// Number of lines waiting for acknowledgement, on screen and pending.
	pub fn ack_count(&self) -> usize {
		self.caught.ack_count()
	}

	/// Maximum number of drops for unmatched lines.
	pub fn max_drops(&self) -> usize {
		((self.settings.fill * self.rain.cols() as f32).round() as usize).max(1)
	}

	/// Number of active drops below which replays are started.
	pub fn replay_limit(&self) -> usize {
		(self.max_drops() as f32 * (1.0 - self.settings.replay_reserve)).floor() as usize
	}

	/// Sets the rain area size: `cols` wide, `rain_rows` tall.
	pub fn resize(&mut self, cols: u16, rain_rows: u16) {
		self.rain.resize(cols, rain_rows);
		self.caught.resize(
			self.now,
			cols,
			rain_rows,
			self.settings.ascii_only,
			&mut self.rng,
		);
	}

	/// Adds startup lines to the backlog, with their rule but without rain
	/// or caught lines.
	pub fn preload(&mut self, lines: Vec<LogLine>) {
		for line in lines {
			let rule = self.rules.first_match(&line.text);
			self.backlog.push(Entry { line, rule });
		}
	}

	/// Handles one live line.
	pub fn push(&mut self, line: LogLine) {
		let rule_id = self.rules.first_match(&line.text);
		let speed = self.random_speed();
		match rule_id.and_then(|id| self.rules.get(id).map(|rule| (id, rule.clone()))) {
			Some((id, rule)) => {
				self.rain.spawn(
					DropSpec {
						text: &line.text,
						color: rule.color,
						replay: false,
						offset: 0,
						force: true,
					},
					speed,
					&mut self.rng,
				);
				if rule.hold.decodes() {
					self.caught.add(CaughtSpec {
						rule: id,
						color: rule.color,
						hold: rule.hold,
						full_text: format!("{} {}", line.ts.format("%H:%M:%S"), line.text),
					});
				}
			}
			None => {
				if self.rain.active_drops() < self.max_drops() {
					self.rain.spawn(
						DropSpec {
							text: &line.text,
							color: self.settings.default_color,
							replay: false,
							offset: 0,
							force: false,
						},
						speed,
						&mut self.rng,
					);
				}
			}
		}
		self.backlog.push(Entry {
			line,
			rule: rule_id,
		});
	}

	/// Advances the simulation by `dt`.
	pub fn tick(&mut self, dt: Duration) {
		self.now += dt;
		self.rain.tick(
			dt,
			self.settings.fade_half_life,
			self.settings.replay_brightness,
			&mut self.rng,
		);
		self.caught.tick(
			self.now,
			self.rain.cols(),
			self.rain.rows(),
			self.settings.ascii_only,
			&mut self.rng,
		);
		self.replay(dt);
	}

	pub fn ack_oldest(&mut self) {
		self.caught.ack_oldest(self.now);
	}

	pub fn ack_all(&mut self) {
		self.caught.ack_all(self.now);
	}

	fn random_speed(&mut self) -> f32 {
		let jitter = self.settings.speed_jitter;
		let factor = 1.0 - jitter + self.rng.f32() * 2.0 * jitter;
		(self.settings.speed * factor).max(0.1)
	}

	fn replay(&mut self, dt: Duration) {
		if !self.settings.replay || self.backlog.is_empty() {
			self.replay_credit = 0.0;
			return;
		}
		let limit = self.replay_limit();
		self.replay_credit += REPLAY_RATE * dt.as_secs_f32();
		while self.replay_credit >= 1.0 {
			if self.rain.active_drops() >= limit || !self.rain.has_free_column() {
				self.replay_credit = self.replay_credit.min(1.0);
				return;
			}
			self.replay_credit -= 1.0;
			let index = self.rng.usize(..self.backlog.len());
			let text = match self.backlog.get(index) {
				Some(entry) => entry.line.text.clone(),
				None => return,
			};
			let length = text.chars().count().max(1);
			let offset = self.rng.usize(..length);
			let speed = self.random_speed();
			self.rain.spawn(
				DropSpec {
					text: &text,
					color: self.settings.default_color,
					replay: true,
					offset,
					force: false,
				},
				speed,
				&mut self.rng,
			);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::rules::Rule;
	use chrono::Local;

	const FRAME: Duration = Duration::from_millis(50);

	fn line(text: &str) -> LogLine {
		LogLine::new(Local::now(), 0, text)
	}

	fn rules() -> RuleSet {
		RuleSet::new(vec![
			Rule::new("ack", "CRIT", "#ff0000", Some("ack")).unwrap(),
			Rule::new("timed", "WARN", "#ffb428", Some("10s")).unwrap(),
			Rule::new("color", "INFO", "#3cc8ff", None).unwrap(),
		])
		.unwrap()
	}

	fn sim_with(settings: SimSettings, cols: u16, rows: u16) -> Sim {
		let mut sim = Sim::new(settings, rules(), Rng::with_seed(42));
		sim.resize(cols, rows);
		sim
	}

	fn no_replay() -> SimSettings {
		SimSettings {
			replay: false,
			..SimSettings::default()
		}
	}

	fn run(sim: &mut Sim, duration: Duration) {
		let steps = duration.as_millis() / FRAME.as_millis();
		for _ in 0..steps {
			sim.tick(FRAME);
		}
	}

	#[test]
	fn unmatched_drops_never_exceed_max() {
		let mut sim = sim_with(no_replay(), 80, 22);
		assert_eq!(sim.max_drops(), 28);
		for round in 0..200 {
			for index in 0..50 {
				sim.push(line(&format!("plain line {round} {index}")));
				assert!(sim.rain().active_drops() <= sim.max_drops());
			}
			sim.tick(FRAME);
		}
		assert_eq!(sim.backlog().len(), 10_000);
	}

	#[test]
	fn matched_line_always_drops() {
		let mut sim = sim_with(no_replay(), 20, 22);
		for index in 0..30 {
			sim.push(line(&format!("plain {index}")));
		}
		let before = sim.rain().active_drops();
		assert_eq!(before, sim.max_drops());
		sim.push(line("INFO login"));
		assert_eq!(sim.rain().active_drops(), before + 1);
		// Even with every column busy.
		for _ in 0..40 {
			sim.push(line("INFO again"));
		}
		assert_eq!(sim.rain().active_drops(), before + 41);
		let colored = sim
			.rain()
			.drops()
			.iter()
			.filter(|drop| drop.color == Rgb(0x3c, 0xc8, 0xff))
			.count();
		assert_eq!(colored, 41);
	}

	#[test]
	fn drops_use_distinct_free_columns() {
		let settings = SimSettings {
			fill: 1.0,
			..no_replay()
		};
		let mut sim = sim_with(settings, 30, 22);
		for index in 0..30 {
			sim.push(line(&format!("line {index}")));
			let mut columns: Vec<u16> = sim.rain().drops().iter().map(|drop| drop.col).collect();
			let count = columns.len();
			columns.sort();
			columns.dedup();
			assert_eq!(columns.len(), count, "two drops share a column");
		}
	}

	#[test]
	fn replay_tops_up_to_reserve_limit() {
		let mut sim = sim_with(SimSettings::default(), 80, 22);
		sim.preload(vec![line("old line one"), line("old line two")]);
		let limit = sim.replay_limit();
		assert_eq!(limit, 23);
		let mut peak = 0;
		for _ in 0..200 {
			sim.tick(FRAME);
			peak = peak.max(sim.rain().active_drops());
			assert!(sim.rain().active_drops() <= limit);
		}
		assert!(
			peak >= limit - 6,
			"replay did not fill the rain: peak {peak}"
		);
		assert!(sim.rain().drops().iter().all(|drop| drop.replay));
	}

	#[test]
	fn replay_stops_when_live_lines_fill() {
		let mut sim = sim_with(SimSettings::default(), 80, 22);
		sim.preload(vec![line("old line")]);
		for _ in 0..100 {
			for index in 0..40 {
				sim.push(line(&format!("live {index}")));
			}
			sim.tick(FRAME);
		}
		let live_before: usize = sim.rain().drops().iter().filter(|drop| drop.replay).count();
		for _ in 0..20 {
			for index in 0..40 {
				sim.push(line(&format!("live {index}")));
			}
			sim.tick(FRAME);
			assert!(sim.rain().active_drops() <= sim.max_drops());
		}
		let replays = sim.rain().drops().iter().filter(|drop| drop.replay).count();
		assert!(
			replays <= live_before.max(1),
			"replays kept starting under load: {replays}"
		);
	}

	#[test]
	fn no_replay_without_backlog_or_when_disabled() {
		let mut sim = sim_with(SimSettings::default(), 80, 22);
		run(&mut sim, Duration::from_secs(2));
		assert_eq!(sim.rain().active_drops(), 0);
		let mut disabled = sim_with(no_replay(), 80, 22);
		disabled.preload(vec![line("old")]);
		run(&mut disabled, Duration::from_secs(2));
		assert_eq!(disabled.rain().active_drops(), 0);
	}

	#[test]
	fn replay_uses_default_color_and_random_offset() {
		let mut sim = sim_with(SimSettings::default(), 80, 22);
		sim.preload(vec![line("CRIT matched preloaded line with some length")]);
		let mut nonzero_offset = false;
		for _ in 0..100 {
			sim.tick(FRAME);
			for drop in sim.rain().drops() {
				assert_eq!(drop.color, SimSettings::default().default_color);
				nonzero_offset |= drop.offset > 0;
			}
		}
		assert!(nonzero_offset);
		assert_eq!(sim.caught_lines().len(), 0);
		assert_eq!(sim.ack_count(), 0);
	}

	#[test]
	fn timed_line_gone_after_hold_and_dissolve() {
		let mut sim = sim_with(no_replay(), 80, 22);
		sim.push(line("WARN disk almost full"));
		sim.tick(FRAME);
		assert_eq!(sim.caught_lines().len(), 1);
		run(&mut sim, Duration::from_secs(10));
		assert_eq!(sim.caught_lines().len(), 1, "gone before hold + dissolve");
		run(&mut sim, Duration::from_millis(1500));
		assert_eq!(sim.caught_lines().len(), 0);
	}

	#[test]
	fn caught_line_decodes_and_dissolves() {
		let mut sim = sim_with(no_replay(), 80, 22);
		sim.push(line("WARN x"));
		sim.tick(Duration::ZERO);
		let caught = &sim.caught_lines()[0];
		let now = sim.now();
		assert_eq!(caught.char_view(0, now), CharView::Glyph);
		let after_decode = now + Duration::from_secs(1);
		assert!(
			(0..caught.chars.len())
				.all(|index| caught.char_view(index, after_decode) == CharView::Revealed)
		);
		let mid_dissolve = now + Duration::from_millis(10_750);
		let blanks = (0..caught.chars.len())
			.filter(|index| caught.char_view(*index, mid_dissolve) == CharView::Blank)
			.count();
		assert!(blanks > 0 && blanks < caught.chars.len());
		let text: String = caught.chars.iter().collect();
		assert!(text.ends_with(" WARN x"));
		assert_eq!(text.len(), "HH:MM:SS WARN x".len());
	}

	#[test]
	fn ack_line_stays_until_acknowledged() {
		let mut sim = sim_with(no_replay(), 80, 22);
		sim.push(line("CRIT first"));
		sim.tick(FRAME);
		sim.push(line("CRIT second"));
		sim.tick(FRAME);
		assert_eq!(sim.ack_count(), 2);
		run(&mut sim, Duration::from_secs(600));
		assert_eq!(sim.caught_lines().len(), 2);
		let text: String = sim.caught_lines()[0].chars.iter().collect();
		assert!(text.ends_with(" [a]"));

		sim.ack_oldest();
		assert_eq!(sim.ack_count(), 1);
		let remaining: Vec<String> = sim
			.caught_lines()
			.iter()
			.filter(|caught| caught.waiting())
			.map(|caught| caught.chars.iter().collect())
			.collect();
		assert_eq!(remaining.len(), 1);
		assert!(remaining[0].contains("CRIT second"));
		run(&mut sim, Duration::from_millis(1550));
		assert_eq!(sim.caught_lines().len(), 1);

		sim.ack_all();
		assert_eq!(sim.ack_count(), 0);
		run(&mut sim, Duration::from_millis(1550));
		assert_eq!(sim.caught_lines().len(), 0);
	}

	#[test]
	fn full_screen_evicts_oldest_non_ack_line() {
		// 6 rain rows: 4 usable rows for caught lines.
		let mut sim = sim_with(no_replay(), 80, 6);
		sim.push(line("WARN one"));
		sim.tick(FRAME);
		sim.push(line("CRIT two"));
		sim.tick(FRAME);
		sim.push(line("WARN three"));
		sim.tick(FRAME);
		sim.push(line("WARN four"));
		sim.tick(FRAME);
		assert_eq!(sim.caught_lines().len(), 4);
		sim.push(line("WARN five"));
		sim.tick(FRAME);
		let texts: Vec<String> = sim
			.caught_lines()
			.iter()
			.map(|caught| caught.chars.iter().collect())
			.collect();
		assert_eq!(texts.len(), 4);
		assert!(
			!texts.iter().any(|text| text.contains("one")),
			"oldest timed line not evicted"
		);
		assert!(texts.iter().any(|text| text.contains("two")));
		assert!(texts.iter().any(|text| text.contains("five")));
		let mut rows: Vec<u16> = sim.caught_lines().iter().map(|caught| caught.row).collect();
		rows.sort();
		assert_eq!(rows, [1, 2, 3, 4]);
	}

	#[test]
	fn pending_queue_when_all_rows_wait_for_ack() {
		let mut sim = sim_with(no_replay(), 80, 6);
		for index in 0..4 {
			sim.push(line(&format!("CRIT {index}")));
		}
		sim.tick(FRAME);
		assert_eq!(sim.caught_lines().len(), 4);
		for index in 0..60 {
			sim.push(line(&format!("WARN pending {index}")));
		}
		sim.tick(FRAME);
		assert_eq!(sim.caught_lines().len(), 4);
		assert_eq!(sim.pending_len(), caught::MAX_PENDING);
		assert_eq!(sim.ack_count(), 4);

		sim.ack_oldest();
		run(&mut sim, Duration::from_millis(1600));
		assert_eq!(sim.caught_lines().len(), 4);
		assert_eq!(sim.pending_len(), caught::MAX_PENDING - 1);
		// The oldest kept pending line (10, after 0..9 were discarded) was placed.
		assert!(sim.caught_lines().iter().any(|caught| {
			caught
				.chars
				.iter()
				.collect::<String>()
				.contains("pending 10")
		}));
	}

	#[test]
	fn preloaded_lines_do_not_rain_or_decode() {
		let mut sim = sim_with(no_replay(), 80, 22);
		sim.preload(vec![
			line("CRIT old"),
			line("WARN old"),
			line("INFO old"),
			line("plain"),
		]);
		run(&mut sim, Duration::from_secs(1));
		assert_eq!(sim.rain().active_drops(), 0);
		assert_eq!(sim.caught_lines().len(), 0);
		assert_eq!(sim.ack_count(), 0);
		let rule_ids: Vec<Option<usize>> = sim.backlog().iter().map(|entry| entry.rule).collect();
		assert_eq!(rule_ids, [Some(0), Some(1), Some(2), None]);
	}

	#[test]
	fn resize_replaces_lines_that_no_longer_fit() {
		let mut sim = sim_with(no_replay(), 120, 22);
		sim.push(line(&format!("CRIT {}", "x".repeat(150))));
		sim.tick(FRAME);
		assert_eq!(sim.caught_lines()[0].chars.len(), 116);
		sim.resize(40, 5);
		let caught = &sim.caught_lines()[0];
		assert_eq!(caught.chars.len(), 36);
		assert!(caught.row >= 1 && caught.row <= 3);
		assert!(caught.col >= 1 && caught.col as usize + caught.chars.len() < 40);
		let text: String = caught.chars.iter().collect();
		assert!(text.ends_with("… [a]"));
	}

	#[test]
	fn deterministic_with_seed() {
		let build = || {
			let mut sim = sim_with(SimSettings::default(), 80, 22);
			sim.preload(vec![line("abc def")]);
			run(&mut sim, Duration::from_secs(2));
			sim.rain()
				.drops()
				.iter()
				.map(|drop| (drop.col, drop.next_row))
				.collect::<Vec<_>>()
		};
		assert_eq!(build(), build());
	}
}
