//! Log lines, source identifiers and input sanitizing.

use chrono::{DateTime, Local};
use unicode_width::UnicodeWidthChar;

/// Index of a source in the configured source list.
pub type SourceId = usize;

/// Maximum number of characters kept from a single line.
pub const MAX_LINE_CHARS: usize = 4096;

/// One sanitized log line.
#[derive(Debug, Clone, PartialEq)]
pub struct LogLine {
	pub ts: DateTime<Local>,
	pub source: SourceId,
	pub text: Box<str>,
}

impl LogLine {
	pub fn new(ts: DateTime<Local>, source: SourceId, text: impl Into<Box<str>>) -> Self {
		Self {
			ts,
			source,
			text: text.into(),
		}
	}
}

/// Message sent from a source thread to the main loop.
#[derive(Debug, Clone)]
pub enum SourceMsg {
	/// A line read during startup preload.
	Preload(LogLine),
	/// The source has finished sending preload lines.
	PreloadDone(SourceId),
	/// A live line.
	Live(LogLine),
}

/// Makes untrusted log text safe to put on a terminal.
///
/// Removes escape sequences and control characters, turns tabs into a single
/// space, replaces characters that are not exactly one column wide (and with
/// `ascii_only` every non-ASCII character) by `?`, and truncates the result
/// to [`MAX_LINE_CHARS`] characters.
pub fn sanitize(raw: &str, ascii_only: bool) -> String {
	let mut output = String::with_capacity(raw.len().min(MAX_LINE_CHARS));
	let mut kept_chars = 0usize;
	let mut chars = raw.chars().peekable();

	while let Some(current) = chars.next() {
		if kept_chars >= MAX_LINE_CHARS {
			break;
		}
		match current {
			'\x1b' => skip_escape(&mut chars),
			// C1 control-sequence introducers: CSI, OSC, DCS, SOS, PM, APC.
			'\u{9b}' => skip_csi_body(&mut chars),
			'\u{9d}' | '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}' => skip_string_body(&mut chars),
			'\t' => {
				output.push(' ');
				kept_chars += 1;
			}
			other if other.is_control() => {}
			other => {
				let replacement = if ascii_only && !other.is_ascii() {
					'?'
				} else if other.width() == Some(1) {
					other
				} else {
					'?'
				};
				output.push(replacement);
				kept_chars += 1;
			}
		}
	}
	output
}

type CharIter<'a> = std::iter::Peekable<std::str::Chars<'a>>;

/// Skips what follows an ESC character.
fn skip_escape(chars: &mut CharIter<'_>) {
	match chars.peek().copied() {
		Some('[') => {
			chars.next();
			skip_csi_body(chars);
		}
		Some(']') | Some('P') | Some('X') | Some('^') | Some('_') => {
			chars.next();
			skip_string_body(chars);
		}
		Some(next) if ('\u{20}'..='\u{2f}').contains(&next) => {
			// nF sequence: intermediates, then one final byte.
			while let Some(&intermediate) = chars.peek() {
				chars.next();
				if !('\u{20}'..='\u{2f}').contains(&intermediate) {
					break;
				}
			}
		}
		Some(next) if ('\u{30}'..='\u{7e}').contains(&next) => {
			// Two-character sequence such as ESC 7 or ESC c.
			chars.next();
		}
		_ => {}
	}
}

/// Skips parameter and intermediate bytes and the final byte of a CSI sequence.
fn skip_csi_body(chars: &mut CharIter<'_>) {
	while let Some(&next) = chars.peek() {
		if ('\u{20}'..='\u{3f}').contains(&next) {
			chars.next();
		} else {
			if ('\u{40}'..='\u{7e}').contains(&next) {
				chars.next();
			}
			break;
		}
	}
}

/// Skips the body of an OSC/DCS/SOS/PM/APC string up to BEL or ST.
fn skip_string_body(chars: &mut CharIter<'_>) {
	while let Some(next) = chars.next() {
		match next {
			'\x07' | '\u{9c}' => break,
			'\x1b' => {
				if chars.peek() == Some(&'\\') {
					chars.next();
				}
				break;
			}
			_ => {}
		}
	}
}

/// Truncates `text` to at most `max_chars` characters, ending with an
/// ellipsis (`…`, or `...` when `ascii_only`) if anything was cut.
pub fn truncate_with_ellipsis(text: &str, max_chars: usize, ascii_only: bool) -> String {
	let char_count = text.chars().count();
	if char_count <= max_chars {
		return text.to_string();
	}
	let ellipsis = if ascii_only { "..." } else { "…" };
	let ellipsis_len = ellipsis.chars().count();
	if max_chars <= ellipsis_len {
		return text.chars().take(max_chars).collect();
	}
	let mut result: String = text.chars().take(max_chars - ellipsis_len).collect();
	result.push_str(ellipsis);
	result
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn removes_csi_sequences() {
		assert_eq!(sanitize("\x1b[31mred\x1b[0m text", false), "red text");
		assert_eq!(sanitize("a\x1b[2J\x1b[Hb", false), "ab");
		assert_eq!(sanitize("x\x1b[?1049hy", false), "xy");
	}

	#[test]
	fn removes_osc_and_other_strings() {
		assert_eq!(sanitize("a\x1b]0;evil title\x07b", false), "ab");
		assert_eq!(
			sanitize("a\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\b", false),
			"alinkb"
		);
		assert_eq!(sanitize("a\x1bPq#0;2;0;0;0\x1b\\b", false), "ab");
		assert_eq!(sanitize("a\u{9b}31mb", false), "ab");
		assert_eq!(sanitize("a\x1bcb\x1b(Bc", false), "abc");
	}

	#[test]
	fn removes_control_characters() {
		assert_eq!(sanitize("a\rb\nc\x07d\x08e\x00f\x7f", false), "abcdef");
		assert_eq!(sanitize("a\u{85}b", false), "ab");
	}

	#[test]
	fn converts_tabs() {
		assert_eq!(sanitize("a\t\tb", false), "a  b");
	}

	#[test]
	fn replaces_wide_and_zero_width() {
		assert_eq!(sanitize("日本", false), "??");
		assert_eq!(sanitize("a\u{200b}b", false), "a?b");
		assert_eq!(sanitize("e\u{301}", false), "e?");
		assert_eq!(sanitize("café", false), "café");
		assert_eq!(sanitize("café", true), "caf?");
	}

	#[test]
	fn truncates_long_lines() {
		let long = "x".repeat(5000);
		assert_eq!(sanitize(&long, false).chars().count(), MAX_LINE_CHARS);
	}

	#[test]
	fn ellipsis_truncation() {
		assert_eq!(truncate_with_ellipsis("hello", 10, false), "hello");
		assert_eq!(truncate_with_ellipsis("hello world", 6, false), "hello…");
		assert_eq!(truncate_with_ellipsis("hello world", 6, true), "hel...");
	}
}
