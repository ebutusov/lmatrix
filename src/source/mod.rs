//! Log sources. Each source runs in its own thread and sends [`SourceMsg`]s.

pub mod command;
pub mod demo;
pub mod file;
pub mod journald;
pub mod process;

use std::io::{self, BufRead};
use std::sync::mpsc::SyncSender;
use std::thread;

use chrono::{DateTime, Local};

use crate::config::SourceConfig;
use crate::line::{LogLine, SourceId, SourceMsg, sanitize};

pub use process::{ChildGroup, GroupChild, terminate_children};

/// Starts the thread for one configured source.
///
/// A failure to start the thread is reported as an error line; the source
/// then counts as finished preloading.
pub fn spawn(config: &SourceConfig, context: SourceContext, preload: usize) {
	let fallback = context.clone();
	let started = match config {
		SourceConfig::Journald { units, .. } => {
			let units = units.clone();
			thread::Builder::new()
				.name(format!("source-journald-{}", context.id))
				.spawn(move || journald::run(units, context, preload))
		}
		SourceConfig::File { path, .. } => {
			let pattern = path.clone();
			thread::Builder::new()
				.name(format!("source-file-{}", context.id))
				.spawn(move || file::run(pattern, context, preload))
		}
		SourceConfig::Command {
			run,
			restart,
			restart_delay,
			..
		} => {
			let settings = command::CommandSettings {
				run: run.clone(),
				restart: *restart,
				restart_delay: *restart_delay,
			};
			thread::Builder::new()
				.name(format!("source-command-{}", context.id))
				.spawn(move || command::run(settings, context))
		}
	};
	if let Err(error) = started {
		fallback.report_error(&format!("cannot start source thread: {error}"));
		fallback.preload_done();
	}
}

/// Capacity of the channel between sources and the main loop.
pub const CHANNEL_CAPACITY: usize = 8192;

/// What a source thread needs to send lines.
#[derive(Clone)]
pub struct SourceContext {
	pub id: SourceId,
	pub label: Option<String>,
	pub ascii_only: bool,
	pub sender: SyncSender<SourceMsg>,
}

impl SourceContext {
	/// Builds a sanitized line, prefixed with the label if there is one.
	pub fn make_line(&self, ts: DateTime<Local>, raw: &str) -> LogLine {
		let text = match &self.label {
			Some(label) => sanitize(&format!("{label}: {raw}"), self.ascii_only),
			None => sanitize(raw, self.ascii_only),
		};
		LogLine::new(ts, self.id, text)
	}

	/// Sends a live line. Returns `false` once the main loop is gone.
	pub fn send_live(&self, ts: DateTime<Local>, raw: &str) -> bool {
		self.sender
			.send(SourceMsg::Live(self.make_line(ts, raw)))
			.is_ok()
	}

	/// Sends a preloaded line. Returns `false` once the main loop is gone.
	pub fn send_preload(&self, ts: DateTime<Local>, raw: &str) -> bool {
		self.sender
			.send(SourceMsg::Preload(self.make_line(ts, raw)))
			.is_ok()
	}

	/// Signals that this source has sent all its preload lines.
	pub fn preload_done(&self) -> bool {
		self.sender.send(SourceMsg::PreloadDone(self.id)).is_ok()
	}

	/// Reports a source problem as an ordinary line `lmatrix: <description>`.
	pub fn report_error(&self, description: &str) -> bool {
		let text = sanitize(&format!("lmatrix: {description}"), self.ascii_only);
		self.sender
			.send(SourceMsg::Live(LogLine::new(Local::now(), self.id, text)))
			.is_ok()
	}
}

/// Longest line kept from a child or file, in bytes. Longer lines are cut
/// here (and to [`crate::line::MAX_LINE_CHARS`] by [`sanitize`]).
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// Converts raw line bytes (without the `\n`) to text: strips a trailing
/// `\r`, caps the length at [`MAX_LINE_BYTES`] and decodes lossily.
pub fn bytes_to_line(bytes: &[u8]) -> String {
	let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
	let bytes = &bytes[..bytes.len().min(MAX_LINE_BYTES)];
	String::from_utf8_lossy(bytes).into_owned()
}

/// Calls `handle_line` for each line of `reader` until end of input, a read
/// error, or `handle_line` returning `false`. Returns `false` in the last
/// case and `true` otherwise.
///
/// Lines are split on `\n` and converted with [`bytes_to_line`]. A final
/// line without newline is passed on as well.
pub fn for_each_line<R: BufRead>(
	mut reader: R,
	mut handle_line: impl FnMut(String) -> bool,
) -> bool {
	let mut buffer = Vec::new();
	loop {
		buffer.clear();
		match reader.read_until(b'\n', &mut buffer) {
			Ok(0) => return true,
			Ok(_) => {
				let content = buffer.strip_suffix(b"\n").unwrap_or(&buffer);
				if !handle_line(bytes_to_line(content)) {
					return false;
				}
			}
			Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
			Err(_) => return true,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn splits_lines_and_strips_carriage_returns() {
		let input: &[u8] = b"one\r\ntwo\n\xffbad\nlast";
		let mut lines = Vec::new();
		assert!(for_each_line(input, |line| {
			lines.push(line);
			true
		}));
		assert_eq!(lines, vec!["one", "two", "\u{fffd}bad", "last"]);
	}

	#[test]
	fn stops_when_handler_refuses() {
		let input: &[u8] = b"a\nb\nc\n";
		let mut count = 0;
		assert!(!for_each_line(input, |_| {
			count += 1;
			count < 2
		}));
		assert_eq!(count, 2);
	}
}
