//! `command` source: runs a shell command and reads its stdout and stderr.

use std::io::BufReader;
use std::process::{Command, ExitStatus};
use std::thread;
use std::time::Duration;

use chrono::Local;

use super::{GroupChild, SourceContext, for_each_line};

/// Settings of one `command` source.
#[derive(Debug, Clone)]
pub struct CommandSettings {
	/// Shell command, run with `sh -c` (PowerShell on Windows).
	pub run: String,
	/// Start the command again when it exits.
	pub restart: bool,
	/// Pause before restarting.
	pub restart_delay: Duration,
}

/// How one run of the command ended.
enum RunOutcome {
	/// The command exited with this status.
	Exited(ExitStatus),
	/// The command could not be started or waited for.
	Failed(String),
	/// The main loop is gone; the thread must stop.
	ReceiverGone,
}

/// Body of the source thread. Runs the command, restarting it if configured.
pub fn run(settings: CommandSettings, context: SourceContext) {
	// Commands have no preload.
	if !context.preload_done() {
		return;
	}
	loop {
		let ending = match run_once(&settings.run, &context) {
			RunOutcome::ReceiverGone => return,
			RunOutcome::Exited(status) => format!("exited with {status}"),
			RunOutcome::Failed(error) => format!("failed: {error}"),
		};
		if settings.restart {
			let delay = humantime::format_duration(settings.restart_delay);
			let message = format!("command '{}' {ending}, restarting in {delay}", settings.run);
			if !context.report_error(&message) {
				return;
			}
			thread::sleep(settings.restart_delay);
		} else {
			context.report_error(&format!("command '{}' {ending}", settings.run));
			return;
		}
	}
}

/// The command line run by the system shell: `sh -c` on Unix, PowerShell
/// on Windows.
#[cfg(unix)]
fn shell_command(run: &str) -> Command {
	let mut command = Command::new("sh");
	command.arg("-c").arg(run);
	command
}

#[cfg(windows)]
fn shell_command(run: &str) -> Command {
	let mut command = Command::new("powershell");
	command
		.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
		.arg(run);
	command
}

/// Runs the command once and forwards its output until it exits.
fn run_once(run: &str, context: &SourceContext) -> RunOutcome {
	let mut child = match GroupChild::spawn(&mut shell_command(run)) {
		Ok(child) => child,
		Err(error) => return RunOutcome::Failed(format!("cannot start: {error}")),
	};
	let group = child.group();

	let stderr_reader = child.take_stderr().and_then(|stderr| {
		let stderr_context = context.clone();
		thread::Builder::new()
			.name(format!("source-command-{}-stderr", context.id))
			.spawn(move || {
				let completed = for_each_line(BufReader::new(stderr), |line| {
					stderr_context.send_live(Local::now(), &line)
				});
				if !completed {
					group.terminate();
				}
				completed
			})
			.ok()
	});

	let mut receiver_alive = true;
	if let Some(stdout) = child.take_stdout() {
		receiver_alive = for_each_line(BufReader::new(stdout), |line| {
			context.send_live(Local::now(), &line)
		});
	}
	if !receiver_alive {
		child.terminate();
	}
	// stdout is closed: the command exited (or closed it on purpose); wait
	// for it while its group stays registered for termination at app exit.
	let status = child.wait();

	if let Some(handle) = stderr_reader {
		// stderr ends when the last process holding it exits. Joining keeps
		// stderr lines ahead of the exit report.
		if !handle.join().unwrap_or(true) {
			receiver_alive = false;
		}
	}
	if !receiver_alive {
		return RunOutcome::ReceiverGone;
	}
	match status {
		Ok(status) => RunOutcome::Exited(status),
		Err(error) => RunOutcome::Failed(format!("cannot wait: {error}")),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::line::SourceMsg;
	use crate::source::LineFilter;
	use std::sync::mpsc;
	use std::time::Instant;

	/// Prints `a` and `b` on stdout and `oops` on stderr in the platform's shell.
	#[cfg(unix)]
	const TWO_LINES_AND_STDERR: &str = "printf 'a\\nb\\n'; echo oops >&2";
	#[cfg(windows)]
	const TWO_LINES_AND_STDERR: &str = "'a'; 'b'; [Console]::Error.WriteLine('oops')";

	/// How `std::process::ExitStatus` displays an exit code.
	#[cfg(unix)]
	const EXIT_STATUS: &str = "exit status";
	#[cfg(windows)]
	const EXIT_STATUS: &str = "exit code";

	fn collect_until_exit(receiver: &mpsc::Receiver<SourceMsg>) -> (bool, Vec<String>) {
		let deadline = Instant::now() + Duration::from_secs(10);
		let mut preload_done = false;
		let mut lines = Vec::new();
		while Instant::now() < deadline {
			match receiver.recv_timeout(Duration::from_millis(100)) {
				Ok(SourceMsg::PreloadDone(_)) => preload_done = true,
				Ok(SourceMsg::Live(line)) => {
					let text = line.text.to_string();
					let finished = text.contains("exited with");
					lines.push(text);
					if finished {
						break;
					}
				}
				Ok(SourceMsg::Preload(line)) => panic!("unexpected preload {line:?}"),
				Err(mpsc::RecvTimeoutError::Timeout) => {}
				Err(mpsc::RecvTimeoutError::Disconnected) => break,
			}
		}
		(preload_done, lines)
	}

	#[test]
	fn forwards_stdout_and_stderr_lines() {
		let (sender, receiver) = mpsc::sync_channel(64);
		let context = SourceContext {
			id: 3,
			label: Some("cmd".into()),
			ascii_only: false,
			filter: LineFilter::default(),
			sender,
		};
		let settings = CommandSettings {
			run: TWO_LINES_AND_STDERR.into(),
			restart: false,
			restart_delay: Duration::from_secs(5),
		};
		let worker = thread::spawn(move || run(settings, context));
		let (preload_done, lines) = collect_until_exit(&receiver);
		worker.join().unwrap();

		assert!(preload_done);
		assert!(lines.contains(&"(cmd) a".to_string()), "{lines:?}");
		assert!(lines.contains(&"(cmd) b".to_string()), "{lines:?}");
		assert!(lines.contains(&"(cmd) oops".to_string()), "{lines:?}");
		let a_index = lines.iter().position(|line| line == "(cmd) a").unwrap();
		let b_index = lines.iter().position(|line| line == "(cmd) b").unwrap();
		assert!(a_index < b_index);
		let last = lines.last().unwrap();
		assert!(last.starts_with("lmatrix: command '"), "{last}");
		assert!(
			last.contains(&format!("exited with {EXIT_STATUS}: 0")),
			"{last}"
		);
	}

	#[test]
	fn reports_restart() {
		let (sender, receiver) = mpsc::sync_channel(64);
		let context = SourceContext {
			id: 0,
			label: None,
			ascii_only: false,
			filter: LineFilter::default(),
			sender,
		};
		let settings = CommandSettings {
			run: "exit 3".into(),
			restart: true,
			restart_delay: Duration::from_millis(1500),
		};
		thread::spawn(move || run(settings, context));
		let (_, lines) = collect_until_exit(&receiver);
		// Dropping the receiver makes the thread stop after its sleep.
		drop(receiver);
		assert_eq!(
			lines.last().unwrap(),
			&format!(
				"lmatrix: command 'exit 3' exited with {EXIT_STATUS}: 3, restarting in 1s 500ms"
			)
		);
	}
}
