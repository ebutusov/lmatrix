//! Caught lines: decode, hold, dissolve, eviction and the pending queue.

use std::collections::VecDeque;
use std::time::Duration;

use fastrand::Rng;

use crate::line::truncate_with_ellipsis;
use crate::rules::{Hold, RuleId};
use crate::term::Rgb;

/// Characters revealed per second while decoding.
pub const DECODE_RATE: f32 = 32.0;
/// Time a line takes to dissolve.
pub const DISSOLVE_TIME: Duration = Duration::from_millis(1500);
/// Maximum number of lines waiting for a free row.
pub const MAX_PENDING: usize = 50;
/// Marker appended to lines that wait for acknowledgement.
pub const ACK_MARKER: &str = " [a]";

/// How one character of a caught line is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharView {
	/// Not shown (dissolved); the background stays cleared.
	Blank,
	/// Not yet decoded: show a random glyph.
	Glyph,
	/// Decoded: show the character.
	Revealed,
}

/// A line decoded out of the rain.
#[derive(Debug, Clone)]
pub struct CaughtLine {
	/// Arrival order; lower is older.
	pub seq: u64,
	pub rule: RuleId,
	pub color: Rgb,
	pub hold: Hold,
	/// `HH:MM:SS text`, before truncation.
	full_text: String,
	/// The displayed characters, truncated, including the ack marker.
	pub chars: Vec<char>,
	/// Dissolve rank of every character: a character disappears once the
	/// number of dissolved characters exceeds its rank.
	dissolve_rank: Vec<u32>,
	pub row: u16,
	/// Column of the first character.
	pub col: u16,
	/// Time of appearance (sim clock); `None` while not on screen yet.
	pub born: Option<Duration>,
	/// When dissolving starts (timed hold) or started (acknowledged).
	pub dissolve_start: Option<Duration>,
}

impl CaughtLine {
	/// True while the line waits for acknowledgement.
	pub fn waiting(&self) -> bool {
		self.hold == Hold::UntilAck && self.dissolve_start.is_none()
	}

	/// Range of `chars` holding the `[a]` marker, for ack lines.
	pub fn marker_range(&self) -> Option<std::ops::Range<usize>> {
		if self.hold == Hold::UntilAck && self.chars.len() >= 3 {
			Some(self.chars.len() - 3..self.chars.len())
		} else {
			None
		}
	}

	pub fn finished(&self, now: Duration) -> bool {
		self.dissolve_start
			.is_some_and(|start| now >= start + DISSOLVE_TIME)
	}

	pub fn char_view(&self, index: usize, now: Duration) -> CharView {
		let Some(born) = self.born else {
			return CharView::Blank;
		};
		if let Some(start) = self.dissolve_start
			&& now >= start
		{
			let fraction = (now - start).as_secs_f32() / DISSOLVE_TIME.as_secs_f32();
			let gone = (fraction * self.chars.len() as f32).floor() as u32;
			return if self
				.dissolve_rank
				.get(index)
				.is_none_or(|rank| *rank < gone)
			{
				CharView::Blank
			} else {
				CharView::Revealed
			};
		}
		let revealed = (now.saturating_sub(born).as_secs_f32() * DECODE_RATE).floor() as usize;
		if index < revealed {
			CharView::Revealed
		} else {
			CharView::Glyph
		}
	}

	/// Computes the displayed text for a screen `cols` wide.
	fn layout(&mut self, cols: u16, ascii_only: bool, rng: &mut Rng) {
		let max_len = (cols as usize).saturating_sub(4);
		let mut text = if self.hold == Hold::UntilAck {
			let body_max = max_len.saturating_sub(ACK_MARKER.len());
			let mut body = truncate_with_ellipsis(&self.full_text, body_max, ascii_only);
			body.push_str(ACK_MARKER);
			body
		} else {
			truncate_with_ellipsis(&self.full_text, max_len, ascii_only)
		};
		if text.chars().count() > max_len {
			text = text.chars().take(max_len).collect();
		}
		let chars: Vec<char> = text.chars().collect();
		if chars.len() != self.dissolve_rank.len() {
			let mut ranks: Vec<u32> = (0..chars.len() as u32).collect();
			rng.shuffle(&mut ranks);
			self.dissolve_rank = ranks;
		}
		self.chars = chars;
	}
}

/// Data needed to create a caught line.
#[derive(Debug, Clone)]
pub struct CaughtSpec {
	pub rule: RuleId,
	pub color: Rgb,
	pub hold: Hold,
	pub full_text: String,
}

/// All caught lines: on screen, waiting to be placed, and pending.
#[derive(Debug, Clone, Default)]
pub struct Caught {
	lines: Vec<CaughtLine>,
	/// New lines, placed on the next tick (so a frozen screen stays still).
	incoming: Vec<CaughtLine>,
	/// Lines waiting for a free row.
	pending: VecDeque<CaughtLine>,
	next_seq: u64,
}

impl Caught {
	pub fn lines(&self) -> &[CaughtLine] {
		&self.lines
	}

	pub fn pending_len(&self) -> usize {
		self.pending.len()
	}

	/// Lines waiting for acknowledgement, on screen or not yet placed.
	pub fn ack_count(&self) -> usize {
		self.lines
			.iter()
			.chain(self.incoming.iter())
			.chain(self.pending.iter())
			.filter(|line| line.waiting())
			.count()
	}

	/// Queues a new caught line; it is placed on the next tick.
	pub fn add(&mut self, spec: CaughtSpec) {
		let seq = self.next_seq;
		self.next_seq += 1;
		self.incoming.push(CaughtLine {
			seq,
			rule: spec.rule,
			color: spec.color,
			hold: spec.hold,
			full_text: spec.full_text,
			chars: Vec::new(),
			dissolve_rank: Vec::new(),
			row: 0,
			col: 0,
			born: None,
			dissolve_start: None,
		});
	}

	/// Removes finished lines and places pending and new lines.
	pub fn tick(
		&mut self,
		now: Duration,
		cols: u16,
		rain_rows: u16,
		ascii_only: bool,
		rng: &mut Rng,
	) {
		self.lines.retain(|line| !line.finished(now));
		self.place_pending(now, cols, rain_rows, ascii_only, rng);
		let incoming = std::mem::take(&mut self.incoming);
		for line in incoming {
			self.place(line, now, cols, rain_rows, ascii_only, rng);
		}
	}

	/// Acknowledges the oldest waiting line.
	pub fn ack_oldest(&mut self, now: Duration) {
		if let Some(line) = self
			.lines
			.iter_mut()
			.filter(|line| line.waiting())
			.min_by_key(|line| line.seq)
		{
			line.dissolve_start = Some(now);
			return;
		}
		// Not on screen yet: the oldest waiting line is in a queue. It would
		// dissolve at once, so it is dropped.
		let oldest_queued = self
			.incoming
			.iter()
			.chain(self.pending.iter())
			.filter(|line| line.waiting())
			.map(|line| line.seq)
			.min();
		if let Some(seq) = oldest_queued {
			self.incoming.retain(|line| line.seq != seq);
			self.pending.retain(|line| line.seq != seq);
		}
	}

	/// Acknowledges every waiting line. Queued ack lines are dropped.
	pub fn ack_all(&mut self, now: Duration) {
		for line in self.lines.iter_mut().filter(|line| line.waiting()) {
			line.dissolve_start = Some(now);
		}
		self.incoming.retain(|line| !line.waiting());
		self.pending.retain(|line| !line.waiting());
	}

	/// Re-lays out lines for a new size; lines that no longer fit are moved.
	pub fn resize(
		&mut self,
		now: Duration,
		cols: u16,
		rain_rows: u16,
		ascii_only: bool,
		rng: &mut Rng,
	) {
		let mut lines = std::mem::take(&mut self.lines);
		lines.sort_by_key(|line| line.seq);
		let mut misplaced = Vec::new();
		for mut line in lines {
			line.layout(cols, ascii_only, rng);
			let row_ok = rain_rows >= 3 && line.row >= 1 && line.row <= rain_rows - 2;
			let fits = line.col >= 1 && line.col as usize + line.chars.len() < cols as usize;
			let row_free = !self.lines.iter().any(|other| other.row == line.row);
			if row_ok && fits && row_free {
				self.lines.push(line);
			} else {
				misplaced.push(line);
			}
		}
		for line in misplaced {
			self.place(line, now, cols, rain_rows, ascii_only, rng);
		}
		for line in self.pending.iter_mut() {
			line.chars.clear();
		}
	}

	/// Rows that can hold a caught line: all but the first and last.
	fn usable_rows(rain_rows: u16) -> std::ops::Range<u16> {
		1..rain_rows.saturating_sub(1).max(1)
	}

	fn free_rows(&self, rain_rows: u16) -> Vec<u16> {
		Self::usable_rows(rain_rows)
			.filter(|row| !self.lines.iter().any(|line| line.row == *row))
			.collect()
	}

	fn place_pending(
		&mut self,
		now: Duration,
		cols: u16,
		rain_rows: u16,
		ascii_only: bool,
		rng: &mut Rng,
	) {
		while !self.pending.is_empty() {
			let free = self.free_rows(rain_rows);
			if free.is_empty() {
				break;
			}
			let row = free[rng.usize(..free.len())];
			if let Some(line) = self.pending.pop_front() {
				self.put_on_row(line, row, now, cols, ascii_only, rng);
			}
		}
	}

	/// Places a line on a free row, evicting the oldest line that is not
	/// waiting for acknowledgement if needed, or queues it as pending.
	fn place(
		&mut self,
		line: CaughtLine,
		now: Duration,
		cols: u16,
		rain_rows: u16,
		ascii_only: bool,
		rng: &mut Rng,
	) {
		let free = self.free_rows(rain_rows);
		let row = if !free.is_empty() {
			Some(free[rng.usize(..free.len())])
		} else {
			let evict_index = self
				.lines
				.iter()
				.enumerate()
				.filter(|(_, other)| !other.waiting())
				.min_by_key(|(_, other)| other.seq)
				.map(|(index, _)| index);
			evict_index.map(|index| self.lines.remove(index).row)
		};
		match row {
			Some(row) => self.put_on_row(line, row, now, cols, ascii_only, rng),
			None => {
				if self.pending.len() >= MAX_PENDING {
					self.pending.pop_front();
				}
				self.pending.push_back(line);
			}
		}
	}

	fn put_on_row(
		&mut self,
		mut line: CaughtLine,
		row: u16,
		now: Duration,
		cols: u16,
		ascii_only: bool,
		rng: &mut Rng,
	) {
		line.layout(cols, ascii_only, rng);
		let len = line.chars.len() as u16;
		// Keep two cells of margin on each side.
		let min_col = 2u16;
		let max_col = cols.saturating_sub(2).saturating_sub(len).max(min_col);
		line.col = if max_col > min_col {
			rng.u16(min_col..=max_col)
		} else {
			min_col.min(cols.saturating_sub(len + 1)).max(1)
		};
		line.row = row;
		if line.born.is_none() {
			line.born = Some(now);
			if let Hold::For(duration) = line.hold {
				line.dissolve_start = Some(now + duration);
			}
		}
		self.lines.push(line);
	}
}
