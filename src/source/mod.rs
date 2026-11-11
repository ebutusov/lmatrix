//! Log sources. Each source runs in its own thread and sends [`SourceMsg`]s.

pub mod demo;

use std::sync::Mutex;
use std::sync::mpsc::SyncSender;

use chrono::{DateTime, Local};

use crate::line::{LogLine, SourceId, SourceMsg, sanitize};

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

static CHILD_GROUPS: Mutex<Vec<i32>> = Mutex::new(Vec::new());

/// Remembers a child process group so it is terminated on exit.
pub fn register_child_group(pgid: i32) {
	if let Ok(mut groups) = CHILD_GROUPS.lock() {
		groups.push(pgid);
	}
}

/// Forgets a child process group that has exited.
pub fn unregister_child_group(pgid: i32) {
	if let Ok(mut groups) = CHILD_GROUPS.lock() {
		groups.retain(|group| *group != pgid);
	}
}

/// Sends SIGTERM to every registered child process group.
pub fn terminate_children() {
	let groups = match CHILD_GROUPS.lock() {
		Ok(mut groups) => std::mem::take(&mut *groups),
		Err(_) => return,
	};
	for pgid in groups {
		if pgid > 0 {
			// SAFETY: kill() has no memory-safety preconditions; a negative
			// pid addresses the process group we created for the child.
			unsafe {
				libc::kill(-pgid, libc::SIGTERM);
			}
		}
	}
}
