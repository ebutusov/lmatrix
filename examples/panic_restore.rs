//! Enters the full-screen terminal state and panics, to check that the panic
//! hook restores the terminal. Used by `tests/e2e_tmux.sh`.

fn main() {
	let _guard = lmatrix::term::TerminalGuard::enter().expect("terminal setup");
	print!("inside the alternate screen");
	std::thread::sleep(std::time::Duration::from_millis(300));
	panic!("deliberate panic to test terminal restoration");
}
