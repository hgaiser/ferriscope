//! Client-level end-to-end test for the input gate.
//!
//! The gate: *keyboard and pointer from the control stream reach the app.* This exercises the whole path with real
//! clients — a Unix control socket, Xwayland, the xwm, the Smithay seat, and
//! `xev` as the app — rather than the unit-level decoder tests in `seat.rs`.
//!
//! It is `#[ignore]`d: it needs a render node, Xwayland and `xev`. Run it alone:
//!
//! ```text
//! cargo test -p ferriscope-core --test input_reaches_app -- --ignored --exact input_reaches_xev --nocapture
//! ```
//!
//! The compositor *connects* to the control socket, so the test binds a
//! `UnixListener` first and accepts the connection after `Ready`. `xev` is
//! spawned against the X display from `Ready`, and its stdout is accumulated on
//! a reader thread. The test then sends `motion-abs`, `button` and `key` lines
//! and waits for the matching X11 events to appear.

use std::{
	io::{BufRead, BufReader, ErrorKind, Write},
	os::unix::net::{UnixListener, UnixStream},
	path::PathBuf,
	process::{Child, Command, Stdio},
	sync::{Arc, Mutex, mpsc},
	time::{Duration, Instant},
};

use ferriscope_core::{Compositor, CompositorConfig, CompositorHandles, Ready};

/// Output width, small enough that a software-render fallback still keeps up.
const WIDTH: u32 = 320;
/// Output height.
const HEIGHT: u32 = 240;
/// Budget for the `Ready` handshake (Xwayland startup dominates).
const READY_TIMEOUT: Duration = Duration::from_secs(20);
/// Budget for the compositor to connect to our control listener.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(10);
/// Budget for `xev` to create and map its window.
const XEV_WINDOW_TIMEOUT: Duration = Duration::from_secs(20);
/// Budget for the injected events to surface in `xev`'s output.
const EVENT_TIMEOUT: Duration = Duration::from_secs(15);
/// Watchdog budget for `Compositor::stop` not to hang.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// Sleep between injected events, so each reaches the client before the next.
const EVENT_GAP: Duration = Duration::from_millis(150);
/// Poll slice while waiting on output or a connection.
const POLL_SLICE: Duration = Duration::from_millis(50);

/// Kills `xev`, stops the compositor and removes the control socket on drop.
///
/// A failed assertion must not leak a compositor thread, an `xev` or a socket.
struct InputGuard {
	compositor: Option<Compositor>,
	child: Option<Child>,
	socket_path: PathBuf,
}

impl InputGuard {
	/// Kill the client, then stop the compositor under a watchdog so a wedged
	/// event loop cannot hang the harness. Returns whether cleanup was clean.
	fn stop(&mut self, timeout: Duration) -> bool {
		self.kill_child();
		let (status_tx, status_rx) = mpsc::channel();
		let compositor = self.compositor.take();
		let stopper = std::thread::spawn(move || {
			let result = match compositor {
				Some(compositor) => compositor.stop(),
				None => Ok(()),
			};
			let _ = status_tx.send(result);
		});

		let stopped = match status_rx.recv_timeout(timeout) {
			Ok(result) => {
				let _ = stopper.join();
				result.is_ok()
			},
			Err(_) => false,
		};
		let _ = std::fs::remove_file(&self.socket_path);
		stopped
	}

	/// Kill and reap `xev` if it is still running.
	fn kill_child(&mut self) {
		if let Some(mut child) = self.child.take() {
			let _ = child.kill();
			let _ = child.wait();
		}
	}
}

impl Drop for InputGuard {
	fn drop(&mut self) {
		// The explicit `stop` is the asserted path; drop is the safety net.
		let _ = self.stop(STOP_TIMEOUT);
	}
}

/// A unique control-socket path under the temp dir.
fn unique_socket_path() -> PathBuf {
	let nanos = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_nanos())
		.unwrap_or(0);
	std::env::temp_dir().join(format!("ferriscope-input-{}-{nanos}.sock", std::process::id()))
}

/// Accept the compositor's control connection, bounded.
fn accept_control(listener: &UnixListener) -> UnixStream {
	listener.set_nonblocking(true).expect("listener nonblocking");
	let deadline = Instant::now() + ACCEPT_TIMEOUT;
	loop {
		match listener.accept() {
			Ok((stream, _)) => return stream,
			Err(e) if e.kind() == ErrorKind::WouldBlock => {},
			Err(e) => panic!("accept control connection: {e}"),
		}
		assert!(
			Instant::now() < deadline,
			"compositor did not connect to the control socket within {ACCEPT_TIMEOUT:?}"
		);
		std::thread::sleep(POLL_SLICE);
	}
}

/// Wait, bounded, until `needle` appears in the accumulated output.
fn wait_for_output(output: &Arc<Mutex<String>>, needle: &str, timeout: Duration) -> bool {
	let deadline = Instant::now() + timeout;
	loop {
		if output.lock().expect("output lock").contains(needle) {
			return true;
		}
		if Instant::now() >= deadline {
			return false;
		}
		std::thread::sleep(POLL_SLICE);
	}
}

/// Send one control line, flush it, and give the compositor a beat to inject it.
fn send_line(stream: &mut UnixStream, line: &str) {
	stream
		.write_all(line.as_bytes())
		.unwrap_or_else(|e| panic!("write control line {line:?}: {e}"));
	stream
		.flush()
		.unwrap_or_else(|e| panic!("flush control line {line:?}: {e}"));
	std::thread::sleep(EVENT_GAP);
}

/// Snapshot the accumulated `xev` output for failure messages.
fn output_snapshot(output: &Arc<Mutex<String>>) -> String {
	output.lock().expect("output lock").clone()
}

#[test]
#[ignore = "requires GPU, Xwayland and xev"]
fn input_reaches_xev() {
	let socket_path = unique_socket_path();
	let _ = std::fs::remove_file(&socket_path);
	let listener = UnixListener::bind(&socket_path).expect("bind control listener");

	let config = CompositorConfig {
		width: WIDTH,
		height: HEIGHT,
		xwayland: true,
		command: None,
		control_socket: Some(socket_path.clone()),
		..CompositorConfig::default()
	};

	let (compositor, handles): (Compositor, CompositorHandles) = Compositor::launch(config).expect("launch compositor");
	let mut guard = InputGuard {
		compositor: Some(compositor),
		child: None,
		socket_path,
	};

	let ready: Ready = handles
		.ready
		.recv_timeout(READY_TIMEOUT)
		.expect("ready handshake within timeout");
	let xdisplay = ready.xdisplay.expect("xwayland reports an X display");

	let mut control = accept_control(&listener);

	// `xev` prints events as it receives them; accumulate its stdout so the
	// assertions can watch for them without blocking the test thread.
	let mut xev = Command::new("xev");
	xev.env("DISPLAY", format!(":{xdisplay}"))
		.stdout(Stdio::piped())
		.stderr(Stdio::null());
	if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") {
		xev.env("XDG_RUNTIME_DIR", runtime_dir);
	}
	let mut child = xev.spawn().expect("spawn xev");
	let stdout = child.stdout.take().expect("xev stdout piped");
	let output = Arc::new(Mutex::new(String::new()));
	let reader_output = Arc::clone(&output);
	let reader = std::thread::spawn(move || {
		let mut reader = BufReader::new(stdout);
		let mut line = String::new();
		loop {
			line.clear();
			match reader.read_line(&mut line) {
				Ok(0) => break,
				Ok(_) => reader_output.lock().expect("output lock").push_str(&line),
				Err(_) => break,
			}
		}
	});
	guard.child = Some(child);

	// xev creates its window immediately; the xwm maps it fullscreen and
	// focuses it. Wait for the startup banner before injecting anything.
	assert!(
		wait_for_output(&output, "Outer window is", XEV_WINDOW_TIMEOUT),
		"xev did not start within {XEV_WINDOW_TIMEOUT:?}; output was:\n{}",
		output_snapshot(&output)
	);

	// Deliver pointer (motion + left button) and keyboard (KEY_A) input. Each
	// line is a separate round-trip so ordering is unambiguous.
	send_line(&mut control, "motion-abs 100 100\n");
	send_line(&mut control, "button 272 1\n");
	send_line(&mut control, "button 272 0\n");
	send_line(&mut control, "key 30 1\n");
	send_line(&mut control, "key 30 0\n");

	assert!(
		wait_for_output(&output, "MotionNotify event", EVENT_TIMEOUT),
		"no MotionNotify in xev output within {EVENT_TIMEOUT:?}; output was:\n{}",
		output_snapshot(&output)
	);
	assert!(
		wait_for_output(&output, "ButtonPress event", EVENT_TIMEOUT),
		"no ButtonPress in xev output within {EVENT_TIMEOUT:?}; output was:\n{}",
		output_snapshot(&output)
	);
	assert!(
		wait_for_output(&output, "KeyPress event", EVENT_TIMEOUT),
		"no KeyPress in xev output within {EVENT_TIMEOUT:?}; output was:\n{}",
		output_snapshot(&output)
	);

	guard.kill_child();
	let _ = reader.join();
	assert!(guard.stop(STOP_TIMEOUT), "compositor stopped cleanly");
}
