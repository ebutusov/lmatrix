//! Configuration file: parsing, defaults, validation.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use crate::rules::Rule;
use crate::sim::{ReplayWindow, SimSettings};
use crate::term::Rgb;

/// The commented default configuration printed by `--print-default-config`.
pub const DEFAULT_CONFIG: &str = include_str!("../lmatrix.example.toml");

/// Default `rain.strip_pattern`: everything up to the first `:` and the
/// spaces after it, typically `identifier[pid]: `.
pub const DEFAULT_STRIP_PATTERN: &str = r"^[^:]*:\s*";

/// Color mode selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorChoice {
	Auto,
	TrueColor,
	Palette256,
	Basic16,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DisplayConfig {
	pub fps: u32,
	pub color: ColorChoice,
	pub default_color: Rgb,
	pub ascii_only: bool,
	pub window_lines: u16,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RainConfig {
	pub speed: f32,
	pub speed_jitter: f32,
	pub fill: f32,
	pub fade_half_life: Duration,
	pub replay: bool,
	pub replay_brightness: f32,
	pub replay_reserve: f32,
	pub replay_window: ReplayWindow,
	pub min_brightness: f32,
	/// Regex whose first match is removed from rain text; `None` = off.
	pub strip_pattern: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TailConfig {
	pub backlog: usize,
	pub preload: usize,
}

/// A configured log source.
#[derive(Debug, Clone, PartialEq)]
pub enum SourceConfig {
	Journald {
		units: Vec<String>,
		/// Read the journal of this host over ssh instead of the local one.
		remote: Option<RemoteHost>,
		label: Option<String>,
		strip_pattern: Option<String>,
	},
	File {
		path: String,
		label: Option<String>,
		strip_pattern: Option<String>,
	},
	Command {
		run: String,
		restart: bool,
		restart_delay: Duration,
		label: Option<String>,
		strip_pattern: Option<String>,
	},
}

impl SourceConfig {
	pub fn label(&self) -> Option<&str> {
		match self {
			SourceConfig::Journald { label, .. }
			| SourceConfig::File { label, .. }
			| SourceConfig::Command { label, .. } => label.as_deref(),
		}
	}

	/// This source's own `strip_pattern`: `None` means `rain.strip_pattern`
	/// applies, `Some("")` turns stripping off for this source.
	pub fn strip_pattern(&self) -> Option<&str> {
		match self {
			SourceConfig::Journald { strip_pattern, .. }
			| SourceConfig::File { strip_pattern, .. }
			| SourceConfig::Command { strip_pattern, .. } => strip_pattern.as_deref(),
		}
	}

	pub fn default_journald() -> Self {
		SourceConfig::Journald {
			units: Vec::new(),
			remote: None,
			label: None,
			strip_pattern: None,
		}
	}
}

/// A remote host whose journal is read over ssh.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteHost {
	/// ssh destination, such as `admin@web1` or a `Host` alias.
	pub host: String,
	/// Extra ssh arguments, such as `["-C", "-p", "2222"]`.
	pub ssh_args: Vec<String>,
}

/// A validated configuration.
#[derive(Debug, Clone)]
pub struct Config {
	pub display: DisplayConfig,
	pub rain: RainConfig,
	pub tail: TailConfig,
	pub sources: Vec<SourceConfig>,
	pub rules: Vec<Rule>,
}

impl Default for Config {
	fn default() -> Self {
		Self {
			display: DisplayConfig {
				fps: 20,
				color: ColorChoice::Auto,
				default_color: Rgb(0x28, 0xff, 0x6e),
				ascii_only: false,
				window_lines: 2,
			},
			rain: RainConfig {
				speed: 16.0,
				speed_jitter: 0.35,
				fill: 0.35,
				fade_half_life: Duration::from_millis(480),
				replay: true,
				replay_brightness: 0.55,
				replay_reserve: 0.15,
				replay_window: ReplayWindow::Auto,
				min_brightness: 0.3,
				strip_pattern: Some(DEFAULT_STRIP_PATTERN.to_string()),
			},
			tail: TailConfig {
				backlog: 10_000,
				preload: 200,
			},
			sources: vec![SourceConfig::default_journald()],
			rules: Vec::new(),
		}
	}
}

impl Config {
	/// Fails when a source reads the local journal on a system that has
	/// none (Windows).
	pub fn check_platform(&self) -> Result<()> {
		self.check_local_journal(cfg!(unix))
	}

	fn check_local_journal(&self, available: bool) -> Result<()> {
		if available {
			return Ok(());
		}
		for (index, source) in self.sources.iter().enumerate() {
			if let SourceConfig::Journald { remote: None, .. } = source {
				bail!(
					"source[{index}]: this system has no local journal; set host to read a \
					 remote one over ssh (without any [[source]] section, a local journald \
					 source is used)"
				);
			}
		}
		Ok(())
	}

	/// Parses and validates configuration text.
	pub fn parse(text: &str) -> Result<Config> {
		let raw: RawConfig =
			toml::from_str(text).map_err(|error| anyhow!("{}", error.to_string().trim_end()))?;
		raw.validate()
	}

	/// Reads and validates a configuration file.
	pub fn load_file(path: &Path) -> Result<Config> {
		let text = std::fs::read_to_string(path)
			.with_context(|| format!("cannot read {}", path.display()))?;
		Config::parse(&text).with_context(|| format!("invalid config {}", path.display()))
	}

	/// Settings for the simulation.
	pub fn sim_settings(&self) -> SimSettings {
		let global_strip = self
			.rain
			.strip_pattern
			.as_deref()
			.map(|pattern| regex::Regex::new(pattern).expect("validated"));
		SimSettings {
			speed: self.rain.speed,
			speed_jitter: self.rain.speed_jitter,
			fill: self.rain.fill,
			fade_half_life: self.rain.fade_half_life,
			replay: self.rain.replay,
			replay_brightness: self.rain.replay_brightness,
			replay_reserve: self.rain.replay_reserve,
			replay_window: self.rain.replay_window,
			strip_pattern: global_strip.clone(),
			source_strip_patterns: self
				.sources
				.iter()
				.map(|source| match source.strip_pattern() {
					None => global_strip.clone(),
					Some("") => None,
					Some(pattern) => Some(regex::Regex::new(pattern).expect("validated")),
				})
				.collect(),
			default_color: self.display.default_color,
			backlog: self.tail.backlog,
			ascii_only: self.display.ascii_only,
		}
	}
}

/// Finds the configuration file: `explicit`, then the XDG location, then
/// `/etc/lmatrix/config.toml`. `Ok(None)` means built-in defaults.
pub fn locate(explicit: Option<&Path>) -> Result<Option<PathBuf>> {
	if let Some(path) = explicit {
		if !path.exists() {
			bail!("config file {} does not exist", path.display());
		}
		return Ok(Some(path.to_path_buf()));
	}
	Ok(default_config_paths()
		.into_iter()
		.find(|candidate| candidate.is_file()))
}

/// Config files looked for when none is given, in order.
#[cfg(unix)]
fn default_config_paths() -> Vec<PathBuf> {
	let xdg_dir = std::env::var_os("XDG_CONFIG_HOME")
		.filter(|value| !value.is_empty())
		.map(PathBuf::from)
		.or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
	xdg_dir
		.map(|dir| dir.join("lmatrix").join("config.toml"))
		.into_iter()
		.chain(std::iter::once(PathBuf::from("/etc/lmatrix/config.toml")))
		.collect()
}

/// Config files looked for when none is given: `%APPDATA%\lmatrix\config.toml`.
#[cfg(windows)]
fn default_config_paths() -> Vec<PathBuf> {
	std::env::var_os("APPDATA")
		.filter(|value| !value.is_empty())
		.map(|dir| PathBuf::from(dir).join("lmatrix").join("config.toml"))
		.into_iter()
		.collect()
}

// ---------------------------------------------------------------------------
// Raw (serde) structures

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawConfig {
	#[serde(default)]
	display: RawDisplay,
	#[serde(default)]
	rain: RawRain,
	#[serde(default)]
	tail: RawTail,
	source: Option<Vec<RawSource>>,
	#[serde(default)]
	rule: Vec<RawRule>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawDisplay {
	fps: Option<u32>,
	color: Option<StringOrInt>,
	default_color: Option<String>,
	ascii_only: Option<bool>,
	window_lines: Option<u16>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StringOrInt {
	Text(String),
	Number(i64),
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawRain {
	speed: Option<f32>,
	speed_jitter: Option<f32>,
	fill: Option<f32>,
	fade_half_life: Option<String>,
	replay: Option<bool>,
	replay_brightness: Option<f32>,
	replay_reserve: Option<f32>,
	replay_window: Option<StringOrInt>,
	min_brightness: Option<f32>,
	strip_pattern: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawTail {
	backlog: Option<usize>,
	preload: Option<usize>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum RawSource {
	Journald {
		#[serde(default)]
		units: Vec<String>,
		host: Option<String>,
		ssh_args: Option<Vec<String>>,
		label: Option<String>,
		strip_pattern: Option<String>,
	},
	File {
		path: String,
		label: Option<String>,
		strip_pattern: Option<String>,
	},
	Command {
		run: String,
		restart: Option<bool>,
		restart_delay: Option<String>,
		label: Option<String>,
		strip_pattern: Option<String>,
	},
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
	name: String,
	pattern: String,
	color: String,
	hold: Option<String>,
}

/// Validates the `host` and `ssh_args` keys of a journald source.
fn check_remote(
	key: &str,
	host: Option<String>,
	ssh_args: Option<Vec<String>>,
) -> Result<Option<RemoteHost>> {
	let Some(host) = host else {
		if ssh_args.is_some() {
			bail!("{key}.ssh_args: only allowed together with host");
		}
		return Ok(None);
	};
	if host.trim().is_empty() || host.chars().any(char::is_whitespace) {
		bail!("{key}.host: must be a single ssh destination such as 'admin@web1'");
	}
	if host.starts_with('-') {
		bail!("{key}.host: must not start with '-'");
	}
	let ssh_args = ssh_args.unwrap_or_default();
	if ssh_args.iter().any(|arg| arg.is_empty()) {
		bail!("{key}.ssh_args: arguments must not be empty");
	}
	Ok(Some(RemoteHost { host, ssh_args }))
}

fn check_range<T: PartialOrd + std::fmt::Display + Copy>(
	key: &str,
	value: T,
	min: T,
	max: T,
) -> Result<T> {
	if value < min || value > max {
		bail!("{key}: must be between {min} and {max}, got {value}");
	}
	Ok(value)
}

fn parse_duration(key: &str, text: &str) -> Result<Duration> {
	humantime::parse_duration(text.trim())
		.map_err(|error| anyhow!("{key}: invalid duration '{text}' ({error})"))
}

/// Checks a source's `strip_pattern`; `""` is kept and means off.
fn check_strip_pattern(key: &str, pattern: Option<String>) -> Result<Option<String>> {
	if let Some(text) = pattern.as_deref().filter(|text| !text.is_empty()) {
		regex::Regex::new(text).map_err(|error| anyhow!("{key}: invalid regex: {error}"))?;
	}
	Ok(pattern)
}

fn check_label(key: &str, label: Option<String>) -> Result<Option<String>> {
	match label {
		Some(text) if text.trim().is_empty() => bail!("{key}: must not be empty"),
		other => Ok(other),
	}
}

impl RawConfig {
	fn validate(self) -> Result<Config> {
		let defaults = Config::default();

		let display = {
			let raw = self.display;
			let color = match raw.color {
				None => defaults.display.color,
				Some(StringOrInt::Text(text)) => match text.as_str() {
					"auto" => ColorChoice::Auto,
					"truecolor" | "24bit" => ColorChoice::TrueColor,
					"256" => ColorChoice::Palette256,
					"16" => ColorChoice::Basic16,
					other => bail!(
						"display.color: must be one of auto, truecolor, 256, 16, got '{other}'"
					),
				},
				Some(StringOrInt::Number(256)) => ColorChoice::Palette256,
				Some(StringOrInt::Number(16)) => ColorChoice::Basic16,
				Some(StringOrInt::Number(other)) => {
					bail!("display.color: must be one of auto, truecolor, 256, 16, got {other}")
				}
			};
			DisplayConfig {
				fps: check_range(
					"display.fps",
					raw.fps.unwrap_or(defaults.display.fps),
					5,
					60,
				)?,
				color,
				default_color: match raw.default_color {
					Some(text) => text
						.parse::<Rgb>()
						.map_err(|error| anyhow!("display.default_color: {error}"))?,
					None => defaults.display.default_color,
				},
				ascii_only: raw.ascii_only.unwrap_or(defaults.display.ascii_only),
				window_lines: check_range(
					"display.window_lines",
					raw.window_lines.unwrap_or(defaults.display.window_lines),
					0,
					20,
				)?,
			}
		};

		let rain = {
			let raw = self.rain;
			let strip_pattern = match raw.strip_pattern {
				None => defaults.rain.strip_pattern,
				Some(pattern) if pattern.is_empty() => None,
				Some(pattern) => {
					regex::Regex::new(&pattern)
						.map_err(|error| anyhow!("rain.strip_pattern: invalid regex: {error}"))?;
					Some(pattern)
				}
			};
			let fade_half_life = match raw.fade_half_life {
				Some(text) => parse_duration("rain.fade_half_life", &text)?,
				None => defaults.rain.fade_half_life,
			};
			if fade_half_life < Duration::from_millis(10)
				|| fade_half_life > Duration::from_secs(60)
			{
				bail!("rain.fade_half_life: must be between 10ms and 60s");
			}
			RainConfig {
				speed: check_range(
					"rain.speed",
					raw.speed.unwrap_or(defaults.rain.speed),
					0.5,
					200.0,
				)?,
				speed_jitter: check_range(
					"rain.speed_jitter",
					raw.speed_jitter.unwrap_or(defaults.rain.speed_jitter),
					0.0,
					0.9,
				)?,
				fill: check_range(
					"rain.fill",
					raw.fill.unwrap_or(defaults.rain.fill),
					0.01,
					1.0,
				)?,
				fade_half_life,
				replay: raw.replay.unwrap_or(defaults.rain.replay),
				replay_brightness: check_range(
					"rain.replay_brightness",
					raw.replay_brightness
						.unwrap_or(defaults.rain.replay_brightness),
					0.1,
					1.0,
				)?,
				replay_reserve: check_range(
					"rain.replay_reserve",
					raw.replay_reserve.unwrap_or(defaults.rain.replay_reserve),
					0.0,
					1.0,
				)?,
				replay_window: match raw.replay_window {
					None => defaults.rain.replay_window,
					Some(StringOrInt::Text(text)) if text == "auto" => ReplayWindow::Auto,
					Some(StringOrInt::Text(other)) => {
						bail!("rain.replay_window: must be \"auto\" or a number, got '{other}'")
					}
					Some(StringOrInt::Number(lines)) => {
						ReplayWindow::Lines(
							check_range("rain.replay_window", lines, 1, 10_000_000)? as usize,
						)
					}
				},
				min_brightness: check_range(
					"rain.min_brightness",
					raw.min_brightness.unwrap_or(defaults.rain.min_brightness),
					0.0,
					1.0,
				)?,
				strip_pattern,
			}
		};

		let tail = TailConfig {
			backlog: check_range(
				"tail.backlog",
				self.tail.backlog.unwrap_or(defaults.tail.backlog),
				1,
				10_000_000,
			)?,
			preload: check_range(
				"tail.preload",
				self.tail.preload.unwrap_or(defaults.tail.preload),
				0,
				100_000,
			)?,
		};

		let sources = match self.source {
			None => defaults.sources,
			Some(raw_sources) => {
				let mut sources = Vec::with_capacity(raw_sources.len());
				for (index, raw) in raw_sources.into_iter().enumerate() {
					let key = format!("source[{index}]");
					sources.push(match raw {
						RawSource::Journald {
							units,
							host,
							ssh_args,
							label,
							strip_pattern,
						} => {
							if units.iter().any(|unit| unit.trim().is_empty()) {
								bail!("{key}.units: unit names must not be empty");
							}
							SourceConfig::Journald {
								units,
								remote: check_remote(&key, host, ssh_args)?,
								label: check_label(&format!("{key}.label"), label)?,
								strip_pattern: check_strip_pattern(
									&format!("{key}.strip_pattern"),
									strip_pattern,
								)?,
							}
						}
						RawSource::File {
							path,
							label,
							strip_pattern,
						} => {
							if path.trim().is_empty() {
								bail!("{key}.path: must not be empty");
							}
							glob::Pattern::new(&path).map_err(|error| {
								anyhow!("{key}.path: invalid glob '{path}' ({error})")
							})?;
							SourceConfig::File {
								path,
								label: check_label(&format!("{key}.label"), label)?,
								strip_pattern: check_strip_pattern(
									&format!("{key}.strip_pattern"),
									strip_pattern,
								)?,
							}
						}
						RawSource::Command {
							run,
							restart,
							restart_delay,
							label,
							strip_pattern,
						} => {
							if run.trim().is_empty() {
								bail!("{key}.run: must not be empty");
							}
							let restart_delay = match restart_delay {
								Some(text) => {
									parse_duration(&format!("{key}.restart_delay"), &text)?
								}
								None => Duration::from_secs(5),
							};
							if restart_delay < Duration::from_millis(100) {
								bail!("{key}.restart_delay: must be at least 100ms");
							}
							SourceConfig::Command {
								run,
								restart: restart.unwrap_or(true),
								restart_delay,
								label: check_label(&format!("{key}.label"), label)?,
								strip_pattern: check_strip_pattern(
									&format!("{key}.strip_pattern"),
									strip_pattern,
								)?,
							}
						}
					});
				}
				sources
			}
		};

		let mut rules = Vec::with_capacity(self.rule.len());
		for (index, raw) in self.rule.into_iter().enumerate() {
			if raw.name.trim().is_empty() {
				bail!("rule[{index}].name: must not be empty");
			}
			rules.push(Rule::new(
				&raw.name,
				&raw.pattern,
				&raw.color,
				raw.hold.as_deref(),
			)?);
		}

		Ok(Config {
			display,
			rain,
			tail,
			sources,
			rules,
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::rules::Hold;

	fn error_of(text: &str) -> String {
		format!("{:#}", Config::parse(text).unwrap_err())
	}

	#[test]
	fn empty_config_gives_defaults() {
		let config = Config::parse("").unwrap();
		let defaults = Config::default();
		assert_eq!(config.display, defaults.display);
		assert_eq!(config.rain, defaults.rain);
		assert_eq!(config.tail, defaults.tail);
		assert_eq!(config.sources, vec![SourceConfig::default_journald()]);
		assert!(config.rules.is_empty());
	}

	#[test]
	fn replay_window_accepts_auto_and_numbers() {
		let auto = Config::parse("[rain]\nreplay_window = \"auto\"\n").unwrap();
		assert_eq!(auto.rain.replay_window, ReplayWindow::Auto);
		let fixed = Config::parse("[rain]\nreplay_window = 250\n").unwrap();
		assert_eq!(fixed.rain.replay_window, ReplayWindow::Lines(250));
		assert_eq!(fixed.sim_settings().replay_window, ReplayWindow::Lines(250));
	}

	#[test]
	fn default_config_text_matches_defaults() {
		let config = Config::parse(DEFAULT_CONFIG).unwrap();
		let defaults = Config::default();
		assert_eq!(config.display, defaults.display);
		assert_eq!(config.rain, defaults.rain);
		assert_eq!(config.tail, defaults.tail);
		assert_eq!(config.sources, defaults.sources);
		assert!(config.rules.is_empty());
	}

	#[test]
	fn full_schema_parses() {
		let text = r##"
[display]
fps = 30
color = "256"
default_color = "#00ff00"
ascii_only = true
window_lines = 0

[rain]
speed = 10
speed_jitter = 0.2
fill = 0.5
fade_half_life = "1s"
replay = false
replay_brightness = 0.3
replay_reserve = 0.2

[tail]
backlog = 500
preload = 10

[[source]]
type = "journald"
units = ["ssh.service"]
label = "sys"

[[source]]
type = "file"
path = "/var/log/myapp/*.log"

[[source]]
type = "command"
run = "docker logs -f web"
restart = false
restart_delay = "2s"

[[rule]]
name = "oom"
pattern = "(?i)out of memory|oom-killer"
color = "#ff463c"
hold = "ack"

[[rule]]
name = "auth-fail"
pattern = "Failed password for \\S+"
color = "#ffb428"
hold = "30s"

[[rule]]
name = "login"
pattern = "Accepted publickey"
color = "#3cc8ff"
"##;
		let config = Config::parse(text).unwrap();
		assert_eq!(config.display.fps, 30);
		assert_eq!(config.display.color, ColorChoice::Palette256);
		assert!(config.display.ascii_only);
		assert_eq!(config.display.window_lines, 0);
		assert_eq!(config.rain.fade_half_life, Duration::from_secs(1));
		assert!(!config.rain.replay);
		assert_eq!(config.tail.backlog, 500);
		assert_eq!(config.sources.len(), 3);
		assert_eq!(
			config.sources[2],
			SourceConfig::Command {
				run: "docker logs -f web".into(),
				restart: false,
				restart_delay: Duration::from_secs(2),
				label: None,
				strip_pattern: None,
			}
		);
		assert_eq!(config.sources[0].label(), Some("sys"));
		assert_eq!(config.rules.len(), 3);
		assert_eq!(config.rules[0].hold, Hold::UntilAck);
		assert_eq!(config.rules[1].hold, Hold::For(Duration::from_secs(30)));
		assert_eq!(config.rules[2].hold, Hold::None);
	}

	#[test]
	fn command_defaults() {
		let config = Config::parse("[[source]]\ntype = \"command\"\nrun = \"yes\"\n").unwrap();
		assert_eq!(
			config.sources[0],
			SourceConfig::Command {
				run: "yes".into(),
				restart: true,
				restart_delay: Duration::from_secs(5),
				label: None,
				strip_pattern: None,
			}
		);
	}

	#[test]
	fn remote_journald_source() {
		let config = Config::parse(
			r#"
[[source]]
type = "journald"
host = "admin@web1"
ssh_args = ["-C", "-p", "2222"]
units = ["nginx.service"]
"#,
		)
		.unwrap();
		assert_eq!(
			config.sources,
			vec![SourceConfig::Journald {
				units: vec!["nginx.service".into()],
				remote: Some(RemoteHost {
					host: "admin@web1".into(),
					ssh_args: vec!["-C".into(), "-p".into(), "2222".into()],
				}),
				label: None,
				strip_pattern: None,
			}]
		);
	}

	#[test]
	fn remote_journald_errors() {
		let source = |keys: &str| error_of(&format!("[[source]]\ntype = \"journald\"\n{keys}\n"));
		assert!(source(r#"host = """#).contains("source[0].host"));
		assert!(source(r#"host = "a b""#).contains("source[0].host"));
		assert!(source(r#"host = "-oProxyCommand=x""#).contains("must not start with '-'"));
		assert!(
			source(
				r#"host = "h"
ssh_args = [""]"#
			)
			.contains("source[0].ssh_args")
		);
		assert!(source(r#"ssh_args = ["-C"]"#).contains("only allowed together with host"));
	}

	#[test]
	fn local_journal_needs_platform_support() {
		let local = Config::parse("").unwrap();
		assert!(local.check_local_journal(true).is_ok());
		let error = format!("{:#}", local.check_local_journal(false).unwrap_err());
		assert!(error.contains("source[0]") && error.contains("set host"));

		let remote = Config::parse("[[source]]\ntype = \"journald\"\nhost = \"h\"\n").unwrap();
		assert!(remote.check_local_journal(false).is_ok());
		let none = Config::parse("source = []").unwrap();
		assert!(none.check_local_journal(false).is_ok());
	}

	#[test]
	fn explicit_empty_source_list() {
		let config = Config::parse("source = []\n").unwrap();
		assert!(config.sources.is_empty());
	}

	#[test]
	fn unknown_keys_rejected() {
		assert!(error_of("[display]\nfsp = 20\n").contains("fsp"));
		assert!(error_of("[bogus]\nx = 1\n").contains("bogus"));
		assert!(
			error_of("[[source]]\ntype = \"file\"\npath = \"/x\"\npaht = \"y\"\n").contains("paht")
		);
		assert!(
			error_of(
				"[[rule]]\nname = \"a\"\npattern = \"x\"\ncolor = \"#ffffff\"\nhodl = \"ack\"\n"
			)
			.contains("hodl")
		);
		assert!(error_of("[[source]]\ntype = \"socket\"\n").contains("socket"));
	}

	#[test]
	fn out_of_range_values_rejected() {
		assert!(error_of("[display]\nfps = 100\n").contains("display.fps"));
		assert!(error_of("[display]\nfps = 1\n").contains("display.fps"));
		assert!(error_of("[display]\nwindow_lines = 50\n").contains("display.window_lines"));
		assert!(error_of("[rain]\nspeed_jitter = 0.95\n").contains("rain.speed_jitter"));
		assert!(error_of("[rain]\nfill = 0\n").contains("rain.fill"));
		assert!(error_of("[rain]\nfill = 1.5\n").contains("rain.fill"));
		assert!(error_of("[rain]\nreplay_brightness = 0.05\n").contains("rain.replay_brightness"));
		assert!(error_of("[rain]\nspeed = 0\n").contains("rain.speed"));
		assert!(error_of("[rain]\nmin_brightness = 1.5\n").contains("rain.min_brightness"));
		assert!(error_of("[rain]\nreplay_window = 0\n").contains("rain.replay_window"));
		assert!(error_of("[rain]\nreplay_window = \"big\"\n").contains("rain.replay_window"));
		assert!(error_of("[tail]\nbacklog = 0\n").contains("tail.backlog"));
		assert!(error_of("[display]\ncolor = \"8\"\n").contains("display.color"));
	}

	#[test]
	fn invalid_colors_rejected() {
		assert!(
			error_of("[display]\ndefault_color = \"green\"\n").contains("display.default_color")
		);
		assert!(
			error_of("[[rule]]\nname = \"bad\"\npattern = \"x\"\ncolor = \"#12345\"\n")
				.contains("rule 'bad'")
		);
	}

	#[test]
	fn invalid_durations_rejected() {
		assert!(error_of("[rain]\nfade_half_life = \"fast\"\n").contains("rain.fade_half_life"));
		assert!(
			error_of("[[source]]\ntype = \"command\"\nrun = \"x\"\nrestart_delay = \"soon\"\n")
				.contains("source[0].restart_delay")
		);
		assert!(
			error_of(
				"[[rule]]\nname = \"h\"\npattern = \"x\"\ncolor = \"#ffffff\"\nhold = \"later\"\n"
			)
			.contains("rule 'h'")
		);
	}

	#[test]
	fn invalid_regex_names_rule() {
		assert!(
			error_of("[[rule]]\nname = \"broken\"\npattern = \"(x\"\ncolor = \"#ffffff\"\n")
				.contains("rule 'broken'")
		);
	}

	#[test]
	fn missing_required_fields_rejected() {
		assert!(error_of("[[source]]\ntype = \"file\"\n").contains("path"));
		assert!(error_of("[[source]]\ntype = \"command\"\nrun = \"\"\n").contains("source[0].run"));
		assert!(error_of("[[rule]]\nname = \"x\"\ncolor = \"#ffffff\"\n").contains("pattern"));
	}

	#[test]
	fn strip_pattern_forms() {
		let default = Config::parse("").unwrap();
		assert_eq!(
			default.rain.strip_pattern.as_deref(),
			Some(DEFAULT_STRIP_PATTERN)
		);
		assert!(default.sim_settings().strip_pattern.is_some());
		let off = Config::parse("[rain]\nstrip_pattern = \"\"\n").unwrap();
		assert_eq!(off.rain.strip_pattern, None);
		assert!(off.sim_settings().strip_pattern.is_none());
		assert!(error_of("[rain]\nstrip_pattern = \"(\"\n").contains("rain.strip_pattern"));
	}

	#[test]
	fn per_source_strip_pattern() {
		let config = Config::parse(
			r#"
[rain]
strip_pattern = '^\S+ '

[[source]]
type = "file"
path = "/a.log"

[[source]]
type = "file"
path = "/b.log"
strip_pattern = '^\[\w+\] '

[[source]]
type = "command"
run = "yes"
strip_pattern = ""
"#,
		)
		.unwrap();
		assert_eq!(config.sources[0].strip_pattern(), None);
		assert_eq!(config.sources[1].strip_pattern(), Some(r"^\[\w+\] "));
		assert_eq!(config.sources[2].strip_pattern(), Some(""));
		let settings = config.sim_settings();
		let patterns: Vec<Option<&str>> = settings
			.source_strip_patterns
			.iter()
			.map(|pattern| pattern.as_ref().map(regex::Regex::as_str))
			.collect();
		assert_eq!(patterns, vec![Some(r"^\S+ "), Some(r"^\[\w+\] "), None]);
		let error = error_of("[[source]]\ntype = \"file\"\npath = \"/a\"\nstrip_pattern = \"(\"\n");
		assert!(error.contains("source[0].strip_pattern"), "{error}");
	}

	#[test]
	fn color_accepts_numbers() {
		let config = Config::parse("[display]\ncolor = 16\n").unwrap();
		assert_eq!(config.display.color, ColorChoice::Basic16);
	}
}
