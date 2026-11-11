//! Fake log generator for `--demo`.

use std::thread;
use std::time::Duration;

use chrono::Local;
use fastrand::Rng;

use super::SourceContext;
use crate::rules::Rule;

/// Number of fake history lines sent as preload.
const PRELOAD_LINES: usize = 40;

/// The three rules installed by `--demo` when there is no config file.
pub fn demo_rules() -> Vec<Rule> {
	vec![
		Rule::new(
			"demo-oom",
			"(?i)out of memory|oom-killer|segfault",
			"#ff463c",
			Some("ack"),
		)
		.expect("valid demo rule"),
		Rule::new(
			"demo-auth-fail",
			"Failed password for \\S+",
			"#ffb428",
			Some("10s"),
		)
		.expect("valid demo rule"),
		Rule::new("demo-login", "Accepted publickey", "#3cc8ff", None).expect("valid demo rule"),
	]
}

/// Starts the generator thread.
pub fn spawn(context: SourceContext) {
	thread::Builder::new()
		.name("source-demo".into())
		.spawn(move || run(context))
		.expect("failed to start demo thread");
}

fn run(context: SourceContext) {
	let mut rng = Rng::new();
	let start = Local::now();
	for index in 0..PRELOAD_LINES {
		let age = chrono::Duration::milliseconds(((PRELOAD_LINES - index) * 2300) as i64);
		if !context.send_preload(start - age, &fake_line(&mut rng)) {
			return;
		}
	}
	if !context.preload_done() {
		return;
	}
	loop {
		// About 5 lines per second on average.
		thread::sleep(Duration::from_millis(rng.u64(20..380)));
		if !context.send_live(Local::now(), &fake_line(&mut rng)) {
			return;
		}
	}
}

const USERS: &[&str] = &["root", "admin", "deploy", "git", "ubuntu", "oracle", "test"];
const UNITS: &[&str] = &[
	"nginx.service",
	"cron.service",
	"logrotate.service",
	"apt-daily.service",
	"systemd-tmpfiles-clean.service",
	"docker.service",
	"postgresql.service",
	"fstrim.service",
];
const PATHS: &[&str] = &[
	"/",
	"/index.html",
	"/api/v1/status",
	"/api/v1/users",
	"/static/app.js",
	"/login",
	"/favicon.ico",
	"/metrics",
];

fn ip(rng: &mut Rng) -> String {
	format!(
		"{}.{}.{}.{}",
		rng.u8(1..224),
		rng.u8(..),
		rng.u8(..),
		rng.u8(1..255)
	)
}

/// One realistic fake log line. Roughly one in 20 matches an error rule.
pub fn fake_line(rng: &mut Rng) -> String {
	let pid = rng.u32(300..65000);
	let roll = rng.u32(..100);
	match roll {
		// Error lines (about 5%).
		0 => format!(
			"kernel: Out of memory: Killed process {pid} (java) total-vm:{}kB, anon-rss:{}kB",
			rng.u32(1_000_000..9_000_000),
			rng.u32(500_000..4_000_000)
		),
		1 => format!(
			"kernel: worker[{pid}]: segfault at {:x} ip {:012x} sp {:012x} error 4 in libc.so.6",
			rng.u32(..),
			rng.u64(..1 << 47),
			rng.u64(..1 << 47)
		),
		2..=4 => format!(
			"sshd[{pid}]: Failed password for {} from {} port {} ssh2",
			USERS[rng.usize(..USERS.len())],
			ip(rng),
			rng.u16(1024..65535)
		),
		// Logins, colored only.
		5..=8 => format!(
			"sshd[{pid}]: Accepted publickey for {} from {} port {} ssh2: ED25519 SHA256:{:x}",
			USERS[rng.usize(..USERS.len())],
			ip(rng),
			rng.u16(1024..65535),
			rng.u64(..)
		),
		9..=24 => format!(
			"systemd[1]: {} {}",
			["Started", "Starting", "Finished", "Reloaded", "Stopped"][rng.usize(..5)],
			UNITS[rng.usize(..UNITS.len())]
		),
		25..=34 => format!(
			"CRON[{pid}]: ({}) CMD ({})",
			USERS[rng.usize(..USERS.len())],
			[
				"run-parts /etc/cron.hourly",
				"/usr/local/bin/backup.sh",
				"test -x /usr/sbin/anacron",
				"php /var/www/cron.php"
			][rng.usize(..4)]
		),
		35..=44 => format!(
			"kernel: [{}.{:06}] {}",
			rng.u32(1000..900_000),
			rng.u32(..1_000_000),
			[
				"eth0: Link is Up - 1Gbps/Full - flow control rx/tx",
				"EXT4-fs (sda1): re-mounted. Opts: errors=remount-ro",
				"audit: type=1400 audit: apparmor=\"STATUS\" operation=\"profile_load\"",
				"TCP: request_sock_TCP: Possible SYN flooding on port 443.",
				"usb 1-1: new high-speed USB device number 4 using xhci_hcd",
				"nf_conntrack: default automatic helper assignment has been turned off"
			][rng.usize(..6)]
		),
		45..=54 => format!(
			"sshd[{pid}]: {}",
			[
				format!(
					"Connection closed by {} port {} [preauth]",
					ip(rng),
					rng.u16(1024..65535)
				),
				format!(
					"pam_unix(sshd:session): session opened for user {}",
					USERS[rng.usize(..USERS.len())]
				),
				format!(
					"Received disconnect from {} port {}:11: disconnected by user",
					ip(rng),
					rng.u16(1024..65535)
				),
				format!(
					"Invalid user {} from {}",
					["pi", "guest", "support", "user"][rng.usize(..4)],
					ip(rng)
				),
			][rng.usize(..4)]
		),
		_ => {
			let status = [200, 200, 200, 200, 304, 301, 404, 500, 502][rng.usize(..9)];
			format!(
				"nginx[{}]: {} - - \"{} {} HTTP/1.1\" {status} {} \"-\" \"Mozilla/5.0\"",
				rng.u32(800..900),
				ip(rng),
				["GET", "GET", "GET", "POST", "HEAD"][rng.usize(..5)],
				PATHS[rng.usize(..PATHS.len())],
				rng.u32(0..60_000)
			)
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::rules::RuleSet;

	#[test]
	fn about_one_in_twenty_lines_is_an_error() {
		let rules = RuleSet::new(demo_rules()).unwrap();
		let mut rng = Rng::with_seed(7);
		let total = 20_000;
		let errors = (0..total)
			.filter(|_| {
				let line = fake_line(&mut rng);
				matches!(rules.first_match(&line), Some(0) | Some(1))
			})
			.count();
		let share = errors as f64 / total as f64;
		assert!((0.035..0.07).contains(&share), "error share {share}");
	}
}
