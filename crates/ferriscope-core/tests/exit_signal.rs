//! GPU integration tests for observing the compositor's exit.
//!
//! An embedder blocks in `Compositor::wait` on one thread and stops the
//! compositor through a `StopHandle` from another, so it never polls. These
//! tests are `#[ignore]`d: they need a render node. Run them with:
//!
//! ```text
//! cargo test -p ferriscope-core --test exit_signal -- --ignored --test-threads=1
//! ```

use std::{
	sync::mpsc,
	time::{Duration, Instant},
};

use ferriscope_core::{Compositor, CompositorConfig, CompositorHandles};

/// Output width, small enough that a software-render fallback still keeps up.
const WIDTH: u32 = 320;
/// Output height.
const HEIGHT: u32 = 240;
/// Handshake budget.
const READY_TIMEOUT: Duration = Duration::from_secs(10);
/// Budget for `wait` to observe the exit.
const EXIT_TIMEOUT: Duration = Duration::from_secs(10);

fn launch(command: Option<Vec<String>>) -> (Compositor, CompositorHandles) {
	let config = CompositorConfig {
		width: WIDTH,
		height: HEIGHT,
		xwayland: false,
		command,
		..CompositorConfig::default()
	};
	Compositor::launch(config).expect("launch compositor")
}

/// Run `Compositor::wait` on its own thread and report its result.
fn wait_in_thread(compositor: Compositor) -> mpsc::Receiver<ferriscope_core::Result<()>> {
	let (tx, rx) = mpsc::channel();
	std::thread::spawn(move || {
		let _ = tx.send(compositor.wait());
	});
	rx
}

/// A thread blocked in `wait` returns once another thread calls the handle.
#[test]
#[ignore = "requires a GPU render node"]
fn wait_returns_after_a_stop_from_another_thread() {
	let (compositor, handles) = launch(None);
	handles.ready.recv_timeout(READY_TIMEOUT).expect("ready handshake");
	let stop = compositor.stop_handle();
	let exited = wait_in_thread(compositor);

	assert!(
		exited.recv_timeout(Duration::from_millis(300)).is_err(),
		"the compositor exited without being stopped"
	);

	let stopper = std::thread::spawn(move || stop.stop());
	stopper.join().expect("stop thread");
	let started = Instant::now();
	let result = exited.recv_timeout(EXIT_TIMEOUT).expect("wait observed the stop");
	assert!(result.is_ok(), "stop reported {result:?}");
	assert!(started.elapsed() < EXIT_TIMEOUT);
}

/// The handle is harmless once the compositor is gone, and may be repeated.
#[test]
#[ignore = "requires a GPU render node"]
fn a_stop_handle_outlives_the_compositor() {
	let (compositor, handles) = launch(None);
	handles.ready.recv_timeout(READY_TIMEOUT).expect("ready handshake");
	let stop = compositor.stop_handle();
	stop.stop();
	stop.stop();
	compositor.wait().expect("wait after stop");
	stop.stop();
}

/// `wait` returns on its own when the hosted command exits.
#[test]
#[ignore = "requires a GPU render node"]
fn wait_returns_when_the_hosted_command_exits() {
	let (compositor, _handles) = launch(Some(vec!["true".to_string()]));
	let exited = wait_in_thread(compositor);
	let result = exited
		.recv_timeout(EXIT_TIMEOUT)
		.expect("wait observed the hosted command exiting");
	assert!(result.is_ok(), "the compositor reported {result:?}");
}
