use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;

use anyhow::Result;
use clap::Parser;
use fastrand::Rng;

use lmatrix::config::{self, ColorChoice, Config};
use lmatrix::rules::RuleSet;
use lmatrix::sim::Sim;
use lmatrix::source::{self, CHANNEL_CAPACITY, SourceContext};
use lmatrix::term::{self, ColorMode, TerminalGuard};
use lmatrix::ui::{self, UiOptions};

/// Signals that end the app cleanly. Windows has no SIGHUP.
#[cfg(unix)]
const SHUTDOWN_SIGNALS: &[i32] = &[
	signal_hook::consts::SIGTERM,
	signal_hook::consts::SIGHUP,
	signal_hook::consts::SIGINT,
];
#[cfg(windows)]
const SHUTDOWN_SIGNALS: &[i32] = &[signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT];

/// Matrix-rain style log viewer: the rain is made of your logs.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
	/// Use this config file.
	#[arg(long, value_name = "PATH")]
	config: Option<PathBuf>,

	/// Validate the config, print the result and exit with 0 or 1.
	#[arg(long)]
	check: bool,

	/// Print the commented default config to stdout.
	#[arg(long)]
	print_default_config: bool,

	/// Ignore configured sources and use a built-in fake line generator.
	#[arg(long)]
	demo: bool,
}

fn main() -> ExitCode {
	let args = Args::parse();
	match run(args) {
		Ok(code) => code,
		Err(error) => {
			term::restore_terminal();
			source::terminate_children();
			eprintln!("lmatrix: {error:#}");
			ExitCode::FAILURE
		}
	}
}

fn run(args: Args) -> Result<ExitCode> {
	if args.print_default_config {
		print!("{}", config::DEFAULT_CONFIG);
		return Ok(ExitCode::SUCCESS);
	}

	let path = config::locate(args.config.as_deref())?;
	let loaded = match &path {
		Some(path) => Config::load_file(path),
		None => Ok(Config::default()),
	};

	if args.check {
		return Ok(match loaded {
			Ok(config) => {
				let origin = match &path {
					Some(path) => path.display().to_string(),
					None => "built-in defaults (no config file found)".to_string(),
				};
				println!(
					"config OK: {origin} ({} source(s), {} rule(s))",
					config.sources.len(),
					config.rules.len()
				);
				ExitCode::SUCCESS
			}
			Err(error) => {
				println!("config error: {error:#}");
				ExitCode::FAILURE
			}
		});
	}

	let mut config = loaded?;
	if args.demo && path.is_none() {
		config.rules = source::demo::demo_rules();
	}

	let rules = RuleSet::new(config.rules.clone())?;
	let sim = Sim::new(config.sim_settings(), rules, Rng::new());

	let (sender, receiver) = mpsc::sync_channel(CHANNEL_CAPACITY);
	let ascii_only = config.display.ascii_only;
	let source_count = if args.demo {
		source::demo::spawn(SourceContext {
			id: 0,
			label: None,
			ascii_only,
			sender,
		});
		1
	} else {
		for (id, source_config) in config.sources.iter().enumerate() {
			let context = SourceContext {
				id,
				label: source_config.label().map(str::to_string),
				ascii_only,
				sender: sender.clone(),
			};
			source::spawn(source_config, context, config.tail.preload);
		}
		drop(sender);
		config.sources.len()
	};

	let shutdown = Arc::new(AtomicBool::new(false));
	for signal in SHUTDOWN_SIGNALS {
		signal_hook::flag::register(*signal, Arc::clone(&shutdown))?;
	}

	let options = UiOptions {
		fps: config.display.fps,
		min_brightness: config.rain.min_brightness,
		color_mode: match config.display.color {
			ColorChoice::Auto => term::detect_color_mode_from_env(),
			ColorChoice::TrueColor => ColorMode::TrueColor,
			ColorChoice::Palette256 => ColorMode::Palette256,
			ColorChoice::Basic16 => ColorMode::Basic16,
		},
		window_lines: config.display.window_lines,
		ascii_only,
	};

	let guard = TerminalGuard::enter()?;
	let result = ui::run(sim, receiver, source_count, options, &shutdown);
	drop(guard);
	source::terminate_children();
	result.map(|()| ExitCode::SUCCESS)
}
