//! Highlight rules: first match wins.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use regex::{Regex, RegexSet};

use crate::term::Rgb;

/// Index of a rule in the rule list.
pub type RuleId = usize;

/// What happens to a line matched by a rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
	/// Color only.
	None,
	/// Decode and keep on screen for this long.
	For(Duration),
	/// Decode and keep on screen until acknowledged.
	UntilAck,
}

impl Hold {
	/// Parses `"ack"`, `"0s"` or a duration such as `"30s"` or `"5m"`.
	pub fn parse(text: &str) -> Result<Hold> {
		let trimmed = text.trim();
		if trimmed.eq_ignore_ascii_case("ack") {
			return Ok(Hold::UntilAck);
		}
		let duration = humantime::parse_duration(trimmed).map_err(|error| {
			anyhow!(
				"invalid hold '{text}': expected \"ack\" or a duration such as \"30s\" ({error})"
			)
		})?;
		if duration.is_zero() {
			Ok(Hold::None)
		} else {
			Ok(Hold::For(duration))
		}
	}

	pub fn decodes(self) -> bool {
		self != Hold::None
	}
}

#[derive(Debug, Clone)]
pub struct Rule {
	pub name: String,
	pub regex: Regex,
	pub color: Rgb,
	pub hold: Hold,
}

impl Rule {
	/// Builds a rule; errors name the rule.
	pub fn new(name: &str, pattern: &str, color: &str, hold: Option<&str>) -> Result<Rule> {
		let regex =
			Regex::new(pattern).with_context(|| format!("rule '{name}': invalid pattern"))?;
		let color = color
			.parse::<Rgb>()
			.map_err(|error| anyhow!("rule '{name}': color: {error}"))?;
		let hold = match hold {
			Some(text) => Hold::parse(text).with_context(|| format!("rule '{name}': hold"))?,
			None => Hold::None,
		};
		Ok(Rule {
			name: name.to_string(),
			regex,
			color,
			hold,
		})
	}
}

/// The configured rules, matched in one pass.
#[derive(Debug, Clone)]
pub struct RuleSet {
	rules: Vec<Rule>,
	set: RegexSet,
}

impl Default for RuleSet {
	fn default() -> Self {
		Self::new(Vec::new()).expect("empty rule set is valid")
	}
}

impl RuleSet {
	pub fn new(rules: Vec<Rule>) -> Result<RuleSet> {
		let set = RegexSet::new(rules.iter().map(|rule| rule.regex.as_str()));
		let set = match set {
			Ok(set) => set,
			Err(error) => bail!("rules: {error}"),
		};
		Ok(RuleSet { rules, set })
	}

	/// The first rule (in config order) matching `text`.
	pub fn first_match(&self, text: &str) -> Option<RuleId> {
		if self.rules.is_empty() {
			return None;
		}
		self.set.matches(text).iter().next()
	}

	pub fn get(&self, id: RuleId) -> Option<&Rule> {
		self.rules.get(id)
	}

	pub fn len(&self) -> usize {
		self.rules.len()
	}

	pub fn is_empty(&self) -> bool {
		self.rules.is_empty()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn first_match_wins() {
		let rules = RuleSet::new(vec![
			Rule::new("a", "error", "#ff0000", Some("ack")).unwrap(),
			Rule::new("b", "err", "#00ff00", None).unwrap(),
			Rule::new("c", "(?i)WARN", "#0000ff", Some("10s")).unwrap(),
		])
		.unwrap();
		assert_eq!(rules.first_match("an error here"), Some(0));
		assert_eq!(rules.first_match("err only"), Some(1));
		assert_eq!(rules.first_match("warn and error"), Some(0));
		assert_eq!(rules.first_match("a warning"), Some(2));
		assert_eq!(rules.first_match("nothing"), None);
		assert_eq!(RuleSet::default().first_match("anything"), None);
	}

	#[test]
	fn case_sensitive_by_default() {
		let rules = RuleSet::new(vec![Rule::new("a", "Error", "#ff0000", None).unwrap()]).unwrap();
		assert_eq!(rules.first_match("error"), None);
		assert_eq!(rules.first_match("Error"), Some(0));
	}

	#[test]
	fn hold_forms_parse() {
		assert_eq!(Hold::parse("ack").unwrap(), Hold::UntilAck);
		assert_eq!(Hold::parse("0s").unwrap(), Hold::None);
		assert_eq!(
			Hold::parse("30s").unwrap(),
			Hold::For(Duration::from_secs(30))
		);
		assert_eq!(
			Hold::parse("5m").unwrap(),
			Hold::For(Duration::from_secs(300))
		);
		assert!(Hold::parse("soon").is_err());
		assert_eq!(
			Rule::new("x", "x", "#000000", None).unwrap().hold,
			Hold::None
		);
	}

	#[test]
	fn invalid_regex_names_rule() {
		let error = Rule::new("broken-rule", "(unclosed", "#ff0000", None).unwrap_err();
		assert!(format!("{error:#}").contains("broken-rule"));
	}

	#[test]
	fn invalid_color_names_rule() {
		let error = Rule::new("bad-color", "x", "red", None).unwrap_err();
		assert!(format!("{error:#}").contains("bad-color"));
	}
}
