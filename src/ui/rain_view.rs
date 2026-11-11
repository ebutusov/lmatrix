//! Rain mode: help bar, rain, caught lines, backlog window.

use fastrand::Rng;

use super::Palette;
use crate::sim::{CharView, Sim, random_glyph};
use crate::term::{Cell, Frame, TermColor};

/// Brightness below which a rain cell is not drawn.
const MIN_VISIBLE_LEVEL: f32 = 0.06;
/// Brightness above which a rain cell is drawn in the head color.
const HEAD_LEVEL: f32 = 0.95;

pub const RAIN_HELP: &str = "[enter] tail  [space] freeze  [a] ack  [A] ack all  [q] quit";

/// Draws a help bar on `row`: `left` text, and `right` text in its own
/// color aligned to the right edge.
pub fn draw_help_bar(
	frame: &mut Frame,
	row: u16,
	left: &str,
	right: Option<(&str, TermColor)>,
	palette: &Palette,
) {
	let bar = Cell {
		ch: ' ',
		fg: palette.help_fg,
		bg: palette.help_bg,
	};
	frame.fill_row(row, 0, bar);
	let cols = frame.cols();
	let right_len = right
		.map(|(text, _)| text.chars().count() as u16)
		.unwrap_or(0);
	let left_room = cols.saturating_sub(if right_len > 0 { right_len + 3 } else { 1 });
	let left_text: String = left
		.chars()
		.take(left_room.saturating_sub(1) as usize)
		.collect();
	frame.put_str(1, row, &left_text, palette.help_fg, palette.help_bg);
	if let Some((text, color)) = right
		&& right_len + 2 <= cols
	{
		frame.put_str(cols - right_len - 1, row, text, color, palette.help_bg);
	}
}

/// Draws the rain grid, starting at screen row `top`.
pub fn draw_rain(frame: &mut Frame, sim: &Sim, top: u16, palette: &Palette) {
	let rain = sim.rain();
	for row in 0..rain.rows() {
		for col in 0..rain.cols() {
			let cell = rain.cell(col, row);
			if cell.level < MIN_VISIBLE_LEVEL {
				continue;
			}
			let head = cell.level > HEAD_LEVEL;
			let fg = palette.mode.shade(
				cell.color,
				palette.rain_level(cell.level),
				head,
				palette.head_rgb,
			);
			frame.set(
				col,
				top + row,
				Cell {
					ch: cell.ch,
					fg,
					bg: TermColor::Default,
				},
			);
		}
	}
}

/// Draws the caught lines over the rain, starting at screen row `top`.
pub fn draw_caught(frame: &mut Frame, sim: &Sim, top: u16, palette: &Palette, rng: &mut Rng) {
	let now = sim.now();
	// Blink the ack marker: 0.5 s on, 0.5 s off.
	let marker_on = now.as_millis() % 1000 < 500;
	for caught in sim.caught_lines() {
		let row = top + caught.row;
		let color = palette.mode.solid(caught.color);
		let marker = caught.marker_range();
		// Clear the text extent plus one padding cell on each side.
		let first = caught.col.saturating_sub(1);
		let last = caught.col + caught.chars.len() as u16;
		for col in first..=last {
			frame.set(col, row, Cell::BLANK);
		}
		for (index, ch) in caught.chars.iter().enumerate() {
			let col = caught.col + index as u16;
			let in_marker = marker.as_ref().is_some_and(|range| range.contains(&index));
			let cell = match caught.char_view(index, now) {
				CharView::Blank => continue,
				CharView::Glyph => Cell {
					ch: random_glyph(rng),
					fg: palette.glyph_dim,
					bg: TermColor::Default,
				},
				CharView::Revealed if in_marker && caught.waiting() => {
					if !marker_on {
						continue;
					}
					Cell {
						ch: *ch,
						fg: palette.marker,
						bg: TermColor::Default,
					}
				}
				CharView::Revealed => Cell {
					ch: *ch,
					fg: color,
					bg: TermColor::Default,
				},
			};
			frame.set(col, row, cell);
		}
	}
}
