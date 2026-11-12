//! `journald` source: follows `journalctl -f -o json`.
//!
//! journalctl is run as a child process; libsystemd is not linked.

use std::io::BufReader;
use std::process::Command;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use chrono::{DateTime, Local};
use serde_json::Value;

use super::{GroupChild, SourceContext, for_each_line};

/// Pause before journalctl is started again after it exited
/// **(assumption)**: the spec does not say; 5 s matches the `command`
/// default restart delay.
const RESTART_DELAY: Duration = Duration::from_secs(5);

/// Time without any entry (also before the first one) after which the
/// backlog is considered complete.
const BACKLOG_GAP_TIMEOUT: Duration = Duration::from_millis(300);

/// One parsed journal entry.
#[derive(Debug, Clone, PartialEq)]
pub struct JournalEntry {
	pub ts: DateTime<Local>,
	pub text: String,
	/// Journal cursor, used to resume after journalctl restarts.
	pub cursor: Option<String>,
}

/// Parses one line of `journalctl -o json` output. Returns `None` for lines
/// that are not JSON objects.
pub fn parse_entry(line: &str) -> Option<JournalEntry> {
	let value: Value = serde_json::from_str(line).ok()?;
	let fields = value.as_object()?;
	let ts = fields
		.get("__REALTIME_TIMESTAMP")
		.and_then(parse_timestamp)
		.unwrap_or_else(Local::now);
	let message = fields
		.get("MESSAGE")
		.and_then(field_text)
		.unwrap_or_default();
	let identifier = fields
		.get("SYSLOG_IDENTIFIER")
		.and_then(field_text)
		.or_else(|| fields.get("_COMM").and_then(field_text));
	let pid = fields.get("_PID").and_then(field_text);
	let text = match (identifier, pid) {
		(Some(identifier), Some(pid)) => format!("{identifier}[{pid}]: {message}"),
		(Some(identifier), None) => format!("{identifier}: {message}"),
		(None, _) => message,
	};
	let cursor = fields
		.get("__CURSOR")
		.and_then(Value::as_str)
		.map(str::to_string);
	Some(JournalEntry { ts, text, cursor })
}

/// Text of a journal field: a string, a number, or an array of byte values
/// (used by journalctl for data that is not valid UTF-8), decoded lossily.
fn field_text(value: &Value) -> Option<String> {
	match value {
		Value::String(text) => Some(text.clone()),
		Value::Number(number) => Some(number.to_string()),
		Value::Array(items) => {
			let bytes: Vec<u8> = items
				.iter()
				.filter_map(Value::as_u64)
				.map(|byte| byte as u8)
				.collect();
			Some(String::from_utf8_lossy(&bytes).into_owned())
		}
		_ => None,
	}
}

/// Parses `__REALTIME_TIMESTAMP` (microseconds since the epoch, as a string).
fn parse_timestamp(value: &Value) -> Option<DateTime<Local>> {
	let micros: i64 = match value {
		Value::String(text) => text.parse().ok()?,
		Value::Number(number) => number.as_i64()?,
		_ => return None,
	};
	DateTime::from_timestamp_micros(micros).map(|utc| utc.with_timezone(&Local))
}

/// Decides which entries are preload and sends `PreloadDone` exactly once.
///
/// journalctl does not mark where the `-n` backlog ends, so **(assumption)**
/// the boundary is inferred: entries older than the time the source started
/// are preload; preload ends at the first entry not older than that, after
/// `preload` preload entries, or when no entry arrives for a short while.
struct PreloadGate {
	start: DateTime<Local>,
	preload: usize,
	sent: usize,
	done: bool,
}

impl PreloadGate {
	fn new(start: DateTime<Local>, preload: usize) -> Self {
		Self {
			start,
			preload,
			sent: 0,
			done: false,
		}
	}

	/// Sends `PreloadDone` unless it was already sent.
	fn finish(&mut self, context: &SourceContext) -> bool {
		if self.done {
			return true;
		}
		self.done = true;
		context.preload_done()
	}

	/// Sends one entry as preload or live line.
	fn send(&mut self, context: &SourceContext, entry: &JournalEntry) -> bool {
		if !self.done && entry.ts < self.start {
			if !context.send_preload(entry.ts, &entry.text) {
				return false;
			}
			self.sent += 1;
			if self.sent >= self.preload {
				return self.finish(context);
			}
			return true;
		}
		self.finish(context) && context.send_live(entry.ts, &entry.text)
	}

	/// How long to wait for the next entry before ending the preload.
	fn timeout(&self) -> Option<Duration> {
		(!self.done).then_some(BACKLOG_GAP_TIMEOUT)
	}
}

/// Body of the source thread.
pub fn run(units: Vec<String>, context: SourceContext, preload: usize) {
	let mut gate = PreloadGate::new(Local::now(), preload);
	if preload == 0 && !gate.finish(&context) {
		return;
	}
	let mut last_cursor: Option<String> = None;
	loop {
		let mut command = Command::new("journalctl");
		command.args(["-f", "-o", "json"]);
		match &last_cursor {
			// After a restart, continue where the previous run stopped.
			Some(cursor) => {
				command.arg(format!("--after-cursor={cursor}"));
			}
			None => {
				command.arg("-n").arg(preload.to_string());
			}
		}
		for unit in &units {
			command.arg("-u").arg(unit);
		}

		let mut child = match GroupChild::spawn(&mut command) {
			Ok(child) => child,
			Err(error) => {
				context.report_error(&format!("cannot start journalctl: {error}"));
				gate.finish(&context);
				return;
			}
		};
		if !follow(&mut child, &context, &mut gate, &mut last_cursor) {
			child.terminate();
			let _ = child.wait();
			return;
		}
		let status = match child.wait() {
			Ok(status) => status.to_string(),
			Err(error) => error.to_string(),
		};
		let delay = humantime::format_duration(RESTART_DELAY);
		if !gate.finish(&context)
			|| !context.report_error(&format!(
				"journalctl exited with {status}, restarting in {delay}"
			)) {
			return;
		}
		thread::sleep(RESTART_DELAY);
	}
}

/// Forwards the output of one journalctl run until it closes stdout.
/// Returns `false` when the main loop is gone.
fn follow(
	child: &mut GroupChild,
	context: &SourceContext,
	gate: &mut PreloadGate,
	last_cursor: &mut Option<String>,
) -> bool {
	let group = child.group();
	let stderr_reader = child.take_stderr().and_then(|stderr| {
		let stderr_context = context.clone();
		thread::Builder::new()
			.name(format!("source-journald-{}-stderr", context.id))
			.spawn(move || {
				let completed = for_each_line(BufReader::new(stderr), |line| {
					line.trim().is_empty()
						|| stderr_context.report_error(&format!("journalctl: {line}"))
				});
				if !completed {
					group.terminate();
				}
				completed
			})
			.ok()
	});

	// Parse on a reader thread so the preload timeout can be applied here.
	let (entry_sender, entry_receiver) = mpsc::sync_channel::<JournalEntry>(1024);
	let stdout_reader = child.take_stdout().and_then(|stdout| {
		thread::Builder::new()
			.name(format!("source-journald-{}-stdout", context.id))
			.spawn(move || {
				for_each_line(BufReader::new(stdout), |line| match parse_entry(&line) {
					Some(entry) => entry_sender.send(entry).is_ok(),
					None => true,
				});
			})
			.ok()
	});

	let mut receiver_alive = true;
	loop {
		let received = match gate.timeout() {
			Some(timeout) => entry_receiver.recv_timeout(timeout),
			None => entry_receiver
				.recv()
				.map_err(|_| RecvTimeoutError::Disconnected),
		};
		match received {
			Ok(entry) => {
				if entry.cursor.is_some() {
					last_cursor.clone_from(&entry.cursor);
				}
				if !gate.send(context, &entry) {
					receiver_alive = false;
					break;
				}
			}
			Err(RecvTimeoutError::Timeout) => {
				if !gate.finish(context) {
					receiver_alive = false;
					break;
				}
			}
			Err(RecvTimeoutError::Disconnected) => break,
		}
	}

	if !receiver_alive {
		// Unblocks the reader threads, which then end on EOF.
		child.terminate();
	}
	drop(entry_receiver);
	if let Some(handle) = stdout_reader {
		let _ = handle.join();
	}
	if let Some(handle) = stderr_reader
		&& !handle.join().unwrap_or(true)
	{
		receiver_alive = false;
	}
	receiver_alive
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::line::SourceMsg;
	use chrono::TimeZone;

	#[test]
	fn identifier_and_pid() {
		let entry = parse_entry(
			r#"{"__REALTIME_TIMESTAMP":"1700000000123456","SYSLOG_IDENTIFIER":"sshd","_PID":"812","_COMM":"sshd-session","MESSAGE":"Accepted publickey for git","__CURSOR":"s=abc"}"#,
		)
		.unwrap();
		assert_eq!(entry.text, "sshd[812]: Accepted publickey for git");
		assert_eq!(entry.cursor.as_deref(), Some("s=abc"));
		let expected = Local.timestamp_opt(1_700_000_000, 123_456_000).unwrap();
		assert_eq!(entry.ts, expected);
	}

	#[test]
	fn identifier_without_pid() {
		let entry =
			parse_entry(r#"{"SYSLOG_IDENTIFIER":"kernel","MESSAGE":"eth0: Link is Up"}"#).unwrap();
		assert_eq!(entry.text, "kernel: eth0: Link is Up");
	}

	#[test]
	fn falls_back_to_comm() {
		let entry = parse_entry(r#"{"_COMM":"cron","_PID":"77","MESSAGE":"tick"}"#).unwrap();
		assert_eq!(entry.text, "cron[77]: tick");
	}

	#[test]
	fn message_only() {
		let entry = parse_entry(r#"{"MESSAGE":"bare"}"#).unwrap();
		assert_eq!(entry.text, "bare");
	}

	#[test]
	fn byte_array_message() {
		let entry =
			parse_entry(r#"{"SYSLOG_IDENTIFIER":"app","MESSAGE":[104,105,255,33]}"#).unwrap();
		assert_eq!(entry.text, "app: hi\u{fffd}!");
	}

	#[test]
	fn missing_or_bad_timestamp_falls_back_to_now() {
		let before = Local::now();
		let entry = parse_entry(r#"{"__REALTIME_TIMESTAMP":"soon","MESSAGE":"x"}"#).unwrap();
		assert!(entry.ts >= before);
	}

	#[test]
	fn ignores_invalid_lines() {
		assert!(parse_entry("not json").is_none());
		assert!(parse_entry("[1,2]").is_none());
		assert!(parse_entry("").is_none());
	}

	fn entry_at(ts: DateTime<Local>, text: &str) -> JournalEntry {
		JournalEntry {
			ts,
			text: text.into(),
			cursor: None,
		}
	}

	#[test]
	fn gate_splits_backlog_from_live_entries() {
		let (sender, receiver) = mpsc::sync_channel(64);
		let context = SourceContext {
			id: 1,
			label: None,
			ascii_only: false,
			sender,
		};
		let start = Local::now();
		let mut gate = PreloadGate::new(start, 10);
		let old = start - chrono::Duration::seconds(5);
		assert!(gate.send(&context, &entry_at(old, "old")));
		assert_eq!(gate.timeout(), Some(BACKLOG_GAP_TIMEOUT));
		assert!(gate.send(&context, &entry_at(start, "new")));
		assert!(gate.send(&context, &entry_at(old, "late but live")));
		assert!(gate.finish(&context));
		assert_eq!(gate.timeout(), None);
		drop(context);

		let messages: Vec<SourceMsg> = receiver.iter().collect();
		assert_eq!(messages.len(), 4);
		assert!(matches!(&messages[0], SourceMsg::Preload(line) if &*line.text == "old"));
		assert!(matches!(messages[1], SourceMsg::PreloadDone(1)));
		assert!(matches!(&messages[2], SourceMsg::Live(line) if &*line.text == "new"));
		assert!(matches!(&messages[3], SourceMsg::Live(line) if &*line.text == "late but live"));
	}

	#[test]
	fn gate_ends_after_preload_count() {
		let (sender, receiver) = mpsc::sync_channel(64);
		let context = SourceContext {
			id: 0,
			label: None,
			ascii_only: false,
			sender,
		};
		let start = Local::now();
		let mut gate = PreloadGate::new(start, 2);
		let old = start - chrono::Duration::seconds(5);
		assert!(gate.send(&context, &entry_at(old, "one")));
		assert!(gate.send(&context, &entry_at(old, "two")));
		assert!(gate.send(&context, &entry_at(old, "three")));
		drop(context);
		let kinds: Vec<&str> = receiver
			.iter()
			.map(|message| match message {
				SourceMsg::Preload(_) => "preload",
				SourceMsg::PreloadDone(_) => "done",
				SourceMsg::Live(_) => "live",
			})
			.collect();
		assert_eq!(kinds, vec!["preload", "preload", "done", "live"]);
	}

	/// Needs a readable journal: `cargo test -- --ignored`.
	#[test]
	#[ignore]
	fn reads_real_journal() {
		let (sender, receiver) = mpsc::sync_channel(64);
		let context = SourceContext {
			id: 0,
			label: None,
			ascii_only: false,
			sender,
		};
		thread::spawn(move || run(Vec::new(), context, 3));
		let started = std::time::Instant::now();
		let mut preload_lines = Vec::new();
		loop {
			match receiver.recv_timeout(Duration::from_secs(5)).unwrap() {
				SourceMsg::Preload(line) => preload_lines.push(line.text.to_string()),
				SourceMsg::PreloadDone(_) => break,
				SourceMsg::Live(line) => eprintln!("live: {}", line.text),
			}
		}
		eprintln!("preload after {:?}: {preload_lines:#?}", started.elapsed());
		assert!(preload_lines.len() <= 3);
	}
}
