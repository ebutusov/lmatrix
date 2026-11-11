//! Cell buffer and diff renderer.

use std::io::{self, Write};

use super::color::TermColor;

/// One screen cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
	pub ch: char,
	pub fg: TermColor,
	pub bg: TermColor,
}

impl Cell {
	pub const BLANK: Cell = Cell {
		ch: ' ',
		fg: TermColor::Default,
		bg: TermColor::Default,
	};
}

/// A full screen of cells.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
	cols: u16,
	rows: u16,
	cells: Vec<Cell>,
}

impl Frame {
	pub fn new(cols: u16, rows: u16) -> Self {
		Self {
			cols,
			rows,
			cells: vec![Cell::BLANK; cols as usize * rows as usize],
		}
	}

	pub fn cols(&self) -> u16 {
		self.cols
	}

	pub fn rows(&self) -> u16 {
		self.rows
	}

	/// Resets every cell to blank.
	pub fn clear(&mut self) {
		self.cells.fill(Cell::BLANK);
	}

	pub fn get(&self, col: u16, row: u16) -> Option<&Cell> {
		if col < self.cols && row < self.rows {
			self.cells
				.get(row as usize * self.cols as usize + col as usize)
		} else {
			None
		}
	}

	/// Sets one cell; out-of-range positions are ignored.
	pub fn set(&mut self, col: u16, row: u16, cell: Cell) {
		if col < self.cols && row < self.rows {
			self.cells[row as usize * self.cols as usize + col as usize] = cell;
		}
	}

	/// Writes `text` starting at (`col`, `row`), clipped at the right edge.
	/// Returns the column after the last written character.
	pub fn put_str(&mut self, col: u16, row: u16, text: &str, fg: TermColor, bg: TermColor) -> u16 {
		let mut current_col = col;
		for ch in text.chars() {
			if current_col >= self.cols {
				break;
			}
			self.set(current_col, row, Cell { ch, fg, bg });
			current_col += 1;
		}
		current_col
	}

	/// Fills one row from `col` to the right edge with `cell`.
	pub fn fill_row(&mut self, row: u16, from_col: u16, cell: Cell) {
		for col in from_col..self.cols {
			self.set(col, row, cell);
		}
	}
}

/// Writes frames to a terminal, sending only the cells that changed.
#[derive(Debug, Default)]
pub struct Renderer {
	previous: Option<Frame>,
	output: Vec<u8>,
}

impl Renderer {
	pub fn new() -> Self {
		Self::default()
	}

	/// Forces the next render to redraw every cell.
	pub fn invalidate(&mut self) {
		self.previous = None;
	}

	/// Renders `frame`, writing the difference to the previous frame to `out`
	/// in one write, wrapped in synchronized-output sequences. Writes nothing
	/// when no cell changed.
	pub fn render<W: Write>(&mut self, frame: &Frame, out: &mut W) -> io::Result<()> {
		self.output.clear();
		let full_redraw = match &self.previous {
			Some(previous) => previous.cols != frame.cols || previous.rows != frame.rows,
			None => true,
		};

		let mut cursor: Option<(u16, u16)> = None;
		let mut current_fg: Option<TermColor> = None;
		let mut current_bg: Option<TermColor> = None;
		let mut wrote_any = false;

		for row in 0..frame.rows {
			for col in 0..frame.cols {
				let index = row as usize * frame.cols as usize + col as usize;
				let cell = frame.cells[index];
				if !full_redraw
					&& let Some(previous) = &self.previous
					&& previous.cells[index] == cell
				{
					continue;
				}
				if !wrote_any {
					self.output.extend_from_slice(b"\x1b[?2026h");
					if full_redraw {
						self.output.extend_from_slice(b"\x1b[0m\x1b[2J");
					}
					wrote_any = true;
				}
				if cursor != Some((col, row)) {
					write!(self.output, "\x1b[{};{}H", row + 1, col + 1)?;
				}
				if current_fg != Some(cell.fg) || current_bg != Some(cell.bg) {
					write_sgr(&mut self.output, cell, current_fg, current_bg)?;
					current_fg = Some(cell.fg);
					current_bg = Some(cell.bg);
				}
				let mut utf8 = [0u8; 4];
				self.output
					.extend_from_slice(cell.ch.encode_utf8(&mut utf8).as_bytes());
				cursor = Some((col + 1, row));
			}
		}

		if wrote_any {
			self.output.extend_from_slice(b"\x1b[0m\x1b[?2026l");
			out.write_all(&self.output)?;
			out.flush()?;
		}

		match &mut self.previous {
			Some(previous) if previous.cols == frame.cols && previous.rows == frame.rows => {
				previous.cells.copy_from_slice(&frame.cells);
			}
			_ => self.previous = Some(frame.clone()),
		}
		Ok(())
	}
}

fn write_sgr(
	output: &mut Vec<u8>,
	cell: Cell,
	current_fg: Option<TermColor>,
	current_bg: Option<TermColor>,
) -> io::Result<()> {
	output.extend_from_slice(b"\x1b[");
	let mut first = true;
	if current_fg != Some(cell.fg) {
		write_color(output, cell.fg, true)?;
		first = false;
	}
	if current_bg != Some(cell.bg) {
		if !first {
			output.push(b';');
		}
		write_color(output, cell.bg, false)?;
	}
	output.push(b'm');
	Ok(())
}

fn write_color(output: &mut Vec<u8>, color: TermColor, foreground: bool) -> io::Result<()> {
	match color {
		TermColor::Default => write!(output, "{}", if foreground { 39 } else { 49 }),
		TermColor::Rgb(red, green, blue) => {
			write!(
				output,
				"{};2;{red};{green};{blue}",
				if foreground { 38 } else { 48 }
			)
		}
		TermColor::Indexed(index) => {
			write!(output, "{};5;{index}", if foreground { 38 } else { 48 })
		}
		TermColor::Basic(index) => {
			let code = match (index < 8, foreground) {
				(true, true) => 30 + index as u16,
				(false, true) => 90 + (index as u16 - 8),
				(true, false) => 40 + index as u16,
				(false, false) => 100 + (index as u16 - 8),
			};
			write!(output, "{code}")
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn green() -> TermColor {
		TermColor::Rgb(0, 255, 0)
	}

	#[test]
	fn unchanged_frame_writes_nothing() {
		let mut frame = Frame::new(10, 3);
		frame.put_str(0, 0, "hello", green(), TermColor::Default);
		let mut renderer = Renderer::new();
		let mut first = Vec::new();
		renderer.render(&frame, &mut first).unwrap();
		assert!(!first.is_empty());
		let mut second = Vec::new();
		renderer.render(&frame, &mut second).unwrap();
		assert!(second.is_empty());
	}

	#[test]
	fn one_cell_change_writes_one_cell() {
		let mut frame = Frame::new(10, 3);
		let mut renderer = Renderer::new();
		renderer.render(&frame, &mut Vec::new()).unwrap();
		frame.set(
			4,
			2,
			Cell {
				ch: 'X',
				fg: green(),
				bg: TermColor::Default,
			},
		);
		let mut output = Vec::new();
		renderer.render(&frame, &mut output).unwrap();
		let text = String::from_utf8(output).unwrap();
		assert_eq!(
			text,
			"\x1b[?2026h\x1b[3;5H\x1b[38;2;0;255;0;49mX\x1b[0m\x1b[?2026l"
		);
	}

	#[test]
	fn contiguous_cells_skip_cursor_moves_and_repeated_colors() {
		let mut frame = Frame::new(10, 1);
		let mut renderer = Renderer::new();
		renderer.render(&frame, &mut Vec::new()).unwrap();
		frame.put_str(2, 0, "abc", TermColor::Basic(10), TermColor::Basic(0));
		let mut output = Vec::new();
		renderer.render(&frame, &mut output).unwrap();
		let text = String::from_utf8(output).unwrap();
		assert_eq!(text, "\x1b[?2026h\x1b[1;3H\x1b[92;40mabc\x1b[0m\x1b[?2026l");
	}

	#[test]
	fn invalidate_forces_full_redraw() {
		let frame = Frame::new(4, 2);
		let mut renderer = Renderer::new();
		renderer.render(&frame, &mut Vec::new()).unwrap();
		renderer.invalidate();
		let mut output = Vec::new();
		renderer.render(&frame, &mut output).unwrap();
		assert!(String::from_utf8(output).unwrap().contains("\x1b[2J"));
	}
}
