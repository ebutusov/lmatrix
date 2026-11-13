//! `eventlog` source: the local Windows event log (System, Application or
//! any other channel), read through the Evt* API of `wevtapi.dll`.
//!
//! The backlog is an exact reverse query of the newest entries of each
//! channel; live entries come from a pull subscription that starts right
//! after a bookmark on the newest backlog entry, so nothing is lost or
//! repeated in between.

use chrono::{DateTime, Local};

use super::SourceContext;

/// One rendered event.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EventRecord {
	pub channel: String,
	pub provider: String,
	pub event_id: u32,
	pub level: u8,
	pub record_id: u64,
	/// `TimeCreated` as a FILETIME: 100 ns ticks since 1601-01-01 UTC.
	pub filetime: u64,
	/// The publisher's formatted message, if it could be resolved.
	pub message: Option<String>,
	/// The event's EventData / UserData values, used when there is no message.
	pub data: Vec<String>,
}

/// FILETIME ticks between 1601-01-01 and 1970-01-01.
const FILETIME_UNIX_EPOCH: u64 = 116_444_736_000_000_000;

/// Converts a FILETIME to local time. `None` before 1970 or out of range.
pub fn filetime_to_local(filetime: u64) -> Option<DateTime<Local>> {
	let ticks = filetime.checked_sub(FILETIME_UNIX_EPOCH)?;
	let micros = i64::try_from(ticks / 10).ok()?;
	DateTime::from_timestamp_micros(micros).map(|utc| utc.with_timezone(&Local))
}

/// Name of a standard event level.
pub fn level_name(level: u8) -> String {
	match level {
		1 => "Critical".into(),
		2 => "Error".into(),
		3 => "Warning".into(),
		0 | 4 => "Information".into(),
		5 => "Verbose".into(),
		other => format!("Level{other}"),
	}
}

/// Joins the lines of a multi-line message with single spaces. Without
/// this, [`crate::line::sanitize`] would remove the line breaks and glue
/// the lines together.
pub fn flatten_message(message: &str) -> String {
	message.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Line text: `<Channel> <Level> <Provider>[<EventID>]: <message>`.
pub fn format_line(record: &EventRecord) -> String {
	let message = record
		.message
		.as_deref()
		.map(flatten_message)
		.filter(|message| !message.is_empty())
		.or_else(|| {
			let data = flatten_message(&record.data.join(", "));
			(!data.is_empty()).then_some(data)
		})
		.unwrap_or_else(|| "(no message)".to_string());
	format!(
		"{} {} {}[{}]: {}",
		record.channel,
		level_name(record.level),
		record.provider,
		record.event_id,
		message
	)
}

/// Merges the backlogs of several channels: oldest first, at most the
/// newest `count` entries.
pub fn merge_backlog(per_channel: Vec<Vec<EventRecord>>, count: usize) -> Vec<EventRecord> {
	let mut merged: Vec<EventRecord> = per_channel.into_iter().flatten().collect();
	merged.sort_by_key(|record| (record.filetime, record.record_id));
	let skip = merged.len().saturating_sub(count);
	merged.split_off(skip)
}

/// Body of the source thread.
#[cfg(windows)]
pub fn run(channels: Vec<String>, context: SourceContext, preload: usize) {
	ffi::run(channels, context, preload);
}

/// Body of the source thread. Not reached in practice: the config check
/// refuses eventlog sources outside Windows.
#[cfg(not(windows))]
pub fn run(_channels: Vec<String>, context: SourceContext, _preload: usize) {
	context.report_error("eventlog: the Windows event log is only available on Windows");
	context.preload_done();
}

#[cfg(windows)]
// The Evt* constants keep their Win32 names, also in match patterns.
#[allow(non_upper_case_globals)]
mod ffi {
	use std::collections::HashMap;
	use std::io;
	use std::thread;
	use std::time::{Duration, Instant};

	use windows_sys::Win32::Foundation::{
		CloseHandle, ERROR_EVT_MAX_INSERTS_REACHED, ERROR_EVT_UNRESOLVED_PARAMETER_INSERT,
		ERROR_EVT_UNRESOLVED_VALUE_INSERT, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_OPERATION,
		ERROR_NO_MORE_ITEMS, ERROR_TIMEOUT, GetLastError, HANDLE, WAIT_FAILED,
	};
	use windows_sys::Win32::System::EventLog::{
		EVT_HANDLE, EVT_VARIANT, EVT_VARIANT_TYPE_ARRAY, EVT_VARIANT_TYPE_MASK, EvtClose,
		EvtCreateBookmark, EvtCreateRenderContext, EvtFormatMessage, EvtFormatMessageEvent,
		EvtNext, EvtOpenPublisherMetadata, EvtQuery, EvtQueryChannelPath, EvtQueryReverseDirection,
		EvtRender, EvtRenderContextSystem, EvtRenderContextUser, EvtRenderEventValues,
		EvtSubscribe, EvtSubscribeStartAfterBookmark, EvtSubscribeStrict,
		EvtSubscribeToFutureEvents, EvtSystemChannel, EvtSystemEventID, EvtSystemEventRecordId,
		EvtSystemLevel, EvtSystemProviderName, EvtSystemTimeCreated, EvtUpdateBookmark,
		EvtVarTypeAnsiString, EvtVarTypeByte, EvtVarTypeFileTime, EvtVarTypeHexInt32,
		EvtVarTypeHexInt64, EvtVarTypeInt16, EvtVarTypeInt32, EvtVarTypeInt64, EvtVarTypeSByte,
		EvtVarTypeString, EvtVarTypeUInt16, EvtVarTypeUInt32, EvtVarTypeUInt64,
	};
	use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForMultipleObjects};

	use chrono::Local;

	use super::{EventRecord, filetime_to_local, format_line, merge_backlog};
	use crate::source::SourceContext;

	/// Pause before a broken subscription is opened again.
	const RESUBSCRIBE_DELAY: Duration = Duration::from_secs(5);
	/// Longest wait for a signal; on every wake all channels are drained,
	/// so a missed signal delays an entry by at most this long.
	const WAIT_TIMEOUT_MS: u32 = 1000;
	/// Events fetched per `EvtNext` call.
	const BATCH: usize = 64;
	/// `EvtNext` timeout for queries: wait until the batch is ready.
	const INFINITE: u32 = u32::MAX;

	/// An Evt* handle, closed on drop.
	struct EvtHandle(EVT_HANDLE);

	impl EvtHandle {
		/// Wraps a returned handle; `0` means failure (see `GetLastError`).
		fn new(handle: EVT_HANDLE) -> io::Result<Self> {
			if handle == 0 {
				Err(io::Error::last_os_error())
			} else {
				Ok(Self(handle))
			}
		}
	}

	impl Drop for EvtHandle {
		fn drop(&mut self) {
			unsafe { EvtClose(self.0) };
		}
	}

	/// A manual-reset Win32 event, closed on drop.
	struct Signal(HANDLE);

	impl Signal {
		fn new() -> io::Result<Self> {
			let handle = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
			if handle.is_null() {
				Err(io::Error::last_os_error())
			} else {
				Ok(Self(handle))
			}
		}

		fn reset(&self) {
			unsafe { ResetEvent(self.0) };
		}
	}

	impl Drop for Signal {
		fn drop(&mut self) {
			unsafe { CloseHandle(self.0) };
		}
	}

	fn wide(text: &str) -> Vec<u16> {
		text.encode_utf16().chain(std::iter::once(0)).collect()
	}

	/// Reads a NUL-terminated UTF-16 string.
	///
	/// # Safety
	/// `pointer` must be null or point to a NUL-terminated UTF-16 string.
	unsafe fn from_wide_ptr(pointer: *const u16) -> String {
		if pointer.is_null() {
			return String::new();
		}
		let mut length = 0;
		while unsafe { *pointer.add(length) } != 0 {
			length += 1;
		}
		String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(pointer, length) })
	}

	/// What `EvtNext` returned.
	enum Batch {
		Events(Vec<EvtHandle>),
		/// Nothing (more) to read for now.
		Empty,
		Failed(io::Error),
	}

	fn next_batch(result_set: &EvtHandle, timeout: u32) -> Batch {
		let mut raw = [0isize; BATCH];
		let mut returned = 0u32;
		let ok = unsafe {
			EvtNext(
				result_set.0,
				BATCH as u32,
				raw.as_mut_ptr(),
				timeout,
				0,
				&mut returned,
			)
		};
		if ok == 0 {
			let code = unsafe { GetLastError() };
			// A pull subscription with nothing pending fails with
			// ERROR_INVALID_OPERATION (undocumented; winlogbeat handles it
			// the same way).
			let nothing_pending = code == ERROR_NO_MORE_ITEMS
				|| code == ERROR_TIMEOUT
				|| (code == ERROR_INVALID_OPERATION && returned == 0);
			return if nothing_pending {
				Batch::Empty
			} else {
				Batch::Failed(io::Error::from_raw_os_error(code as i32))
			};
		}
		Batch::Events(
			raw[..returned as usize]
				.iter()
				.map(|&handle| EvtHandle(handle))
				.collect(),
		)
	}

	/// Turns event handles into [`EventRecord`]s.
	struct Renderer {
		system: EvtHandle,
		user: EvtHandle,
		/// Publisher metadata by provider name; `None` when the provider
		/// cannot be opened (for example, it was uninstalled).
		publishers: HashMap<String, Option<EvtHandle>>,
		/// Reused buffer for `EvtRender`, as `u64` for `EVT_VARIANT`
		/// alignment.
		values: Vec<u64>,
	}

	impl Renderer {
		fn new() -> io::Result<Self> {
			let system = EvtHandle::new(unsafe {
				EvtCreateRenderContext(0, std::ptr::null(), EvtRenderContextSystem)
			})?;
			let user = EvtHandle::new(unsafe {
				EvtCreateRenderContext(0, std::ptr::null(), EvtRenderContextUser)
			})?;
			Ok(Self {
				system,
				user,
				publishers: HashMap::new(),
				values: vec![0; 256],
			})
		}

		/// Renders the values of `context` for `event` into `self.values`
		/// and returns how many there are.
		fn render_values(&mut self, context: EVT_HANDLE, event: &EvtHandle) -> io::Result<usize> {
			loop {
				let size = (self.values.len() * 8) as u32;
				let mut used = 0u32;
				let mut count = 0u32;
				let ok = unsafe {
					EvtRender(
						context,
						event.0,
						EvtRenderEventValues,
						size,
						self.values.as_mut_ptr().cast(),
						&mut used,
						&mut count,
					)
				};
				if ok != 0 {
					return Ok(count as usize);
				}
				let code = unsafe { GetLastError() };
				let needed = (used as usize).div_ceil(8);
				if code != ERROR_INSUFFICIENT_BUFFER || needed <= self.values.len() {
					return Err(io::Error::from_raw_os_error(code as i32));
				}
				self.values.resize(needed, 0);
			}
		}

		fn variants(&self, count: usize) -> &[EVT_VARIANT] {
			let available = self.values.len() * 8 / size_of::<EVT_VARIANT>();
			unsafe { std::slice::from_raw_parts(self.values.as_ptr().cast(), count.min(available)) }
		}

		fn render(&mut self, event: &EvtHandle, channel: &str) -> EventRecord {
			let mut record = EventRecord {
				channel: channel.to_string(),
				..EventRecord::default()
			};
			let count = match self.render_values(self.system.0, event) {
				Ok(count) => count,
				Err(error) => {
					// Show why instead of an empty line.
					record.message = Some(format!("(cannot read event: {error})"));
					return record;
				}
			};
			{
				let values = self.variants(count);
				let get = |id: i32| values.get(id as usize);
				if let Some(text) = get(EvtSystemProviderName).and_then(variant_text) {
					record.provider = text;
				}
				if let Some(text) = get(EvtSystemChannel).and_then(variant_text)
					&& !text.is_empty()
				{
					record.channel = text;
				}
				if let Some(value) = get(EvtSystemEventID).and_then(variant_u64) {
					record.event_id = value as u32;
				}
				if let Some(value) = get(EvtSystemLevel).and_then(variant_u64) {
					record.level = value as u8;
				}
				if let Some(value) = get(EvtSystemEventRecordId).and_then(variant_u64) {
					record.record_id = value;
				}
				if let Some(value) = get(EvtSystemTimeCreated).and_then(variant_u64) {
					record.filetime = value;
				}
			}
			record.message = self.format_message(event, &record.provider);
			if record.message.is_none()
				&& let Ok(count) = self.render_values(self.user.0, event)
			{
				record.data = self
					.variants(count)
					.iter()
					.filter_map(variant_text)
					.filter(|text| !text.trim().is_empty())
					.collect();
			}
			record
		}

		/// The publisher's message for `event`. Text that comes back with a
		/// partial error (an insert that could not be resolved) is kept.
		fn format_message(&mut self, event: &EvtHandle, provider: &str) -> Option<String> {
			if provider.is_empty() {
				return None;
			}
			let publisher = self
				.publishers
				.entry(provider.to_string())
				.or_insert_with(|| {
					let name = wide(provider);
					EvtHandle::new(unsafe {
						EvtOpenPublisherMetadata(0, name.as_ptr(), std::ptr::null(), 0, 0)
					})
					.ok()
				})
				.as_ref()?;
			let mut buffer = vec![0u16; 1024];
			loop {
				let mut used = 0u32;
				let ok = unsafe {
					EvtFormatMessage(
						publisher.0,
						event.0,
						0,
						0,
						std::ptr::null(),
						EvtFormatMessageEvent,
						buffer.len() as u32,
						buffer.as_mut_ptr(),
						&mut used,
					)
				};
				if ok == 0 {
					match unsafe { GetLastError() } {
						ERROR_INSUFFICIENT_BUFFER if used as usize > buffer.len() => {
							buffer.resize(used as usize, 0);
							continue;
						}
						ERROR_EVT_UNRESOLVED_VALUE_INSERT
						| ERROR_EVT_UNRESOLVED_PARAMETER_INSERT
						| ERROR_EVT_MAX_INSERTS_REACHED => {}
						_ => return None,
					}
				}
				let length = buffer
					.iter()
					.take(used as usize)
					.position(|&unit| unit == 0)
					.unwrap_or((used as usize).min(buffer.len()));
				let text = String::from_utf16_lossy(&buffer[..length]);
				return (!text.trim().is_empty()).then_some(text);
			}
		}
	}

	fn variant_text(value: &EVT_VARIANT) -> Option<String> {
		// Arrays are not shown.
		if value.Type & EVT_VARIANT_TYPE_ARRAY != 0 {
			return None;
		}
		let kind = (value.Type & EVT_VARIANT_TYPE_MASK) as i32;
		unsafe {
			match kind {
				EvtVarTypeString => Some(from_wide_ptr(value.Anonymous.StringVal)),
				EvtVarTypeAnsiString => {
					let pointer = value.Anonymous.AnsiStringVal;
					(!pointer.is_null()).then(|| {
						std::ffi::CStr::from_ptr(pointer.cast())
							.to_string_lossy()
							.into_owned()
					})
				}
				EvtVarTypeSByte => Some(value.Anonymous.SByteVal.to_string()),
				EvtVarTypeInt16 => Some(value.Anonymous.Int16Val.to_string()),
				EvtVarTypeInt32 => Some(value.Anonymous.Int32Val.to_string()),
				EvtVarTypeInt64 => Some(value.Anonymous.Int64Val.to_string()),
				EvtVarTypeHexInt32 => Some(format!("0x{:x}", value.Anonymous.UInt32Val)),
				EvtVarTypeHexInt64 => Some(format!("0x{:x}", value.Anonymous.UInt64Val)),
				_ => variant_u64(value).map(|number| number.to_string()),
			}
		}
	}

	fn variant_u64(value: &EVT_VARIANT) -> Option<u64> {
		if value.Type & EVT_VARIANT_TYPE_ARRAY != 0 {
			return None;
		}
		let kind = (value.Type & EVT_VARIANT_TYPE_MASK) as i32;
		unsafe {
			match kind {
				EvtVarTypeByte => Some(value.Anonymous.ByteVal.into()),
				EvtVarTypeUInt16 => Some(value.Anonymous.UInt16Val.into()),
				EvtVarTypeUInt32 => Some(value.Anonymous.UInt32Val.into()),
				EvtVarTypeUInt64 | EvtVarTypeFileTime => Some(value.Anonymous.UInt64Val),
				_ => None,
			}
		}
	}

	/// State of one followed channel.
	struct Channel {
		name: String,
		signal: Signal,
		/// Points at the newest entry seen; the subscription resumes after it.
		bookmark: EvtHandle,
		/// Whether `bookmark` points at an entry yet.
		bookmarked: bool,
		subscription: Option<EvtHandle>,
		/// When a broken subscription is opened again.
		retry_at: Option<Instant>,
	}

	impl Channel {
		fn new(name: String) -> io::Result<Self> {
			Ok(Self {
				name,
				signal: Signal::new()?,
				bookmark: EvtHandle::new(unsafe { EvtCreateBookmark(std::ptr::null()) })?,
				bookmarked: false,
				subscription: None,
				retry_at: None,
			})
		}

		fn mark(&mut self, event: &EvtHandle) {
			if unsafe { EvtUpdateBookmark(self.bookmark.0, event.0) } != 0 {
				self.bookmarked = true;
			}
		}

		/// Reads up to `count` of the newest entries, newest first, and
		/// bookmarks the newest.
		fn read_backlog(
			&mut self,
			renderer: &mut Renderer,
			count: usize,
		) -> io::Result<Vec<EventRecord>> {
			let path = wide(&self.name);
			let query = wide("*");
			let result_set = EvtHandle::new(unsafe {
				EvtQuery(
					0,
					path.as_ptr(),
					query.as_ptr(),
					EvtQueryChannelPath | EvtQueryReverseDirection,
				)
			})?;
			let mut records = Vec::new();
			while records.len() < count {
				let events = match next_batch(&result_set, INFINITE) {
					Batch::Events(events) => events,
					Batch::Empty => break,
					Batch::Failed(error) => return Err(error),
				};
				for event in events.iter().take(count - records.len()) {
					if records.is_empty() {
						self.mark(event);
					}
					records.push(renderer.render(event, &self.name));
				}
			}
			Ok(records)
		}

		/// Opens the subscription: right after the bookmark if there is one
		/// (strictly, so a lost bookmark never replays the whole log),
		/// otherwise for new entries only.
		fn subscribe(&mut self, context: &SourceContext) -> io::Result<()> {
			self.subscription = None;
			let path = wide(&self.name);
			let query = wide("*");
			let open = |bookmark: EVT_HANDLE, flags: u32| {
				EvtHandle::new(unsafe {
					EvtSubscribe(
						0,
						self.signal.0,
						path.as_ptr(),
						query.as_ptr(),
						bookmark,
						std::ptr::null(),
						None,
						flags,
					)
				})
			};
			let subscription = if self.bookmarked {
				match open(
					self.bookmark.0,
					EvtSubscribeStartAfterBookmark | EvtSubscribeStrict,
				) {
					Ok(subscription) => subscription,
					Err(_) => {
						let subscription = open(0, EvtSubscribeToFutureEvents)?;
						context.report_error(&format!(
							"eventlog {}: bookmark lost, some events may be missing",
							self.name
						));
						subscription
					}
				}
			} else {
				open(0, EvtSubscribeToFutureEvents)?
			};
			self.subscription = Some(subscription);
			Ok(())
		}

		/// Sends all pending live entries. Returns `false` once the main
		/// loop is gone.
		fn drain(&mut self, renderer: &mut Renderer, context: &SourceContext) -> bool {
			// Reset before reading, so a signal for an entry that arrives
			// while draining is not lost.
			self.signal.reset();
			loop {
				let Some(subscription) = &self.subscription else {
					return true;
				};
				let events = match next_batch(subscription, 0) {
					Batch::Events(events) => events,
					Batch::Empty => return true,
					Batch::Failed(error) => {
						self.subscription = None;
						self.retry_at = Some(Instant::now() + RESUBSCRIBE_DELAY);
						let delay = humantime::format_duration(RESUBSCRIBE_DELAY);
						return context.report_error(&format!(
							"eventlog {}: {error}, resubscribing in {delay}",
							self.name
						));
					}
				};
				for event in &events {
					let record = renderer.render(event, &self.name);
					self.mark(event);
					if !send_record(context, &record, false) {
						return false;
					}
				}
			}
		}
	}

	/// Sends a record as a preload or live line.
	fn send_record(context: &SourceContext, record: &EventRecord, preload: bool) -> bool {
		let ts = filetime_to_local(record.filetime).unwrap_or_else(Local::now);
		let text = format_line(record);
		if preload {
			context.send_preload(ts, &text)
		} else {
			context.send_live(ts, &text)
		}
	}

	pub fn run(channel_names: Vec<String>, context: SourceContext, preload: usize) {
		let mut renderer = match Renderer::new() {
			Ok(renderer) => renderer,
			Err(error) => {
				context.report_error(&format!("eventlog: cannot render events: {error}"));
				context.preload_done();
				return;
			}
		};

		let mut channels = Vec::new();
		let mut backlogs = Vec::new();
		for name in channel_names {
			let mut channel = match Channel::new(name.clone()) {
				Ok(channel) => channel,
				Err(error) => {
					if !context.report_error(&format!("eventlog {name}: {error}")) {
						return;
					}
					continue;
				}
			};
			if preload > 0 {
				match channel.read_backlog(&mut renderer, preload) {
					Ok(records) => backlogs.push(records),
					Err(error) => {
						if !context.report_error(&format!("eventlog {name}: {error}")) {
							return;
						}
						continue;
					}
				}
			}
			channels.push(channel);
		}

		for record in merge_backlog(backlogs, preload) {
			if !send_record(&context, &record, true) {
				return;
			}
		}
		if !context.preload_done() {
			return;
		}

		channels.retain_mut(|channel| match channel.subscribe(&context) {
			Ok(()) => true,
			Err(error) => {
				context.report_error(&format!("eventlog {}: {error}", channel.name));
				false
			}
		});
		if channels.is_empty() {
			return;
		}

		let signals: Vec<HANDLE> = channels.iter().map(|channel| channel.signal.0).collect();
		loop {
			let woken = unsafe {
				WaitForMultipleObjects(signals.len() as u32, signals.as_ptr(), 0, WAIT_TIMEOUT_MS)
			};
			if woken == WAIT_FAILED {
				let error = io::Error::last_os_error();
				context.report_error(&format!("eventlog: cannot wait for events: {error}"));
				thread::sleep(RESUBSCRIBE_DELAY);
			}
			// Every channel is drained on every wake: cheap, and it also
			// picks up an entry whose signal was missed.
			let now = Instant::now();
			for channel in &mut channels {
				if channel.retry_at.is_some_and(|retry_at| retry_at <= now) {
					channel.retry_at = None;
					if let Err(error) = channel.subscribe(&context) {
						channel.retry_at = Some(now + RESUBSCRIBE_DELAY);
						if !context.report_error(&format!("eventlog {}: {error}", channel.name)) {
							return;
						}
					}
				}
				if !channel.drain(&mut renderer, &context) {
					return;
				}
			}
		}
	}

	#[cfg(test)]
	mod tests {
		use std::sync::mpsc;
		use std::time::Duration;

		use super::*;
		use crate::line::SourceMsg;

		#[test]
		fn reads_application_backlog() {
			let (sender, receiver) = mpsc::sync_channel(64);
			let context = SourceContext {
				id: 0,
				label: None,
				ascii_only: false,
				sender,
			};
			thread::spawn(move || run(vec!["Application".into()], context, 3));
			let mut preload_lines = Vec::new();
			loop {
				match receiver.recv_timeout(Duration::from_secs(10)).unwrap() {
					SourceMsg::Preload(line) => preload_lines.push(line.text.to_string()),
					SourceMsg::PreloadDone(_) => break,
					SourceMsg::Live(line) => panic!("unexpected line: {}", line.text),
				}
			}
			assert!(preload_lines.len() <= 3, "{preload_lines:#?}");
			for line in &preload_lines {
				assert!(line.starts_with("Application "), "{line}");
			}
			// The subscription must survive empty reads: no error lines
			// for a few wakes of the follow loop.
			let deadline = std::time::Instant::now() + Duration::from_secs(3);
			while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
				match receiver.recv_timeout(left) {
					Ok(SourceMsg::Live(line)) => {
						assert!(!line.text.starts_with("lmatrix:"), "{}", line.text)
					}
					Ok(_) => {}
					Err(_) => break,
				}
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use chrono::TimeZone;

	fn record(channel: &str, filetime: u64, record_id: u64) -> EventRecord {
		EventRecord {
			channel: channel.into(),
			provider: "Service Control Manager".into(),
			event_id: 7036,
			level: 4,
			record_id,
			filetime,
			message: Some("x".into()),
			data: Vec::new(),
		}
	}

	#[test]
	fn filetime_conversion() {
		let filetime = FILETIME_UNIX_EPOCH + 1_700_000_000 * 10_000_000 + 1_234_560;
		let expected = Local.timestamp_opt(1_700_000_000, 123_456_000).unwrap();
		assert_eq!(filetime_to_local(filetime), Some(expected));
		assert_eq!(filetime_to_local(0), None);
	}

	#[test]
	fn level_names() {
		assert_eq!(level_name(1), "Critical");
		assert_eq!(level_name(2), "Error");
		assert_eq!(level_name(3), "Warning");
		assert_eq!(level_name(0), "Information");
		assert_eq!(level_name(4), "Information");
		assert_eq!(level_name(5), "Verbose");
		assert_eq!(level_name(17), "Level17");
	}

	#[test]
	fn line_with_message() {
		let mut event = record("System", 0, 1);
		event.level = 2;
		event.event_id = 7031;
		event.message = Some(
			"The Print Spooler service terminated unexpectedly.\r\n\r\n  It has done this 1 time(s)."
				.into(),
		);
		assert_eq!(
			format_line(&event),
			"System Error Service Control Manager[7031]: The Print Spooler service terminated \
			 unexpectedly. It has done this 1 time(s)."
		);
	}

	#[test]
	fn line_falls_back_to_data() {
		let mut event = record("Application", 0, 1);
		event.provider = "Gone".into();
		event.event_id = 1;
		event.level = 3;
		event.message = None;
		event.data = vec!["disk full".into(), "C:\n".into()];
		assert_eq!(
			format_line(&event),
			"Application Warning Gone[1]: disk full, C:"
		);
		event.data.clear();
		event.message = Some("  \r\n".into());
		assert_eq!(
			format_line(&event),
			"Application Warning Gone[1]: (no message)"
		);
	}

	#[test]
	fn backlog_merge_keeps_newest_in_order() {
		let system = vec![record("System", 30, 3), record("System", 10, 1)];
		let application = vec![record("Application", 20, 9), record("Application", 5, 8)];
		let merged = merge_backlog(vec![system, application], 3);
		let order: Vec<(String, u64)> = merged
			.iter()
			.map(|event| (event.channel.clone(), event.filetime))
			.collect();
		assert_eq!(
			order,
			vec![
				("System".to_string(), 10),
				("Application".to_string(), 20),
				("System".to_string(), 30)
			]
		);
		assert!(merge_backlog(Vec::new(), 5).is_empty());
	}
}
