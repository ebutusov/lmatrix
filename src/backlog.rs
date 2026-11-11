//! Bounded ring buffer of received lines.

use std::collections::VecDeque;

use crate::line::LogLine;
use crate::rules::RuleId;

/// A backlog entry: the line and the rule it matched.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
	pub line: LogLine,
	pub rule: Option<RuleId>,
}

/// Keeps the newest `capacity` entries. Every entry gets a sequence number
/// that increases by one per pushed entry, so views can track positions
/// while old entries are discarded.
#[derive(Debug, Clone)]
pub struct Backlog {
	entries: VecDeque<Entry>,
	capacity: usize,
	/// Sequence number of the front entry.
	first_seq: u64,
}

impl Backlog {
	pub fn new(capacity: usize) -> Self {
		let capacity = capacity.max(1);
		Self {
			entries: VecDeque::with_capacity(capacity.min(65536)),
			capacity,
			first_seq: 0,
		}
	}

	/// Appends an entry, discarding the oldest one when full.
	pub fn push(&mut self, entry: Entry) {
		if self.entries.len() == self.capacity {
			self.entries.pop_front();
			self.first_seq += 1;
		}
		self.entries.push_back(entry);
	}

	pub fn len(&self) -> usize {
		self.entries.len()
	}

	pub fn is_empty(&self) -> bool {
		self.entries.is_empty()
	}

	pub fn capacity(&self) -> usize {
		self.capacity
	}

	/// Sequence number of the oldest kept entry.
	pub fn first_seq(&self) -> u64 {
		self.first_seq
	}

	/// Sequence number the next pushed entry will get.
	pub fn end_seq(&self) -> u64 {
		self.first_seq + self.entries.len() as u64
	}

	/// Entry by position, 0 being the oldest.
	pub fn get(&self, index: usize) -> Option<&Entry> {
		self.entries.get(index)
	}

	/// Entry by sequence number, if still kept.
	pub fn get_seq(&self, seq: u64) -> Option<&Entry> {
		seq.checked_sub(self.first_seq)
			.and_then(|offset| self.entries.get(offset as usize))
	}

	pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Entry> + ExactSizeIterator {
		self.entries.iter()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use chrono::Local;

	fn entry(text: &str) -> Entry {
		Entry {
			line: LogLine::new(Local::now(), 0, text),
			rule: None,
		}
	}

	#[test]
	fn keeps_capacity_and_discards_oldest() {
		let mut backlog = Backlog::new(3);
		for text in ["a", "b", "c", "d", "e"] {
			backlog.push(entry(text));
		}
		assert_eq!(backlog.len(), 3);
		let texts: Vec<&str> = backlog.iter().map(|item| &*item.line.text).collect();
		assert_eq!(texts, ["c", "d", "e"]);
		assert_eq!(backlog.first_seq(), 2);
		assert_eq!(backlog.end_seq(), 5);
		assert_eq!(&*backlog.get_seq(3).unwrap().line.text, "d");
		assert!(backlog.get_seq(1).is_none());
		assert!(backlog.get_seq(5).is_none());
	}
}
