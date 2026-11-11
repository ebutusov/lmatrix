//! `file` source: follows every file matching a glob, like `tail -F`.
//!
//! One thread polls all files of the source. [`FileSource`] and
//! [`FollowedFile`] do the work without sleeping or sending, so tests can
//! drive them step by step.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use chrono::Local;

use super::{MAX_LINE_BYTES, SourceContext, bytes_to_line};

/// Pause between checks for new data.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Pause between evaluations of the glob.
const RESCAN_INTERVAL: Duration = Duration::from_secs(5);

/// Number of polls a followed path may be missing before it counts as gone
/// **(assumption)**: gives log rotation (rename, then create) about a second
/// to put the new file in place without reporting a disappearance.
const MISSING_POLLS_BEFORE_GONE: u32 = 4;

/// Size of the chunks read backwards when looking for the last lines.
const TAIL_CHUNK_BYTES: u64 = 64 * 1024;

/// Most bytes read from the end of a file for the preload, so a huge file
/// without newlines is not read completely.
const MAX_TAIL_BYTES: u64 = 16 * 1024 * 1024;

/// Something the file source wants to send.
#[derive(Debug, Clone, PartialEq)]
pub enum FileOutput {
	Preload(String),
	Live(String),
	Error(String),
}

/// The last lines of a file and the state needed to follow it from the end.
#[derive(Debug, Default, PartialEq)]
pub struct Tail {
	/// Complete lines, oldest first.
	pub lines: Vec<String>,
	/// Bytes after the last newline (an unfinished last line).
	pub partial: Vec<u8>,
	/// Size of the file when it was read.
	pub end_offset: u64,
}

/// Reads the last `count` complete lines of `file` by reading backwards
/// from its end in chunks.
pub fn read_tail(file: &mut File, count: usize) -> io::Result<Tail> {
	let end_offset = file.metadata()?.len();
	let mut start = end_offset;
	// Bytes from `start` to `end_offset`.
	let mut data: Vec<u8> = Vec::new();
	let mut newlines = 0usize;
	while start > 0 && newlines <= count && end_offset - start < MAX_TAIL_BYTES {
		let chunk_len = TAIL_CHUNK_BYTES.min(start);
		start -= chunk_len;
		let mut chunk = vec![0u8; chunk_len as usize];
		file.seek(SeekFrom::Start(start))?;
		file.read_exact(&mut chunk)?;
		newlines += chunk.iter().filter(|&&byte| byte == b'\n').count();
		chunk.extend_from_slice(&data);
		data = chunk;
	}

	let (mut pieces, partial): (Vec<&[u8]>, Vec<u8>) =
		match data.iter().rposition(|&byte| byte == b'\n') {
			Some(last_newline) => (
				data[..last_newline].split(|&byte| byte == b'\n').collect(),
				data[last_newline + 1..].to_vec(),
			),
			None => (Vec::new(), data.clone()),
		};
	if start > 0 && !pieces.is_empty() {
		// The first piece may be the end of a line that starts earlier.
		pieces.remove(0);
	}
	let skip = pieces.len().saturating_sub(count);
	let lines = pieces[skip..]
		.iter()
		.map(|piece| bytes_to_line(piece))
		.collect();
	Ok(Tail {
		lines,
		partial,
		end_offset,
	})
}

/// What happened when a followed file was polled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollStatus {
	/// Still followed.
	Active,
	/// The path has been missing for a while; stop following it.
	Gone,
}

/// One file being followed.
pub struct FollowedFile {
	path: PathBuf,
	file: File,
	inode: u64,
	offset: u64,
	partial: Vec<u8>,
	missing_polls: u32,
}

impl FollowedFile {
	/// Opens `path` to be followed from its start.
	pub fn open_from_start(path: &Path) -> io::Result<Self> {
		let file = File::open(path)?;
		let inode = file.metadata()?.ino();
		Ok(Self {
			path: path.to_path_buf(),
			file,
			inode,
			offset: 0,
			partial: Vec::new(),
			missing_polls: 0,
		})
	}

	/// Opens `path`, returns its last `count` lines and follows it from its
	/// end.
	pub fn open_with_tail(path: &Path, count: usize) -> io::Result<(Self, Vec<String>)> {
		let mut followed = Self::open_from_start(path)?;
		let tail = read_tail(&mut followed.file, count)?;
		followed.offset = tail.end_offset;
		followed.partial = tail.partial;
		Ok((followed, tail.lines))
	}

	pub fn path(&self) -> &Path {
		&self.path
	}

	/// Reads new data, appending completed lines to `lines`. Handles
	/// rotation (the path now names another inode) and truncation (the file
	/// is shorter than what was read) by continuing at the start of the
	/// current file.
	pub fn poll(&mut self, lines: &mut Vec<String>) -> PollStatus {
		let path_metadata = match fs::metadata(&self.path) {
			Ok(metadata) => metadata,
			Err(_) => {
				// Gone or unreadable for now: keep draining the open file.
				self.read_new_data(lines);
				self.missing_polls += 1;
				if self.missing_polls >= MISSING_POLLS_BEFORE_GONE {
					self.flush_partial(lines);
					return PollStatus::Gone;
				}
				return PollStatus::Active;
			}
		};
		self.missing_polls = 0;

		if path_metadata.ino() != self.inode {
			// Rotated: finish the old file, then switch to the new one.
			self.read_new_data(lines);
			self.flush_partial(lines);
			match File::open(&self.path) {
				Ok(file) => {
					self.inode = file
						.metadata()
						.map(|metadata| metadata.ino())
						.unwrap_or(path_metadata.ino());
					self.file = file;
					self.offset = 0;
				}
				Err(_) => return PollStatus::Active,
			}
		} else if let Ok(metadata) = self.file.metadata()
			&& metadata.len() < self.offset
		{
			// Truncated: start over.
			self.flush_partial(lines);
			self.offset = 0;
		}
		self.read_new_data(lines);
		PollStatus::Active
	}

	/// Reads from `offset` to the end of the open file.
	fn read_new_data(&mut self, lines: &mut Vec<String>) {
		if self.file.seek(SeekFrom::Start(self.offset)).is_err() {
			return;
		}
		let mut buffer = [0u8; 64 * 1024];
		loop {
			match self.file.read(&mut buffer) {
				Ok(0) => break,
				Ok(read) => {
					self.offset += read as u64;
					self.take_lines(&buffer[..read], lines);
				}
				Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
				Err(_) => break,
			}
		}
	}

	/// Splits `data` into lines, keeping an unfinished last line buffered.
	fn take_lines(&mut self, data: &[u8], lines: &mut Vec<String>) {
		let mut rest = data;
		while let Some(newline) = rest.iter().position(|&byte| byte == b'\n') {
			self.partial.extend_from_slice(&rest[..newline]);
			lines.push(bytes_to_line(&self.partial));
			self.partial.clear();
			rest = &rest[newline + 1..];
		}
		self.partial.extend_from_slice(rest);
		if self.partial.len() > MAX_LINE_BYTES {
			// A line this long is cut anyway; do not buffer without bound.
			self.flush_partial(lines);
		}
	}

	/// Emits a buffered unfinished line, if any.
	fn flush_partial(&mut self, lines: &mut Vec<String>) {
		if !self.partial.is_empty() {
			lines.push(bytes_to_line(&self.partial));
			self.partial.clear();
		}
	}
}

/// All files of one `file` source.
pub struct FileSource {
	pattern: String,
	followed: BTreeMap<PathBuf, FollowedFile>,
	/// Paths whose open failure has been reported already.
	reported_open_errors: HashSet<PathBuf>,
}

impl FileSource {
	pub fn new(pattern: impl Into<String>) -> Self {
		Self {
			pattern: pattern.into(),
			followed: BTreeMap::new(),
			reported_open_errors: HashSet::new(),
		}
	}

	/// Paths currently followed.
	pub fn followed_paths(&self) -> impl Iterator<Item = &Path> {
		self.followed.keys().map(PathBuf::as_path)
	}

	/// Regular files matching the glob, or an error for an invalid pattern.
	fn matching_files(&self) -> Result<Vec<PathBuf>, String> {
		let paths = glob::glob(&self.pattern).map_err(|error| error.to_string())?;
		Ok(paths
			.filter_map(Result::ok)
			.filter(|path| path.is_file())
			.collect())
	}

	/// First scan: the last `preload` lines of each matching file become
	/// preload output, then the files are followed from their end.
	/// Returns `false` if the pattern is invalid (the source cannot work).
	pub fn start(&mut self, preload: usize, output: &mut Vec<FileOutput>) -> bool {
		let paths = match self.matching_files() {
			Ok(paths) => paths,
			Err(error) => {
				output.push(FileOutput::Error(format!(
					"file source {}: invalid pattern: {error}",
					self.pattern
				)));
				return false;
			}
		};
		if paths.is_empty() {
			output.push(FileOutput::Error(format!(
				"file source {}: no matching files",
				self.pattern
			)));
		}
		for path in paths {
			match FollowedFile::open_with_tail(&path, preload) {
				Ok((followed, lines)) => {
					output.extend(lines.into_iter().map(FileOutput::Preload));
					self.followed.insert(path, followed);
				}
				Err(error) => self.report_open_error(&path, &error, output),
			}
		}
		true
	}

	/// Re-evaluates the glob; files not followed yet are new and are read
	/// from their start.
	pub fn rescan(&mut self, output: &mut Vec<FileOutput>) {
		let Ok(paths) = self.matching_files() else {
			return;
		};
		for path in paths {
			if self.followed.contains_key(&path) {
				continue;
			}
			match FollowedFile::open_from_start(&path) {
				Ok(followed) => {
					self.reported_open_errors.remove(&path);
					self.followed.insert(path, followed);
				}
				Err(error) => self.report_open_error(&path, &error, output),
			}
		}
	}

	/// Polls every followed file for new lines.
	pub fn poll(&mut self, output: &mut Vec<FileOutput>) {
		let mut lines = Vec::new();
		let mut gone = Vec::new();
		for (path, followed) in &mut self.followed {
			if followed.poll(&mut lines) == PollStatus::Gone {
				gone.push(path.clone());
			}
			output.extend(lines.drain(..).map(FileOutput::Live));
		}
		for path in gone {
			self.followed.remove(&path);
			output.push(FileOutput::Error(format!(
				"file {} disappeared",
				path.display()
			)));
		}
	}

	fn report_open_error(&mut self, path: &Path, error: &io::Error, output: &mut Vec<FileOutput>) {
		if self.reported_open_errors.insert(path.to_path_buf()) {
			output.push(FileOutput::Error(format!(
				"cannot open file {}: {error}",
				path.display()
			)));
		}
	}
}

/// Sends collected output. Returns `false` once the main loop is gone.
fn send_all(context: &SourceContext, output: &mut Vec<FileOutput>) -> bool {
	for item in output.drain(..) {
		let sent = match item {
			FileOutput::Preload(line) => context.send_preload(Local::now(), &line),
			FileOutput::Live(line) => context.send_live(Local::now(), &line),
			FileOutput::Error(description) => context.report_error(&description),
		};
		if !sent {
			return false;
		}
	}
	true
}

/// Body of the source thread.
pub fn run(pattern: String, context: SourceContext, preload: usize) {
	let mut source = FileSource::new(pattern);
	let mut output = Vec::new();
	let usable = source.start(preload, &mut output);
	let sent = send_all(&context, &mut output);
	if !context.preload_done() || !sent || !usable {
		return;
	}
	let mut last_scan = Instant::now();
	loop {
		thread::sleep(POLL_INTERVAL);
		if last_scan.elapsed() >= RESCAN_INTERVAL {
			source.rescan(&mut output);
			last_scan = Instant::now();
		}
		source.poll(&mut output);
		if !send_all(&context, &mut output) {
			return;
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::io::Write;

	/// A scratch directory under the system temp dir, removed on drop.
	struct TempDir(PathBuf);

	impl TempDir {
		fn new(name: &str) -> Self {
			let path = std::env::temp_dir().join(format!(
				"lmatrix-test-{name}-{}-{}",
				std::process::id(),
				fastrand::u64(..)
			));
			fs::create_dir_all(&path).unwrap();
			Self(path)
		}

		fn file(&self, name: &str) -> PathBuf {
			self.0.join(name)
		}
	}

	impl Drop for TempDir {
		fn drop(&mut self) {
			let _ = fs::remove_dir_all(&self.0);
		}
	}

	fn append(path: &Path, text: &str) {
		let mut file = fs::OpenOptions::new()
			.create(true)
			.append(true)
			.open(path)
			.unwrap();
		file.write_all(text.as_bytes()).unwrap();
	}

	fn tail_of(path: &Path, count: usize) -> Tail {
		read_tail(&mut File::open(path).unwrap(), count).unwrap()
	}

	#[test]
	fn tail_of_small_file() {
		let dir = TempDir::new("tail-small");
		let path = dir.file("log");
		append(&path, "one\ntwo\r\nthree\nfour\n");
		let tail = tail_of(&path, 2);
		assert_eq!(tail.lines, vec!["three", "four"]);
		assert!(tail.partial.is_empty());
		assert_eq!(tail.end_offset, 20);
		assert_eq!(
			tail_of(&path, 10).lines,
			vec!["one", "two", "three", "four"]
		);
		assert!(tail_of(&path, 0).lines.is_empty());
	}

	#[test]
	fn tail_keeps_unfinished_line() {
		let dir = TempDir::new("tail-partial");
		let path = dir.file("log");
		append(&path, "one\ntwo\nthr");
		let tail = tail_of(&path, 5);
		assert_eq!(tail.lines, vec!["one", "two"]);
		assert_eq!(tail.partial, b"thr");

		let only_partial = dir.file("partial");
		append(&only_partial, "no newline");
		let tail = tail_of(&only_partial, 5);
		assert!(tail.lines.is_empty());
		assert_eq!(tail.partial, b"no newline");

		let empty = dir.file("empty");
		append(&empty, "");
		assert_eq!(tail_of(&empty, 5), Tail::default());
	}

	#[test]
	fn tail_keeps_empty_lines() {
		let dir = TempDir::new("tail-empty-lines");
		let path = dir.file("log");
		append(&path, "\n\na\n\n");
		assert_eq!(tail_of(&path, 10).lines, vec!["", "", "a", ""]);
	}

	#[test]
	fn tail_of_file_larger_than_a_chunk() {
		let dir = TempDir::new("tail-large");
		let path = dir.file("log");
		let mut content = String::new();
		for index in 0..50_000 {
			content.push_str(&format!("line number {index}\n"));
		}
		append(&path, &content);
		let tail = tail_of(&path, 3);
		assert_eq!(
			tail.lines,
			vec![
				"line number 49997",
				"line number 49998",
				"line number 49999"
			]
		);
		let tail = tail_of(&path, 20_000);
		assert_eq!(tail.lines.len(), 20_000);
		assert_eq!(tail.lines[0], "line number 30000");
	}

	#[test]
	fn follows_appended_lines() {
		let dir = TempDir::new("follow-append");
		let path = dir.file("log");
		append(&path, "old\npart");
		let (mut followed, preload) = FollowedFile::open_with_tail(&path, 10).unwrap();
		assert_eq!(preload, vec!["old"]);

		let mut lines = Vec::new();
		assert_eq!(followed.poll(&mut lines), PollStatus::Active);
		assert!(lines.is_empty());

		append(&path, "ial\nnew\nunfin");
		followed.poll(&mut lines);
		assert_eq!(lines, vec!["partial", "new"]);
		lines.clear();

		append(&path, "ished\n");
		followed.poll(&mut lines);
		assert_eq!(lines, vec!["unfinished"]);
	}

	#[test]
	fn detects_truncation() {
		let dir = TempDir::new("follow-truncate");
		let path = dir.file("log");
		append(&path, "aaaaaaaaaa\nbbbbbbbbbb\n");
		let (mut followed, _) = FollowedFile::open_with_tail(&path, 10).unwrap();
		fs::write(&path, "short\n").unwrap();
		let mut lines = Vec::new();
		followed.poll(&mut lines);
		assert_eq!(lines, vec!["short"]);
	}

	#[test]
	fn detects_rotation() {
		let dir = TempDir::new("follow-rotate");
		let path = dir.file("log");
		append(&path, "before\n");
		let (mut followed, _) = FollowedFile::open_with_tail(&path, 10).unwrap();
		append(&path, "last old line\n");
		fs::rename(&path, dir.file("log.1")).unwrap();
		append(&path, "first new line\n");

		let mut lines = Vec::new();
		followed.poll(&mut lines);
		assert_eq!(lines, vec!["last old line", "first new line"]);
		lines.clear();
		append(&path, "second new line\n");
		followed.poll(&mut lines);
		assert_eq!(lines, vec!["second new line"]);
	}

	#[test]
	fn reports_disappearance_after_grace() {
		let dir = TempDir::new("source-gone");
		let path = dir.file("app.log");
		append(&path, "x\n");
		let mut source = FileSource::new(dir.file("*.log").to_string_lossy());
		let mut output = Vec::new();
		assert!(source.start(10, &mut output));
		assert_eq!(output, vec![FileOutput::Preload("x".into())]);
		output.clear();

		fs::remove_file(&path).unwrap();
		for _ in 1..MISSING_POLLS_BEFORE_GONE {
			source.poll(&mut output);
			assert!(output.is_empty());
		}
		source.poll(&mut output);
		assert_eq!(
			output,
			vec![FileOutput::Error(format!(
				"file {} disappeared",
				path.display()
			))]
		);
		assert_eq!(source.followed_paths().count(), 0);
	}

	#[test]
	fn reports_no_match_and_picks_up_new_files_from_start() {
		let dir = TempDir::new("source-rescan");
		let pattern = dir.file("*.log").to_string_lossy().into_owned();
		let mut source = FileSource::new(pattern.clone());
		let mut output = Vec::new();
		assert!(source.start(10, &mut output));
		assert_eq!(
			output,
			vec![FileOutput::Error(format!(
				"file source {pattern}: no matching files"
			))]
		);
		output.clear();

		append(&dir.file("new.log"), "first\nsecond\n");
		append(&dir.file("ignored.txt"), "nope\n");
		fs::create_dir(dir.file("dir.log")).unwrap();
		source.rescan(&mut output);
		source.poll(&mut output);
		assert_eq!(
			output,
			vec![
				FileOutput::Live("first".into()),
				FileOutput::Live("second".into())
			]
		);
		assert_eq!(source.followed_paths().count(), 1);
	}
}
