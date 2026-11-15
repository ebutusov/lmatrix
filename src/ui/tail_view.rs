//! Tail mode: the scrollable backlog with an optional filter.

use std::collections::VecDeque;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use regex::Regex;

use super::Palette;
use super::rain_view::draw_help_bar;
use crate::backlog::{Backlog, Entry};
use crate::line::truncate_with_ellipsis;
use crate::rules::RuleSet;
use crate::term::{Cell, Frame, TermColor};

pub const TAIL_HELP: &str = "TAIL  [enter] rain  [arrows] scroll  [/] filter  [q] quit";

/// Columns the message moves per Left / Right key.
const HSCROLL_STEP: usize = 8;

/// Length of the `HH:MM:SS ` prefix of a row.
const TIMESTAMP_LEN: usize = 9;

/// Cells available for the message after the margins and the timestamp.
fn message_width(cols: u16) -> usize {
	(cols as usize).saturating_sub(2 + TIMESTAMP_LEN)
}

/// What the main loop should do after a key in tail mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailAction {
	None,
	/// Switch back to rain mode.
	Leave,
}

/// Colors of one backlog row.
pub struct RowStyle {
	pub timestamp: TermColor,
	pub message: TermColor,
	pub background: TermColor,
}

/// Draws one backlog entry as `HH:MM:SS text` on `row`, truncated to fit.
/// The message uses its rule's color if it matched one. `hscroll` skips that
/// many characters of the message (the timestamp stays put); an ellipsis
/// marks text hidden on either side.
#[allow(clippy::too_many_arguments)]
pub fn draw_entry(
	frame: &mut Frame,
	row: u16,
	entry: &Entry,
	rules: &RuleSet,
	style: &RowStyle,
	palette: &Palette,
	ascii_only: bool,
	hscroll: usize,
) {
	let cols = frame.cols();
	frame.fill_row(
		row,
		0,
		Cell {
			ch: ' ',
			fg: style.message,
			bg: style.background,
		},
	);
	// One cell of margin on each side.
	let width = (cols as usize).saturating_sub(2);
	let timestamp = entry.line.ts.format("%H:%M:%S ").to_string();
	let msg_width = message_width(cols);
	let shown = if hscroll == 0 || msg_width == 0 {
		let full = format!("{timestamp}{}", entry.line.text);
		truncate_with_ellipsis(&full, width, ascii_only)
	} else {
		let rest: String = entry.line.text.chars().skip(hscroll).collect();
		let mut message: Vec<char> = truncate_with_ellipsis(&rest, msg_width, ascii_only)
			.chars()
			.collect();
		let ellipsis = if ascii_only { "..." } else { "…" };
		for (slot, ch) in message.iter_mut().zip(ellipsis.chars()) {
			*slot = ch;
		}
		let message: String = message.into_iter().collect();
		format!("{timestamp}{message}")
	};
	let timestamp_len = timestamp.chars().count().min(shown.chars().count());
	let message_color = entry
		.rule
		.and_then(|id| rules.get(id))
		.map(|rule| palette.mode.solid(rule.color))
		.unwrap_or(style.message);
	for (index, ch) in shown.chars().enumerate() {
		let col = 1 + index as u16;
		let fg = if index < timestamp_len {
			style.timestamp
		} else {
			message_color
		};
		frame.set(
			col,
			row,
			Cell {
				ch,
				fg,
				bg: style.background,
			},
		);
	}
}

/// State of the tail view.
#[derive(Debug, Default)]
pub struct TailView {
	/// Show the newest lines and follow new ones.
	follow: bool,
	/// Sequence number of the top visible line when not following.
	top_seq: u64,
	filter: Option<Regex>,
	/// Sequence numbers of entries matching the filter, oldest first.
	matches: VecDeque<u64>,
	/// Entries below this sequence number have been tested for the filter.
	scanned_to: u64,
	/// Filter text being typed, if the filter prompt is open.
	editing: Option<String>,
	/// Error shown on the prompt for an invalid filter.
	error: Option<String>,
	/// Characters of each message hidden on the left (Left / Right keys).
	hscroll: usize,
}

impl TailView {
	pub fn new() -> Self {
		Self {
			follow: true,
			..Self::default()
		}
	}

	/// Whether the filter prompt is open (keys go to the prompt).
	pub fn is_editing(&self) -> bool {
		self.editing.is_some()
	}

	pub fn filter_text(&self) -> Option<&str> {
		self.filter.as_ref().map(Regex::as_str)
	}

	/// Starts following again from the left edge, e.g. when the view is
	/// entered.
	pub fn follow(&mut self) {
		self.follow = true;
		self.hscroll = 0;
	}

	/// Largest useful horizontal offset: the one that shows the end of the
	/// longest visible message.
	fn max_hscroll(&self, backlog: &Backlog, visible_rows: usize, cols: u16) -> usize {
		let top = self.top_position(backlog, visible_rows);
		let longest = (top..top + visible_rows)
			.map_while(|position| self.seq_at(position, backlog))
			.filter_map(|seq| backlog.get_seq(seq))
			.map(|entry| entry.line.text.chars().count())
			.max()
			.unwrap_or(0);
		longest.saturating_sub(message_width(cols))
	}

	/// Horizontal offset used for drawing, clamped to the visible lines.
	fn effective_hscroll(&self, backlog: &Backlog, visible_rows: usize, cols: u16) -> usize {
		self.hscroll
			.min(self.max_hscroll(backlog, visible_rows, cols))
	}

	/// Tests new backlog entries against the filter and forgets discarded ones.
	pub fn update(&mut self, backlog: &Backlog) {
		let Some(filter) = &self.filter else {
			return;
		};
		let first_seq = backlog.first_seq();
		while self.matches.front().is_some_and(|seq| *seq < first_seq) {
			self.matches.pop_front();
		}
		for seq in self.scanned_to.max(first_seq)..backlog.end_seq() {
			if let Some(entry) = backlog.get_seq(seq)
				&& filter.is_match(&entry.line.text)
			{
				self.matches.push_back(seq);
			}
		}
		self.scanned_to = backlog.end_seq();
	}

	fn set_filter(&mut self, filter: Option<Regex>, backlog: &Backlog) {
		self.filter = filter;
		self.matches.clear();
		self.scanned_to = 0;
		self.follow = true;
		self.update(backlog);
	}

	/// Number of lines in the view (all, or matching the filter).
	fn len(&self, backlog: &Backlog) -> usize {
		if self.filter.is_some() {
			self.matches.len()
		} else {
			backlog.len()
		}
	}

	fn seq_at(&self, position: usize, backlog: &Backlog) -> Option<u64> {
		if self.filter.is_some() {
			self.matches.get(position).copied()
		} else if position < backlog.len() {
			Some(backlog.first_seq() + position as u64)
		} else {
			None
		}
	}

	/// Position in the view of the first line at or after `seq`.
	fn position_of(&self, seq: u64, backlog: &Backlog) -> usize {
		if self.filter.is_some() {
			self.matches.partition_point(|candidate| *candidate < seq)
		} else {
			(seq.saturating_sub(backlog.first_seq()) as usize).min(backlog.len())
		}
	}

	/// Position of the top visible line.
	fn top_position(&self, backlog: &Backlog, visible_rows: usize) -> usize {
		let max_top = self.len(backlog).saturating_sub(visible_rows);
		if self.follow {
			max_top
		} else {
			self.position_of(self.top_seq, backlog).min(max_top)
		}
	}

	fn scroll_to(&mut self, position: usize, backlog: &Backlog) {
		self.follow = false;
		self.top_seq = self.seq_at(position, backlog).unwrap_or(backlog.end_seq());
	}

	/// Handles a key. Quit keys are handled by the caller.
	pub fn handle_key(
		&mut self,
		key: KeyEvent,
		backlog: &Backlog,
		visible_rows: usize,
		cols: u16,
	) -> TailAction {
		if let Some(input) = &mut self.editing {
			match key.code {
				KeyCode::Enter => {
					if input.is_empty() {
						self.editing = None;
						self.error = None;
						self.set_filter(None, backlog);
					} else {
						match Regex::new(input) {
							Ok(regex) => {
								self.editing = None;
								self.error = None;
								self.set_filter(Some(regex), backlog);
							}
							Err(error) => {
								let message = error.to_string();
								let last_line =
									message.lines().last().unwrap_or("invalid regex").trim();
								let short = last_line.strip_prefix("error: ").unwrap_or(last_line);
								self.error = Some(short.to_string());
							}
						}
					}
				}
				KeyCode::Esc => {
					self.editing = None;
					self.error = None;
				}
				KeyCode::Backspace => {
					input.pop();
					self.error = None;
				}
				KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
					input.push(ch);
					self.error = None;
				}
				_ => {}
			}
			return TailAction::None;
		}

		let length = self.len(backlog);
		let page = visible_rows.max(1);
		let top = self.top_position(backlog, visible_rows);
		let max_top = length.saturating_sub(visible_rows);
		match key.code {
			KeyCode::Enter => return TailAction::Leave,
			KeyCode::Char('/') => {
				self.editing = Some(self.filter_text().unwrap_or("").to_string());
				self.error = None;
			}
			KeyCode::Esc => {
				if self.filter.is_some() {
					self.set_filter(None, backlog);
				}
			}
			KeyCode::Up => self.scroll_to(top.saturating_sub(1), backlog),
			KeyCode::Down => self.scroll_to((top + 1).min(max_top), backlog),
			KeyCode::PageUp => self.scroll_to(top.saturating_sub(page), backlog),
			KeyCode::PageDown => self.scroll_to((top + page).min(max_top), backlog),
			KeyCode::Home => self.scroll_to(0, backlog),
			KeyCode::End | KeyCode::Char('G') => self.follow = true,
			KeyCode::Left => {
				let current = self.effective_hscroll(backlog, visible_rows, cols);
				self.hscroll = current.saturating_sub(HSCROLL_STEP);
			}
			KeyCode::Right => {
				let max = self.max_hscroll(backlog, visible_rows, cols);
				self.hscroll = (self.hscroll.min(max) + HSCROLL_STEP).min(max);
			}
			_ => {}
		}
		TailAction::None
	}

	/// Draws the help bar (or the filter prompt) and the visible lines.
	pub fn draw(
		&self,
		frame: &mut Frame,
		backlog: &Backlog,
		rules: &RuleSet,
		palette: &Palette,
		ascii_only: bool,
	) {
		let rows = frame.rows();
		let visible_rows = rows.saturating_sub(1) as usize;
		let hscroll = self.effective_hscroll(backlog, visible_rows, frame.cols());

		if let Some(input) = &self.editing {
			let prompt = match &self.error {
				Some(error) => format!("filter: /{input}   error: {error}"),
				None => format!("filter: /{input}_   [enter] apply  [esc] cancel"),
			};
			let right = self
				.error
				.as_ref()
				.map(|_| ("invalid regex", palette.error));
			draw_help_bar(frame, 0, &prompt, right, palette);
		} else {
			let left = match self.filter_text() {
				Some(filter) => format!(
					"TAIL  filter: /{filter}/  [esc] clear  [/] edit  [enter] rain  [q] quit"
				),
				None => TAIL_HELP.to_string(),
			};
			let count = if hscroll > 0 {
				format!("col {}  {} lines", hscroll + 1, self.len(backlog))
			} else {
				format!("{} lines", self.len(backlog))
			};
			draw_help_bar(frame, 0, &left, Some((&count, palette.help_fg)), palette);
		}

		let style = RowStyle {
			timestamp: palette.tail_timestamp,
			message: palette.tail_message,
			background: TermColor::Default,
		};
		let top = self.top_position(backlog, visible_rows);
		for offset in 0..visible_rows {
			let Some(seq) = self.seq_at(top + offset, backlog) else {
				break;
			};
			if let Some(entry) = backlog.get_seq(seq) {
				draw_entry(
					frame,
					1 + offset as u16,
					entry,
					rules,
					&style,
					palette,
					ascii_only,
					hscroll,
				);
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::line::LogLine;
	use crate::term::ColorMode;
	use chrono::Local;
	use crossterm::event::KeyEventKind;

	fn backlog_with(count: usize, capacity: usize) -> Backlog {
		let mut backlog = Backlog::new(capacity);
		for index in 0..count {
			backlog.push(Entry {
				line: LogLine::new(
					Local::now(),
					0,
					format!(
						"line {index} {}",
						if index % 2 == 0 { "even" } else { "odd" }
					),
				),
				rule: None,
			});
		}
		backlog
	}

	fn key(code: KeyCode) -> KeyEvent {
		KeyEvent {
			code,
			modifiers: KeyModifiers::NONE,
			kind: KeyEventKind::Press,
			state: crossterm::event::KeyEventState::NONE,
		}
	}

	fn visible_texts(view: &TailView, backlog: &Backlog, rows: u16) -> Vec<String> {
		let mut frame = Frame::new(40, rows);
		view.draw(
			&mut frame,
			backlog,
			&RuleSet::default(),
			&Palette::new(ColorMode::TrueColor),
			false,
		);
		(1..rows)
			.map(|row| {
				(0..40)
					.map(|col| frame.get(col, row).unwrap().ch)
					.collect::<String>()
					.trim_end()
					.to_string()
			})
			.filter(|text| !text.is_empty())
			.map(|text| text[10..].to_string())
			.collect()
	}

	#[test]
	fn follows_newest_lines() {
		let mut backlog = backlog_with(10, 100);
		let view = TailView::new();
		assert_eq!(
			visible_texts(&view, &backlog, 4),
			["line 7 odd", "line 8 even", "line 9 odd"]
		);
		backlog.push(Entry {
			line: LogLine::new(Local::now(), 0, "line 10 new"),
			rule: None,
		});
		assert_eq!(visible_texts(&view, &backlog, 4)[2], "line 10 new");
	}

	#[test]
	fn scrolling_stops_following_and_end_resumes() {
		let mut backlog = backlog_with(10, 100);
		let mut view = TailView::new();
		view.handle_key(key(KeyCode::Up), &backlog, 3, 40);
		assert_eq!(
			visible_texts(&view, &backlog, 4),
			["line 6 even", "line 7 odd", "line 8 even"]
		);
		backlog.push(Entry {
			line: LogLine::new(Local::now(), 0, "line 10"),
			rule: None,
		});
		assert_eq!(visible_texts(&view, &backlog, 4)[0], "line 6 even");
		view.handle_key(key(KeyCode::Home), &backlog, 3, 40);
		assert_eq!(visible_texts(&view, &backlog, 4)[0], "line 0 even");
		view.handle_key(key(KeyCode::PageDown), &backlog, 3, 40);
		assert_eq!(visible_texts(&view, &backlog, 4)[0], "line 3 odd");
		view.handle_key(key(KeyCode::Char('G')), &backlog, 3, 40);
		assert_eq!(visible_texts(&view, &backlog, 4)[2], "line 10");
	}

	#[test]
	fn filter_applies_and_clears() {
		let mut backlog = backlog_with(10, 100);
		let mut view = TailView::new();
		view.handle_key(key(KeyCode::Char('/')), &backlog, 3, 40);
		assert!(view.is_editing());
		for ch in "od+".chars() {
			view.handle_key(key(KeyCode::Char(ch)), &backlog, 3, 40);
		}
		view.handle_key(key(KeyCode::Enter), &backlog, 3, 40);
		assert!(!view.is_editing());
		assert_eq!(view.filter_text(), Some("od+"));
		assert_eq!(
			visible_texts(&view, &backlog, 4),
			["line 5 odd", "line 7 odd", "line 9 odd"]
		);
		backlog.push(Entry {
			line: LogLine::new(Local::now(), 0, "line 10 odd"),
			rule: None,
		});
		backlog.push(Entry {
			line: LogLine::new(Local::now(), 0, "line 11 even"),
			rule: None,
		});
		view.update(&backlog);
		assert_eq!(view.len(&backlog), 6);
		assert_eq!(visible_texts(&view, &backlog, 4)[2], "line 10 odd");
		view.handle_key(key(KeyCode::Esc), &backlog, 3, 40);
		assert_eq!(view.filter_text(), None);
		assert_eq!(view.len(&backlog), 12);
	}

	#[test]
	fn invalid_filter_is_not_applied() {
		let backlog = backlog_with(5, 100);
		let mut view = TailView::new();
		view.handle_key(key(KeyCode::Char('/')), &backlog, 3, 40);
		view.handle_key(key(KeyCode::Char('(')), &backlog, 3, 40);
		view.handle_key(key(KeyCode::Enter), &backlog, 3, 40);
		assert!(view.is_editing());
		assert!(view.error.is_some());
		assert_eq!(view.filter_text(), None);
		view.handle_key(key(KeyCode::Esc), &backlog, 3, 40);
		assert!(!view.is_editing());
	}

	#[test]
	fn filter_survives_discarded_entries() {
		let mut backlog = backlog_with(10, 10);
		let mut view = TailView::new();
		view.set_filter(Some(Regex::new("even").unwrap()), &backlog);
		assert_eq!(view.len(&backlog), 5);
		for index in 10..14 {
			backlog.push(Entry {
				line: LogLine::new(Local::now(), 0, format!("line {index} odd")),
				rule: None,
			});
		}
		view.update(&backlog);
		// line 0 and line 2 were discarded.
		assert_eq!(view.len(&backlog), 3);
	}

	#[test]
	fn left_right_shift_long_lines() {
		let mut backlog = Backlog::new(10);
		let long: String = ('a'..='z').chain('A'..='Z').collect(); // 52 chars
		backlog.push(Entry {
			line: LogLine::new(Local::now(), 0, long.clone()),
			rule: None,
		});
		let mut view = TailView::new();
		// 40 columns leave 29 for the message.
		assert_eq!(
			visible_texts(&view, &backlog, 4),
			[format!("{}…", &long[..28])]
		);
		view.handle_key(key(KeyCode::Right), &backlog, 3, 40);
		assert_eq!(
			visible_texts(&view, &backlog, 4),
			[format!("…{}…", &long[9..36])]
		);
		// Clamped so the end of the longest line touches the right edge.
		for _ in 0..10 {
			view.handle_key(key(KeyCode::Right), &backlog, 3, 40);
		}
		assert_eq!(view.hscroll, 52 - 29);
		assert_eq!(
			visible_texts(&view, &backlog, 4),
			[format!("…{}", &long[24..])]
		);
		view.handle_key(key(KeyCode::Left), &backlog, 3, 40);
		assert_eq!(view.hscroll, 52 - 29 - 8);
		for _ in 0..5 {
			view.handle_key(key(KeyCode::Left), &backlog, 3, 40);
		}
		assert_eq!(view.hscroll, 0);
		assert_eq!(
			visible_texts(&view, &backlog, 4),
			[format!("{}…", &long[..28])]
		);
	}
}
