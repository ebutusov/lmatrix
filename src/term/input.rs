//! Terminal input on its own thread, and detection of a closed terminal.
//!
//! crossterm 0.29 spins forever inside `event::poll` once the terminal is
//! gone: its read loop neither stops on end of file nor on `EIO`. Reading
//! input on a separate thread keeps the main loop in control, so it still
//! sees shutdown signals and a hung-up terminal and can exit.

use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use crossterm::event::{self, Event};

/// How often the input thread checks whether it should stop.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Result of waiting for input.
#[derive(Debug)]
pub enum Input {
	Event(Event),
	/// Nothing arrived within the timeout.
	Timeout,
	/// The input thread has ended: the terminal can no longer be read.
	Closed,
}

/// Receives terminal events from a background thread.
pub struct InputReader {
	events: Receiver<Event>,
	stop: Arc<AtomicBool>,
}

impl InputReader {
	/// Starts the input thread.
	pub fn spawn() -> std::io::Result<Self> {
		let (sender, events) = mpsc::channel();
		let stop = Arc::new(AtomicBool::new(false));
		let thread_stop = Arc::clone(&stop);
		thread::Builder::new().name("input".into()).spawn(move || {
			while !thread_stop.load(Ordering::SeqCst) {
				match event::poll(POLL_INTERVAL) {
					Ok(true) => match event::read() {
						Ok(event) => {
							if sender.send(event).is_err() {
								break;
							}
						}
						Err(_) => break,
					},
					Ok(false) => {}
					Err(_) => break,
				}
			}
		})?;
		Ok(Self { events, stop })
	}

	/// Waits up to `timeout` for the next event.
	pub fn next(&self, timeout: Duration) -> Input {
		match self.events.recv_timeout(timeout) {
			Ok(event) => Input::Event(event),
			Err(RecvTimeoutError::Timeout) => Input::Timeout,
			Err(RecvTimeoutError::Disconnected) => Input::Closed,
		}
	}
}

impl Drop for InputReader {
	fn drop(&mut self) {
		// The thread notices within one poll interval. If it is stuck in
		// crossterm, process exit ends it.
		self.stop.store(true, Ordering::SeqCst);
	}
}

/// True when the terminal on stdin or stdout has hung up (for example a
/// dropped SSH session or a killed tmux pane).
pub fn terminal_hung_up() -> bool {
	[std::io::stdin().as_raw_fd(), std::io::stdout().as_raw_fd()]
		.into_iter()
		.any(fd_hung_up)
}

fn fd_hung_up(fd: i32) -> bool {
	// SAFETY: isatty only inspects the descriptor.
	if unsafe { libc::isatty(fd) } != 1 {
		return false;
	}
	let mut poll_fd = libc::pollfd {
		fd,
		events: 0,
		revents: 0,
	};
	// SAFETY: one valid pollfd and a zero timeout; poll does not block.
	let ready = unsafe { libc::poll(&mut poll_fd, 1, 0) };
	ready > 0 && poll_fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
}
