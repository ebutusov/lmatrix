//! `journald` source: follows `journalctl -f -o json`, locally or on a
//! remote host through ssh.
//!
//! journalctl is run as a child process; libsystemd is not linked.

use std::io::BufReader;
use std::process::{Command, ExitStatus};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use chrono::{DateTime, Local};
use serde_json::Value;

use super::{GroupChild, SourceContext, for_each_line};
use crate::config::RemoteHost;

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

/// Fields requested from journalctl; it always adds `__CURSOR` and
/// `__REALTIME_TIMESTAMP`. Leaving out the rest roughly halves the output,
/// which matters when it comes over ssh. Needs systemd 236 or later.
const OUTPUT_FIELDS: &str = "--output-fields=MESSAGE,SYSLOG_IDENTIFIER,_PID,_COMM";

/// Options always given to ssh. Batch mode, because stdin is closed and the
/// screen is in raw mode, so ssh could not ask for a password. Keepalives,
/// so a dead connection is noticed and the source reconnects.
const SSH_OPTIONS: [&str; 7] = [
	"-T",
	"-o",
	"BatchMode=yes",
	"-o",
	"ServerAliveInterval=15",
	"-o",
	"ServerAliveCountMax=2",
];

/// Where a journalctl run starts reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start<'a> {
	/// The last `n` entries (`-n n`; `0` means only new ones).
	Last(usize),
	/// Right after the entry with this cursor.
	AfterCursor(&'a str),
}

/// Arguments for one journalctl run. With `follow`, journalctl keeps running
/// and prints new entries; without, it prints the requested entries and
/// exits.
pub fn journalctl_args(units: &[String], start: Start, follow: bool) -> Vec<String> {
	let mut args = Vec::new();
	if follow {
		args.push("-f".to_string());
	} else {
		args.push("--no-pager".to_string());
	}
	args.extend([
		"-o".to_string(),
		"json".to_string(),
		OUTPUT_FIELDS.to_string(),
	]);
	match start {
		Start::Last(count) => args.extend(["-n".to_string(), count.to_string()]),
		Start::AfterCursor(cursor) => args.push(format!("--after-cursor={cursor}")),
	}
	for unit in units {
		args.extend(["-u".to_string(), unit.clone()]);
	}
	args
}

/// Quotes `text` as one word for a POSIX shell. ssh joins its remote
/// arguments with spaces and hands them to the remote user's shell, and
/// cursors contain `;`.
pub fn shell_quote(text: &str) -> String {
	format!("'{}'", text.replace('\'', "'\\''"))
}

/// Program and arguments that run journalctl with `args`, locally or on
/// `remote` through ssh.
pub fn command_line(remote: Option<&RemoteHost>, args: &[String]) -> (String, Vec<String>) {
	match remote {
		None => ("journalctl".to_string(), args.to_vec()),
		Some(remote) => {
			let remote_command = std::iter::once("journalctl".to_string())
				.chain(args.iter().map(|arg| shell_quote(arg)))
				.collect::<Vec<_>>()
				.join(" ");
			let mut ssh_args: Vec<String> = SSH_OPTIONS
				.iter()
				.map(|option| option.to_string())
				.collect();
			ssh_args.extend(remote.ssh_args.iter().cloned());
			ssh_args.extend(["--".to_string(), remote.host.clone(), remote_command]);
			("ssh".to_string(), ssh_args)
		}
	}
}

/// Body of the source thread.
pub fn run(units: Vec<String>, remote: Option<RemoteHost>, context: SourceContext, preload: usize) {
	let name = match &remote {
		Some(remote) => format!("journalctl on {}", remote.host),
		None => "journalctl".to_string(),
	};
	let mut gate = PreloadGate::new(Local::now(), preload);
	let mut last_cursor: Option<String> = None;
	if remote.is_some() {
		// Over the network neither the time of the entries (clock skew)
		// nor gaps in the output (latency) tell where the backlog ends, so
		// it is read by a separate run that ends after it.
		if preload > 0 {
			let args = journalctl_args(&units, Start::Last(preload), false);
			match run_journalctl(
				&name,
				remote.as_ref(),
				&args,
				&context,
				&mut Phase::Preload,
				&mut last_cursor,
			) {
				RunOutcome::ReceiverGone => return,
				RunOutcome::Exited(status) if !status.success() => {
					if !context.report_error(&format!(
						"{name} exited with {status} while reading the backlog"
					)) {
						return;
					}
				}
				RunOutcome::Exited(_) => {}
				RunOutcome::Failed(error) => {
					context.report_error(&format!("cannot start {name}: {error}"));
					gate.finish(&context);
					return;
				}
			}
		}
		if !gate.finish(&context) {
			return;
		}
	} else if preload == 0 && !gate.finish(&context) {
		return;
	}

	loop {
		let start = match &last_cursor {
			// After a restart, continue where the previous run stopped.
			Some(cursor) => Start::AfterCursor(cursor),
			None if remote.is_some() => Start::Last(0),
			None => Start::Last(preload),
		};
		let args = journalctl_args(&units, start, true);
		let mut phase = Phase::Follow(&mut gate);
		let status = match run_journalctl(
			&name,
			remote.as_ref(),
			&args,
			&context,
			&mut phase,
			&mut last_cursor,
		) {
			RunOutcome::ReceiverGone => return,
			RunOutcome::Exited(status) => status.to_string(),
			RunOutcome::Failed(error) => {
				// The program is missing or cannot run: retrying will not help.
				context.report_error(&format!("cannot start {name}: {error}"));
				gate.finish(&context);
				return;
			}
		};
		let delay = humantime::format_duration(RESTART_DELAY);
		if !gate.finish(&context)
			|| !context.report_error(&format!(
				"{name} exited with {status}, restarting in {delay}"
			)) {
			return;
		}
		thread::sleep(RESTART_DELAY);
	}
}

/// How the entries of one run are sent.
enum Phase<'a> {
	/// Everything is preload; the run ends after the backlog.
	Preload,
	/// Entries go through the preload gate (live once it is done).
	Follow(&'a mut PreloadGate),
}

/// How one journalctl run ended.
enum RunOutcome {
	Exited(ExitStatus),
	/// journalctl (or ssh) could not be started or waited for.
	Failed(String),
	/// The main loop is gone; the thread must stop.
	ReceiverGone,
}

/// Starts journalctl once and forwards its output until it exits.
fn run_journalctl(
	name: &str,
	remote: Option<&RemoteHost>,
	args: &[String],
	context: &SourceContext,
	phase: &mut Phase,
	last_cursor: &mut Option<String>,
) -> RunOutcome {
	let (program, program_args) = command_line(remote, args);
	let mut command = Command::new(program);
	command.args(program_args);
	let mut child = match GroupChild::spawn(&mut command) {
		Ok(child) => child,
		Err(error) => return RunOutcome::Failed(error.to_string()),
	};
	if !follow(name, &mut child, context, phase, last_cursor) {
		child.terminate();
		let _ = child.wait();
		return RunOutcome::ReceiverGone;
	}
	match child.wait() {
		Ok(status) => RunOutcome::Exited(status),
		Err(error) => RunOutcome::Failed(format!("cannot wait: {error}")),
	}
}

/// Forwards the output of one journalctl run until it closes stdout.
/// Returns `false` when the main loop is gone.
fn follow(
	name: &str,
	child: &mut GroupChild,
	context: &SourceContext,
	phase: &mut Phase,
	last_cursor: &mut Option<String>,
) -> bool {
	let stderr_prefix = name.to_string();
	let group = child.group();
	let stderr_reader = child.take_stderr().and_then(|stderr| {
		let stderr_context = context.clone();
		thread::Builder::new()
			.name(format!("source-journald-{}-stderr", context.id))
			.spawn(move || {
				let completed = for_each_line(BufReader::new(stderr), |line| {
					line.trim().is_empty()
						|| stderr_context.report_error(&format!("{stderr_prefix}: {line}"))
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
		let timeout = match phase {
			Phase::Preload => None,
			Phase::Follow(gate) => gate.timeout(),
		};
		let received = match timeout {
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
				let sent = match phase {
					Phase::Preload => context.send_preload(entry.ts, &entry.text),
					Phase::Follow(gate) => gate.send(context, &entry),
				};
				if !sent {
					receiver_alive = false;
					break;
				}
			}
			Err(RecvTimeoutError::Timeout) => {
				if let Phase::Follow(gate) = phase
					&& !gate.finish(context)
				{
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

	fn strings(items: &[&str]) -> Vec<String> {
		items.iter().map(|item| item.to_string()).collect()
	}

	#[test]
	fn args_for_follow_with_backlog() {
		assert_eq!(
			journalctl_args(&strings(&["ssh.service"]), Start::Last(200), true),
			strings(&[
				"-f",
				"-o",
				"json",
				OUTPUT_FIELDS,
				"-n",
				"200",
				"-u",
				"ssh.service"
			])
		);
	}

	#[test]
	fn args_for_backlog_only() {
		assert_eq!(
			journalctl_args(&[], Start::Last(50), false),
			strings(&["--no-pager", "-o", "json", OUTPUT_FIELDS, "-n", "50"])
		);
	}

	#[test]
	fn args_after_cursor() {
		assert_eq!(
			journalctl_args(&[], Start::AfterCursor("s=a;i=1"), true),
			strings(&["-f", "-o", "json", OUTPUT_FIELDS, "--after-cursor=s=a;i=1"])
		);
	}

	#[test]
	fn shell_quoting() {
		assert_eq!(shell_quote("plain"), "'plain'");
		assert_eq!(shell_quote(""), "''");
		assert_eq!(shell_quote("a b;c"), "'a b;c'");
		assert_eq!(shell_quote("it's"), r"'it'\''s'");
	}

	#[test]
	fn local_command_line() {
		let args = strings(&["-f", "-n", "5"]);
		assert_eq!(command_line(None, &args), ("journalctl".to_string(), args));
	}

	#[test]
	fn remote_command_line() {
		let remote = RemoteHost {
			host: "admin@web1".to_string(),
			ssh_args: strings(&["-C", "-p", "2222"]),
		};
		let args = strings(&["-f", "--after-cursor=s=a;i=1", "-u", "it's.service"]);
		let (program, argv) = command_line(Some(&remote), &args);
		assert_eq!(program, "ssh");
		let mut expected = strings(&SSH_OPTIONS);
		expected.extend(strings(&["-C", "-p", "2222", "--", "admin@web1"]));
		expected
			.push(r"journalctl '-f' '--after-cursor=s=a;i=1' '-u' 'it'\''s.service'".to_string());
		assert_eq!(argv, expected);
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
		thread::spawn(move || run(Vec::new(), None, context, 3));
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
