use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;

use anyhow::Result;
use clap::Parser;
use fastrand::Rng;

use lmatrix::rules::RuleSet;
use lmatrix::sim::{Sim, SimSettings};
use lmatrix::source::{self, CHANNEL_CAPACITY, SourceContext};
use lmatrix::term::{self, TerminalGuard};
use lmatrix::ui::{self, UiOptions};

/// Matrix-rain style log viewer.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
	/// Use a built-in fake line generator instead of the configured sources.
	#[arg(long)]
	demo: bool,
}

fn main() -> ExitCode {
	let args = Args::parse();
	match run(args) {
		Ok(()) => ExitCode::SUCCESS,
		Err(error) => {
			term::restore_terminal();
			eprintln!("lmatrix: {error:#}");
			ExitCode::FAILURE
		}
	}
}

fn run(args: Args) -> Result<()> {
	if !args.demo {
		anyhow::bail!("only --demo is available in this build");
	}
	let settings = SimSettings::default();
	let rules = RuleSet::new(source::demo::demo_rules())?;
	let sim = Sim::new(settings, rules, Rng::new());

	let (sender, receiver) = mpsc::sync_channel(CHANNEL_CAPACITY);
	source::demo::spawn(SourceContext {
		id: 0,
		label: None,
		ascii_only: false,
		sender,
	});

	let shutdown = Arc::new(AtomicBool::new(false));
	for signal in [
		signal_hook::consts::SIGTERM,
		signal_hook::consts::SIGHUP,
		signal_hook::consts::SIGINT,
	] {
		signal_hook::flag::register(signal, Arc::clone(&shutdown))?;
	}

	let options = UiOptions {
		fps: 20,
		color_mode: term::detect_color_mode_from_env(),
		window_lines: 2,
		ascii_only: false,
	};
	let guard = TerminalGuard::enter()?;
	let result = ui::run(sim, receiver, 1, options, &shutdown);
	drop(guard);
	source::terminate_children();
	result
}
