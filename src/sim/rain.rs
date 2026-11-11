//! Falling drops, fading cells, fill factor.

use std::time::Duration;

use fastrand::Rng;

use crate::term::Rgb;

/// Glyphs used in place of spaces and for undecoded characters.
pub const GLYPHS: &[char] = &[
	'0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f', ':', '/', '[',
	']', '<', '>', '=', '+', '-', '*', '#', '%', '&', '$', '@',
];

pub fn random_glyph(rng: &mut Rng) -> char {
	GLYPHS[rng.usize(..GLYPHS.len())]
}

/// Levels below this are cleared (they are not drawn anyway).
const LEVEL_FLOOR: f32 = 0.05;

/// One cell of the rain grid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RainCell {
	pub ch: char,
	pub color: Rgb,
	/// Brightness, 0 = empty, 1 = just written.
	pub level: f32,
}

impl RainCell {
	pub const EMPTY: RainCell = RainCell {
		ch: ' ',
		color: Rgb(0, 0, 0),
		level: 0.0,
	};
}

/// A log line falling down one column.
#[derive(Debug, Clone)]
pub struct RainDrop {
	pub col: u16,
	/// Row the next character is written to.
	pub next_row: u16,
	/// Fractional rows accumulated toward the next advance.
	progress: f32,
	/// Whether the first character has been written.
	started: bool,
	/// Rows per second.
	pub speed: f32,
	text: Box<[char]>,
	/// Index in `text` of the first character written.
	pub offset: usize,
	pub replay: bool,
	pub color: Rgb,
}

impl RainDrop {
	fn char_for_row(&self, row: u16) -> char {
		if self.text.is_empty() {
			return ' ';
		}
		self.text[(self.offset + row as usize) % self.text.len()]
	}
}

/// Parameters a drop is created with.
pub struct DropSpec<'a> {
	pub text: &'a str,
	pub color: Rgb,
	pub replay: bool,
	/// Start offset in characters (replays start at a random position).
	pub offset: usize,
	/// Use any column if none is free (matched lines always rain).
	pub force: bool,
}

/// The rain grid and its drops.
#[derive(Debug, Clone)]
pub struct Rain {
	cols: u16,
	rows: u16,
	cells: Vec<RainCell>,
	drops: Vec<RainDrop>,
	/// Number of active drops per column.
	column_drops: Vec<u16>,
}

impl Rain {
	pub fn new(cols: u16, rows: u16) -> Self {
		Self {
			cols,
			rows,
			cells: vec![RainCell::EMPTY; cols as usize * rows as usize],
			drops: Vec::new(),
			column_drops: vec![0; cols as usize],
		}
	}

	pub fn cols(&self) -> u16 {
		self.cols
	}

	pub fn rows(&self) -> u16 {
		self.rows
	}

	pub fn drops(&self) -> &[RainDrop] {
		&self.drops
	}

	pub fn active_drops(&self) -> usize {
		self.drops.len()
	}

	pub fn cell(&self, col: u16, row: u16) -> RainCell {
		if col < self.cols && row < self.rows {
			self.cells[row as usize * self.cols as usize + col as usize]
		} else {
			RainCell::EMPTY
		}
	}

	pub fn has_free_column(&self) -> bool {
		self.column_drops.contains(&0)
	}

	/// Rebuilds the grid for a new size. Drops whose column no longer exists
	/// are removed; the others continue (and end at the new bottom).
	pub fn resize(&mut self, cols: u16, rows: u16) {
		self.cols = cols;
		self.rows = rows;
		self.cells = vec![RainCell::EMPTY; cols as usize * rows as usize];
		self.drops.retain(|drop| drop.col < cols);
		self.column_drops = vec![0; cols as usize];
		for drop in &self.drops {
			self.column_drops[drop.col as usize] += 1;
		}
	}

	/// Starts a drop in a random free column. Returns `false` if no column
	/// was available.
	pub fn spawn(&mut self, spec: DropSpec<'_>, speed: f32, rng: &mut Rng) -> bool {
		if self.cols == 0 || self.rows == 0 {
			return false;
		}
		let free_count = self
			.column_drops
			.iter()
			.filter(|count| **count == 0)
			.count();
		let col = if free_count > 0 {
			let pick = rng.usize(..free_count);
			self.column_drops
				.iter()
				.enumerate()
				.filter(|(_, count)| **count == 0)
				.nth(pick)
				.map(|(index, _)| index as u16)
				.unwrap_or(0)
		} else if spec.force {
			rng.u16(..self.cols)
		} else {
			return false;
		};
		let text: Box<[char]> = spec.text.chars().collect();
		let offset = if text.is_empty() {
			0
		} else {
			spec.offset % text.len()
		};
		self.column_drops[col as usize] += 1;
		self.drops.push(RainDrop {
			col,
			next_row: 0,
			progress: 0.0,
			// The first character is written on the next tick.
			started: false,
			speed,
			text,
			offset,
			replay: spec.replay,
			color: spec.color,
		});
		true
	}

	/// Advances time: fades cells, moves drops, removes finished drops.
	pub fn tick(
		&mut self,
		dt: Duration,
		fade_half_life: Duration,
		replay_brightness: f32,
		rng: &mut Rng,
	) {
		let half_life = fade_half_life.as_secs_f32().max(0.001);
		let factor = 0.5f32.powf(dt.as_secs_f32() / half_life);
		for cell in &mut self.cells {
			if cell.level > 0.0 {
				cell.level *= factor;
				if cell.level < LEVEL_FLOOR {
					*cell = RainCell::EMPTY;
				}
			}
		}

		let cols = self.cols as usize;
		let rows = self.rows;
		let seconds = dt.as_secs_f32();
		let cells = &mut self.cells;
		let column_drops = &mut self.column_drops;
		self.drops.retain_mut(|drop| {
			if !drop.started {
				drop.started = true;
				if rows == 0 {
					column_drops[drop.col as usize] -= 1;
					return false;
				}
				write_head(cells, cols, drop, rng);
				return true;
			}
			drop.progress += drop.speed * seconds;
			while drop.progress >= 1.0 {
				drop.progress -= 1.0;
				if drop.replay && drop.next_row > 0 && drop.next_row <= rows {
					let behind =
						&mut cells[(drop.next_row as usize - 1) * cols + drop.col as usize];
					behind.level = behind.level.min(replay_brightness);
				}
				if drop.next_row >= rows {
					column_drops[drop.col as usize] -= 1;
					return false;
				}
				write_head(cells, cols, drop, rng);
			}
			true
		});
	}
}

/// Writes the drop's next character at its head position and moves on.
fn write_head(cells: &mut [RainCell], cols: usize, drop: &mut RainDrop, rng: &mut Rng) {
	let mut ch = drop.char_for_row(drop.next_row);
	if ch == ' ' {
		ch = random_glyph(rng);
	}
	cells[drop.next_row as usize * cols + drop.col as usize] = RainCell {
		ch,
		color: drop.color,
		level: 1.0,
	};
	drop.next_row += 1;
}

#[cfg(test)]
mod tests {
	use super::*;

	const GREEN: Rgb = Rgb(0, 255, 0);

	fn spec(text: &str) -> DropSpec<'_> {
		DropSpec {
			text,
			color: GREEN,
			replay: false,
			offset: 0,
			force: false,
		}
	}

	#[test]
	fn drop_writes_text_and_wraps() {
		let mut rng = Rng::with_seed(1);
		let mut rain = Rain::new(1, 7);
		assert!(rain.spawn(spec("abc"), 10.0, &mut rng));
		for _ in 0..7 {
			rain.tick(
				Duration::from_millis(100),
				Duration::from_millis(480),
				0.55,
				&mut rng,
			);
		}
		let column: String = (0..7).map(|row| rain.cell(0, row).ch).collect();
		assert_eq!(column, "abcabca");
		assert_eq!(rain.active_drops(), 1);
		rain.tick(
			Duration::from_millis(100),
			Duration::from_millis(480),
			0.55,
			&mut rng,
		);
		assert_eq!(rain.active_drops(), 0);
		assert!(rain.has_free_column());
	}

	#[test]
	fn spaces_become_glyphs() {
		let mut rng = Rng::with_seed(2);
		let mut rain = Rain::new(1, 3);
		rain.spawn(spec("a b"), 30.0, &mut rng);
		rain.tick(Duration::ZERO, Duration::from_millis(480), 0.55, &mut rng);
		rain.tick(
			Duration::from_millis(70),
			Duration::from_millis(480),
			0.55,
			&mut rng,
		);
		assert_eq!(rain.cell(0, 0).ch, 'a');
		assert!(GLYPHS.contains(&rain.cell(0, 1).ch));
		assert_eq!(rain.cell(0, 2).ch, 'b');
	}

	#[test]
	fn fading_is_time_based() {
		let mut rng = Rng::with_seed(3);
		let half_life = Duration::from_millis(480);
		let mut coarse = Rain::new(1, 10);
		let mut fine = Rain::new(1, 10);
		coarse.spawn(spec("x"), 1.0, &mut rng);
		fine.spawn(spec("x"), 1.0, &mut rng);
		coarse.tick(Duration::ZERO, half_life, 0.55, &mut rng);
		fine.tick(Duration::ZERO, half_life, 0.55, &mut rng);
		coarse.tick(Duration::from_millis(480), half_life, 0.55, &mut rng);
		for _ in 0..48 {
			fine.tick(Duration::from_millis(10), half_life, 0.55, &mut rng);
		}
		assert!((coarse.cell(0, 0).level - 0.5).abs() < 0.001);
		assert!((fine.cell(0, 0).level - 0.5).abs() < 0.01);
	}

	#[test]
	fn replay_cells_are_clamped_behind_head() {
		let mut rng = Rng::with_seed(4);
		let mut rain = Rain::new(1, 5);
		rain.spawn(
			DropSpec {
				replay: true,
				..spec("abcde")
			},
			10.0,
			&mut rng,
		);
		rain.tick(Duration::ZERO, Duration::from_secs(100), 0.55, &mut rng);
		assert_eq!(rain.cell(0, 0).level, 1.0);
		rain.tick(
			Duration::from_millis(100),
			Duration::from_secs(100),
			0.55,
			&mut rng,
		);
		assert!(rain.cell(0, 0).level <= 0.55);
		assert!(rain.cell(0, 1).level > 0.95);
	}

	#[test]
	fn resize_drops_lost_columns() {
		let mut rng = Rng::with_seed(5);
		let mut rain = Rain::new(10, 5);
		for _ in 0..10 {
			rain.spawn(spec("x"), 1.0, &mut rng);
		}
		assert!(!rain.has_free_column());
		rain.resize(4, 5);
		assert_eq!(rain.active_drops(), 4);
		assert!(rain.drops().iter().all(|drop| drop.col < 4));
	}
}
