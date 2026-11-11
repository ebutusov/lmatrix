//! RGB colors and quantizing to the terminal's color mode.

use std::fmt;
use std::str::FromStr;

/// A 24-bit color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
	pub const WHITE: Rgb = Rgb(255, 255, 255);

	/// Multiplies every channel by `factor` (0..1).
	pub fn scale(self, factor: f32) -> Rgb {
		let factor = factor.clamp(0.0, 1.0);
		let scale_channel = |channel: u8| (channel as f32 * factor).round() as u8;
		Rgb(
			scale_channel(self.0),
			scale_channel(self.1),
			scale_channel(self.2),
		)
	}

	fn distance_sq(self, other: Rgb) -> u32 {
		let delta_red = self.0 as i32 - other.0 as i32;
		let delta_green = self.1 as i32 - other.1 as i32;
		let delta_blue = self.2 as i32 - other.2 as i32;
		(delta_red * delta_red + delta_green * delta_green + delta_blue * delta_blue) as u32
	}
}

impl FromStr for Rgb {
	type Err = String;

	/// Parses `#rrggbb`.
	fn from_str(text: &str) -> Result<Self, Self::Err> {
		let invalid = || format!("invalid color '{text}', expected #rrggbb");
		let hex = text.strip_prefix('#').ok_or_else(invalid)?;
		if hex.len() != 6 || !hex.chars().all(|digit| digit.is_ascii_hexdigit()) {
			return Err(invalid());
		}
		let channel = |range: std::ops::Range<usize>| {
			u8::from_str_radix(&hex[range], 16).map_err(|_| invalid())
		};
		Ok(Rgb(channel(0..2)?, channel(2..4)?, channel(4..6)?))
	}
}

impl fmt::Display for Rgb {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(formatter, "#{:02x}{:02x}{:02x}", self.0, self.1, self.2)
	}
}

/// How many colors the terminal can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
	TrueColor,
	Palette256,
	Basic16,
}

/// A color as it is sent to the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TermColor {
	/// The terminal's default foreground or background.
	Default,
	Rgb(u8, u8, u8),
	/// An entry of the 256-color palette.
	Indexed(u8),
	/// One of the 16 basic colors (0..=15, 8..=15 being the bright variants).
	Basic(u8),
}

/// The 16 basic colors, as xterm draws them.
const BASIC_PALETTE: [Rgb; 16] = [
	Rgb(0, 0, 0),
	Rgb(205, 0, 0),
	Rgb(0, 205, 0),
	Rgb(205, 205, 0),
	Rgb(0, 0, 238),
	Rgb(205, 0, 205),
	Rgb(0, 205, 205),
	Rgb(229, 229, 229),
	Rgb(127, 127, 127),
	Rgb(255, 0, 0),
	Rgb(0, 255, 0),
	Rgb(255, 255, 0),
	Rgb(92, 92, 255),
	Rgb(255, 0, 255),
	Rgb(0, 255, 255),
	Rgb(255, 255, 255),
];

const BRIGHT_WHITE: u8 = 15;

const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

impl ColorMode {
	/// Converts a fixed color (text, backgrounds) for this mode.
	pub fn solid(self, color: Rgb) -> TermColor {
		match self {
			ColorMode::TrueColor => TermColor::Rgb(color.0, color.1, color.2),
			ColorMode::Palette256 => TermColor::Indexed(nearest_256(color)),
			ColorMode::Basic16 => TermColor::Basic(nearest_basic(color)),
		}
	}

	/// Converts a rain color at a brightness level (0..1) for this mode.
	///
	/// `head` selects the near-white head color.
	pub fn shade(self, color: Rgb, level: f32, head: bool, head_color: Rgb) -> TermColor {
		match self {
			ColorMode::Basic16 => {
				if head {
					TermColor::Basic(BRIGHT_WHITE)
				} else {
					let hue = basic_hue(color);
					if level > 0.5 {
						TermColor::Basic(hue + 8)
					} else {
						TermColor::Basic(hue)
					}
				}
			}
			_ if head => self.solid(head_color),
			ColorMode::Palette256 => TermColor::Indexed(shade_256(color, level)),
			ColorMode::TrueColor => self.solid(color.scale(level)),
		}
	}
}

/// Index of the nearest cube level (0..6) for one channel.
fn nearest_cube_level(channel: u8) -> usize {
	CUBE_LEVELS
		.iter()
		.enumerate()
		.min_by_key(|(_, level)| (**level as i32 - channel as i32).abs())
		.map(|(index, _)| index)
		.unwrap_or(0)
}

/// Shades a rain color for the 256-color palette.
///
/// The cube has no level between 0 and 95, so plain quantizing turns every
/// faint cell black and cuts trails short. Colored (non-gray) rain stays in
/// the cube and never goes below the darkest cube color of its hue, so a
/// trail fades to dark green (for the default color) instead of ending early.
fn shade_256(color: Rgb, level: f32) -> u8 {
	let scaled = color.scale(level);
	let max_channel = color.0.max(color.1).max(color.2);
	let min_channel = color.0.min(color.1).min(color.2);
	let grayish = max_channel == 0 || (max_channel - min_channel) < max_channel / 4;
	if grayish {
		return nearest_256(scaled);
	}
	let mut indices = [
		nearest_cube_level(scaled.0),
		nearest_cube_level(scaled.1),
		nearest_cube_level(scaled.2),
	];
	if indices == [0, 0, 0] {
		// Darkest color of the same hue: level 1 on the dominant channels.
		let base = [color.0, color.1, color.2];
		for (index, channel) in base.iter().enumerate() {
			indices[index] = usize::from(*channel as u32 * 2 >= max_channel as u32);
		}
	}
	(16 + 36 * indices[0] + 6 * indices[1] + indices[2]) as u8
}

/// Index of the nearest color in the 256-color palette (cube or gray ramp).
pub fn nearest_256(color: Rgb) -> u8 {
	let nearest_level = |channel: u8| -> usize {
		CUBE_LEVELS
			.iter()
			.enumerate()
			.min_by_key(|(_, level)| (**level as i32 - channel as i32).abs())
			.map(|(index, _)| index)
			.unwrap_or(0)
	};
	let (red_index, green_index, blue_index) = (
		nearest_level(color.0),
		nearest_level(color.1),
		nearest_level(color.2),
	);
	let cube_color = Rgb(
		CUBE_LEVELS[red_index],
		CUBE_LEVELS[green_index],
		CUBE_LEVELS[blue_index],
	);
	let cube_index = 16 + 36 * red_index + 6 * green_index + blue_index;

	let average = (color.0 as u32 + color.1 as u32 + color.2 as u32) / 3;
	let gray_step = if average < 8 {
		0
	} else {
		((average - 8) / 10).min(23)
	};
	let gray_candidates = [gray_step, (gray_step + 1).min(23)];
	let (gray_index, gray_distance) = gray_candidates
		.iter()
		.map(|step| {
			let value = (8 + 10 * step) as u8;
			(
				232 + *step as usize,
				color.distance_sq(Rgb(value, value, value)),
			)
		})
		.min_by_key(|(_, distance)| *distance)
		.unwrap_or((232, u32::MAX));

	if gray_distance < color.distance_sq(cube_color) {
		gray_index as u8
	} else {
		cube_index as u8
	}
}

/// Index of the nearest of the 16 basic colors.
pub fn nearest_basic(color: Rgb) -> u8 {
	BASIC_PALETTE
		.iter()
		.enumerate()
		.min_by_key(|(_, entry)| color.distance_sq(**entry))
		.map(|(index, _)| index as u8)
		.unwrap_or(7)
}

/// The basic hue (1..=7, never black) closest to `color` at full brightness.
fn basic_hue(color: Rgb) -> u8 {
	let max_channel = color.0.max(color.1).max(color.2);
	if max_channel == 0 {
		return 7;
	}
	let normalize = |channel: u8| (channel as u32 * 255 / max_channel as u32) as u8;
	let normalized = Rgb(normalize(color.0), normalize(color.1), normalize(color.2));
	(1..8u8)
		.min_by_key(|index| normalized.distance_sq(BASIC_PALETTE[*index as usize + 8]))
		.unwrap_or(7)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_hex() {
		assert_eq!("#28ff6e".parse::<Rgb>(), Ok(Rgb(0x28, 0xff, 0x6e)));
		assert_eq!("#FFFFFF".parse::<Rgb>(), Ok(Rgb::WHITE));
		assert!("28ff6e".parse::<Rgb>().is_err());
		assert!("#28ff6".parse::<Rgb>().is_err());
		assert!("#28ff6g".parse::<Rgb>().is_err());
	}

	#[test]
	fn quantizes_to_256() {
		assert_eq!(nearest_256(Rgb(0, 0, 0)), 16);
		assert_eq!(nearest_256(Rgb(255, 255, 255)), 231);
		assert_eq!(nearest_256(Rgb(255, 0, 0)), 196);
		assert_eq!(nearest_256(Rgb(0, 255, 0)), 46);
		// #28ff6e: red 40 -> level 0, green 255 -> 5, blue 110 -> 95 (1).
		assert_eq!(nearest_256(Rgb(0x28, 0xff, 0x6e)), 16 + 30 + 1);
		// Mid gray lands on the gray ramp.
		assert_eq!(nearest_256(Rgb(128, 128, 128)), 244);
		assert_eq!(nearest_256(Rgb(8, 8, 8)), 232);
	}

	#[test]
	fn quantizes_to_16() {
		assert_eq!(nearest_basic(Rgb(0, 0, 0)), 0);
		assert_eq!(nearest_basic(Rgb(250, 250, 250)), 15);
		assert_eq!(nearest_basic(Rgb(0x10, 0xf0, 0x20)), 10);
		assert_eq!(nearest_basic(Rgb(0x0f, 0x33, 0x19)), 0);
		let green = Rgb(0x28, 0xff, 0x6e);
		let mode = ColorMode::Basic16;
		assert_eq!(
			mode.shade(green, 1.0, true, Rgb::WHITE),
			TermColor::Basic(15)
		);
		assert_eq!(
			mode.shade(green, 0.8, false, Rgb::WHITE),
			TermColor::Basic(10)
		);
		assert_eq!(
			mode.shade(green, 0.3, false, Rgb::WHITE),
			TermColor::Basic(2)
		);
		let red = Rgb(0xff, 0x46, 0x3c);
		assert_eq!(mode.shade(red, 0.8, false, Rgb::WHITE), TermColor::Basic(9));
		assert_eq!(mode.shade(red, 0.2, false, Rgb::WHITE), TermColor::Basic(1));
	}

	#[test]
	fn faint_256_rain_keeps_its_hue() {
		let green = Rgb(0x28, 0xff, 0x6e);
		let mode = ColorMode::Palette256;
		// Bright cells quantize as before.
		assert_eq!(
			mode.shade(green, 1.0, false, Rgb::WHITE),
			TermColor::Indexed(47)
		);
		// Faint cells used to turn black (16); now they stay dark green (22).
		for level in [0.18, 0.1, 0.06] {
			assert_eq!(
				mode.shade(green, level, false, Rgb::WHITE),
				TermColor::Indexed(22),
				"level {level}"
			);
		}
		// Red rain fades to dark red, amber to dark olive.
		assert_eq!(
			mode.shade(Rgb(0xff, 0x46, 0x3c), 0.1, false, Rgb::WHITE),
			TermColor::Indexed(52)
		);
		assert_eq!(
			mode.shade(Rgb(0xff, 0xb4, 0x28), 0.1, false, Rgb::WHITE),
			TermColor::Indexed(58)
		);
		// Gray rain still uses the gray ramp.
		assert_eq!(
			mode.shade(Rgb(200, 200, 200), 0.1, false, Rgb::WHITE),
			TermColor::Indexed(nearest_256(Rgb(20, 20, 20)))
		);
	}

	#[test]
	fn truecolor_scales() {
		let mode = ColorMode::TrueColor;
		assert_eq!(
			mode.shade(Rgb(200, 100, 0), 0.5, false, Rgb::WHITE),
			TermColor::Rgb(100, 50, 0)
		);
		assert_eq!(
			mode.shade(Rgb(200, 100, 0), 1.0, true, Rgb(1, 2, 3)),
			TermColor::Rgb(1, 2, 3)
		);
	}
}
