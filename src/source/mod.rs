//! Log sources. Each source runs in its own thread and sends [`SourceMsg`]s.

pub mod command;
pub mod demo;
pub mod eventlog;
pub mod file;
pub mod journald;
pub mod process;

use std::io::{self, BufRead};
use std::sync::mpsc::SyncSender;
use std::thread;

use chrono::{DateTime, Local};

use regex::Regex;

use crate::config::{SourceConfig, SourceFilter};
use crate::line::{LogLine, SourceId, SourceMsg, sanitize};

pub use process::{ChildGroup, GroupChild, terminate_children};

/// Starts the thread for one configured source.
///
/// A failure to start the thread is reported as an error line; the source
/// then counts as finished preloading.
pub fn spawn(config: &SourceConfig, context: SourceContext, preload: usize) {
	let fallback = context.clone();
	let started = match config {
		SourceConfig::Journald { units, remote, .. } => {
			let units = units.clone();
			let remote = remote.clone();
			thread::Builder::new()
				.name(format!("source-journald-{}", context.id))
				.spawn(move || journald::run(units, remote, context, preload))
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
		SourceConfig::Eventlog { channels, .. } => {
			let channels = channels.clone();
			thread::Builder::new()
				.name(format!("source-eventlog-{}", context.id))
				.spawn(move || eventlog::run(channels, context, preload))
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
	pub filter: LineFilter,
	pub sender: SyncSender<SourceMsg>,
}

/// Compiled `include` / `exclude` regexes of a source.
#[derive(Debug, Clone, Default)]
pub struct LineFilter {
	pub include: Option<Regex>,
	pub exclude: Option<Regex>,
}

impl LineFilter {
	/// Compiles a validated [`SourceFilter`].
	pub fn new(config: &SourceFilter) -> Self {
		let compile = |pattern: &Option<String>| {
			pattern
				.as_deref()
				.map(|pattern| Regex::new(pattern).expect("validated"))
		};
		Self {
			include: compile(&config.include),
			exclude: compile(&config.exclude),
		}
	}

	/// Whether a line with this (sanitized, unlabeled) text is kept.
	pub fn accepts(&self, text: &str) -> bool {
		self.include
			.as_ref()
			.is_none_or(|regex| regex.is_match(text))
			&& !self
				.exclude
				.as_ref()
				.is_some_and(|regex| regex.is_match(text))
	}
}

impl SourceContext {
	/// Builds a sanitized line, prefixed with the label if there is one.
	/// `None` when the source's filter drops it.
	pub fn make_line(&self, ts: DateTime<Local>, raw: &str) -> Option<LogLine> {
		let text = sanitize(raw, self.ascii_only);
		if !self.filter.accepts(&text) {
			return None;
		}
		let text = match &self.label {
			Some(label) => sanitize(&format!("({label}) {text}"), self.ascii_only),
			None => text,
		};
		Some(LogLine::new(ts, self.id, text))
	}

	/// Whether the source's filter keeps a line with this raw text.
	pub fn accepts(&self, raw: &str) -> bool {
		self.filter.accepts(&sanitize(raw, self.ascii_only))
	}

	/// Sends a live line, unless the filter drops it. Returns `false` once
	/// the main loop is gone.
	pub fn send_live(&self, ts: DateTime<Local>, raw: &str) -> bool {
		match self.make_line(ts, raw) {
			Some(line) => self.sender.send(SourceMsg::Live(line)).is_ok(),
			None => true,
		}
	}

	/// Sends a preloaded line, unless the filter drops it. Returns `false`
	/// once the main loop is gone.
	pub fn send_preload(&self, ts: DateTime<Local>, raw: &str) -> bool {
		match self.make_line(ts, raw) {
			Some(line) => self.sender.send(SourceMsg::Preload(line)).is_ok(),
			None => true,
		}
	}

	/// Signals that this source has sent all its preload lines.
	pub fn preload_done(&self) -> bool {
		self.sender.send(SourceMsg::PreloadDone(self.id)).is_ok()
	}

	/// Reports a source problem as an ordinary line `lmatrix: <description>`.
	/// Never filtered.
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
	use std::sync::mpsc;

	fn context_with(
		label: Option<&str>,
		include: Option<&str>,
		exclude: Option<&str>,
	) -> (SourceContext, mpsc::Receiver<SourceMsg>) {
		let (sender, receiver) = mpsc::sync_channel(16);
		let filter = LineFilter::new(&SourceFilter {
			include: include.map(str::to_string),
			exclude: exclude.map(str::to_string),
		});
		let context = SourceContext {
			id: 0,
			label: label.map(str::to_string),
			ascii_only: false,
			filter,
			sender,
		};
		(context, receiver)
	}

	fn texts(receiver: mpsc::Receiver<SourceMsg>) -> Vec<String> {
		receiver
			.iter()
			.map(|message| match message {
				SourceMsg::Live(line) | SourceMsg::Preload(line) => line.text.to_string(),
				SourceMsg::PreloadDone(_) => "done".to_string(),
			})
			.collect()
	}

	#[test]
	fn exclude_drops_matching_lines_but_not_errors() {
		let (context, receiver) = context_with(
			Some("lc"),
			None,
			Some("^Application Information SynTPEnhService\\["),
		);
		let now = Local::now();
		assert!(context.send_live(
			now,
			"Application Information SynTPEnhService[0]: (no message)"
		));
		assert!(context.send_preload(now, "Application Error disk[7]: bad block"));
		assert!(context.report_error("SynTPEnhService is noisy"));
		assert!(!context.accepts("Application Information SynTPEnhService[0]: x"));
		drop(context);
		assert_eq!(
			texts(receiver),
			vec![
				"(lc) Application Error disk[7]: bad block",
				"lmatrix: SynTPEnhService is noisy"
			]
		);
	}

	#[test]
	fn include_keeps_only_matching_lines() {
		let (context, receiver) = context_with(None, Some("(?i)error|warning"), Some("ignored"));
		let now = Local::now();
		for text in ["an Error here", "fine", "warning: x", "error, ignored"] {
			assert!(context.send_live(now, text));
		}
		drop(context);
		assert_eq!(texts(receiver), vec!["an Error here", "warning: x"]);
	}

	#[test]
	fn filter_sees_sanitized_text() {
		let (context, receiver) = context_with(None, Some("^red alert$"), None);
		assert!(context.send_live(Local::now(), "\x1b[31mred alert\x1b[0m"));
		drop(context);
		assert_eq!(texts(receiver), vec!["red alert"]);
	}

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
