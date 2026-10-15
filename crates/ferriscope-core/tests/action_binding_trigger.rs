//! GPU integration test for the `gamescope_action_binding_manager` protocol.
//!
//! The pure hotkey state machine is unit-tested in `action_binding.rs`. This
//! test closes the wire path end to end: a raw `wayland-client` client binds the
//! global, creates a binding, registers `Shift+Tab`, arms it, and the compositor
//! must emit `gamescope_action_binding.triggered` when the chord is injected over
//! the control socket (the compositor's only keyboard route).
//!
//! It is `#[ignore]`d: it launches the compositor, which needs a render node.
//! Run it alone:
//!
//! ```text
//! timeout 120 cargo test -p ferriscope-core --test action_binding_trigger -- --ignored --exact chords_trigger_bindings --nocapture
//! ```

mod protocol {
	#![allow(
		dead_code,
		non_camel_case_types,
		unused_unsafe,
		unused_variables,
		non_upper_case_globals,
		non_snake_case,
		unused_imports,
		missing_docs
	)]
	#![allow(clippy::all)]

	/// Client-side API of the protocol.
	pub mod client {
		use wayland_client;
		use wayland_client::protocol::*;

		pub mod __interfaces {
			use wayland_client::protocol::__interfaces::*;
			wayland_scanner::generate_interfaces!("protocol/gamescope-action-binding.xml");
		}
		use self::__interfaces::*;

		wayland_scanner::generate_client_code!("protocol/gamescope-action-binding.xml");
	}
}

use std::{
	os::unix::net::{UnixListener, UnixStream},
	path::PathBuf,
	sync::mpsc,
	thread,
	time::{Duration, Instant},
};

use ferriscope_core::{Compositor, CompositorConfig, CompositorHandles, Ready};
use protocol::client::{
	gamescope_action_binding::{ArmFlag, GamescopeActionBinding, TriggerFlag},
	gamescope_action_binding_manager::GamescopeActionBindingManager,
};
use wayland_client::{
	Connection, Dispatch, QueueHandle, WEnum, delegate_noop,
	globals::{GlobalListContents, registry_queue_init},
	protocol::wl_registry,
};

/// Output size; small enough that the software fallback keeps up.
const WIDTH: u32 = 320;
/// Output height.
const HEIGHT: u32 = 240;
/// Budget for the `Ready` handshake.
const READY_TIMEOUT: Duration = Duration::from_secs(20);
/// Budget for the compositor to connect to the test's control socket.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(10);
/// Budget for a `triggered` event to arrive after the chord is injected.
const EVENT_TIMEOUT: Duration = Duration::from_secs(15);
/// Watchdog budget for `Compositor::stop` not to hang.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// Sleep between control lines so each reaches the compositor alone.
const CONTROL_GAP: Duration = Duration::from_millis(120);
/// Poll slice while waiting for an event.
const POLL_SLICE: Duration = Duration::from_millis(25);

/// evdev keycode for `KEY_LEFTSHIFT`.
const SHIFT: u32 = 42;
/// evdev keycode for `KEY_TAB`.
const TAB: u32 = 15;
/// evdev keycode for `KEY_A`, used as a non-chord control key.
const A: u32 = 30;

/// Client state: the `triggered` events the compositor sent.
#[derive(Default)]
struct ClientState {
	/// `(sequence, time_lo, time_hi, keyboard_flag)` per `triggered` event.
	triggered: Vec<(u32, u32, u32, bool)>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ClientState {
	fn event(
		_state: &mut Self,
		_registry: &wl_registry::WlRegistry,
		_event: wl_registry::Event,
		_data: &GlobalListContents,
		_conn: &Connection,
		_qh: &QueueHandle<Self>,
	) {
	}
}

delegate_noop!(ClientState: ignore GamescopeActionBindingManager);

impl Dispatch<GamescopeActionBinding, ()> for ClientState {
	fn event(
		state: &mut Self,
		_binding: &GamescopeActionBinding,
		event: protocol::client::gamescope_action_binding::Event,
		_data: &(),
		_conn: &Connection,
		_qh: &QueueHandle<Self>,
	) {
		let protocol::client::gamescope_action_binding::Event::Triggered {
			sequence,
			time_lo,
			time_hi,
			trigger_flags,
		} = event;
		let keyboard = matches!(trigger_flags, WEnum::Value(flags) if flags.contains(TriggerFlag::Keyboard));
		state.triggered.push((sequence, time_lo, time_hi, keyboard));
	}
}

/// The launched compositor and its accepted control stream.
struct Harness {
	compositor: Option<Compositor>,
	socket_path: PathBuf,
	control: UnixStream,
	_handles: CompositorHandles,
}

impl Harness {
	fn stop(&mut self) -> bool {
		let (tx, rx) = mpsc::channel();
		let compositor = self.compositor.take();
		thread::spawn(move || {
			let result = match compositor {
				Some(compositor) => compositor.stop(),
				None => Ok(()),
			};
			let _ = tx.send(result);
		});
		let stopped = rx
			.recv_timeout(STOP_TIMEOUT)
			.map(|result| result.is_ok())
			.unwrap_or(false);
		let _ = std::fs::remove_file(&self.socket_path);
		stopped
	}
}

impl Drop for Harness {
	fn drop(&mut self) {
		let _ = self.stop();
	}
}

/// Send one control line and give the compositor a beat to inject it.
fn send_line(stream: &mut UnixStream, line: &str) {
	use std::io::Write;
	stream
		.write_all(line.as_bytes())
		.unwrap_or_else(|e| panic!("write control line {line:?}: {e}"));
	stream
		.flush()
		.unwrap_or_else(|e| panic!("flush control line {line:?}: {e}"));
	thread::sleep(CONTROL_GAP);
}

/// Launch the compositor and accept its control connection.
fn start() -> (Harness, Ready) {
	let socket_path = std::env::temp_dir().join(format!(
		"ferriscope-action-binding-{}-{}.sock",
		std::process::id(),
		std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.map(|d| d.as_nanos())
			.unwrap_or(0)
	));
	let _ = std::fs::remove_file(&socket_path);
	let listener = UnixListener::bind(&socket_path).expect("bind control listener");

	let config = CompositorConfig {
		width: WIDTH,
		height: HEIGHT,
		xwayland: false,
		command: None,
		control_socket: Some(socket_path.clone()),
		..CompositorConfig::default()
	};
	let (compositor, handles) = Compositor::launch(config).expect("launch compositor");
	let ready = handles
		.ready
		.recv_timeout(READY_TIMEOUT)
		.expect("ready handshake within timeout");

	listener.set_nonblocking(true).expect("listener nonblocking");
	let deadline = Instant::now() + ACCEPT_TIMEOUT;
	let control = loop {
		match listener.accept() {
			Ok((stream, _)) => break stream,
			Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {},
			Err(e) => panic!("accept control connection: {e}"),
		}
		assert!(
			Instant::now() < deadline,
			"compositor did not connect to the control socket within {ACCEPT_TIMEOUT:?}"
		);
		thread::sleep(POLL_SLICE);
	};

	let harness = Harness {
		compositor: Some(compositor),
		socket_path,
		control,
		_handles: handles,
	};
	(harness, ready)
}

/// A client binds the manager, registers and arms `Shift+Tab`, and receives
/// `triggered` when the chord is injected; an unrelated key does not trigger.
#[test]
#[ignore = "requires GPU; native Wayland client for gamescope_action_binding"]
fn chords_trigger_bindings() {
	let (mut harness, ready) = start();
	let runtime_dir = std::env::var("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR is set");
	let socket = std::path::Path::new(&runtime_dir).join(&ready.wayland_display);
	let stream = UnixStream::connect(&socket).expect("connect wayland socket");
	let connection = Connection::from_socket(stream).expect("wayland connection");
	let (globals, mut queue) = registry_queue_init::<ClientState>(&connection).expect("registry");
	let qh = queue.handle();
	let mut client = ClientState::default();

	let manager: GamescopeActionBindingManager = globals
		.bind::<GamescopeActionBindingManager, ClientState, ()>(&qh, 1..=1, ())
		.expect("bind gamescope_action_binding_manager");

	let binding = manager.create_action_binding(&qh, ());
	let mut keysyms = Vec::new();
	for keysym in [smithay_keysym_shift_l(), smithay_keysym_tab()] {
		keysyms.extend_from_slice(&keysym.to_ne_bytes());
	}
	binding.add_keyboard_trigger(keysyms);
	binding.arm(ArmFlag::empty());
	connection.flush().expect("flush requests");
	// The server processes create/trigger/arm.
	queue.roundtrip(&mut client).expect("roundtrip setup");

	// A key outside the chord must not trigger.
	send_line(&mut harness.control, &format!("key {A} 1\n"));
	send_line(&mut harness.control, &format!("key {A} 0\n"));
	queue.roundtrip(&mut client).expect("roundtrip a");
	assert!(
		client.triggered.is_empty(),
		"a non-chord key must not trigger: {:?}",
		client.triggered
	);

	// The registered chord: Shift, then Tab.
	send_line(&mut harness.control, &format!("key {SHIFT} 1\n"));
	send_line(&mut harness.control, &format!("key {TAB} 1\n"));

	let deadline = Instant::now() + EVENT_TIMEOUT;
	while client.triggered.is_empty() && Instant::now() < deadline {
		queue.roundtrip(&mut client).expect("roundtrip trigger");
		thread::sleep(POLL_SLICE);
	}
	assert!(
		!client.triggered.is_empty(),
		"Shift+Tab did not produce a triggered event"
	);
	let (sequence, time_lo, time_hi, keyboard) = client.triggered[0];
	assert!(keyboard, "trigger_flag.keyboard must be set");
	assert_eq!(sequence, 0, "the first trigger has sequence 0");
	assert!(
		u64::from(time_hi) << 32 | u64::from(time_lo) > 0,
		"the timestamp must be a non-zero CLOCK_MONOTONIC value"
	);

	// Release the chord so the compositor's held set is clean.
	send_line(&mut harness.control, &format!("key {TAB} 0\n"));
	send_line(&mut harness.control, &format!("key {SHIFT} 0\n"));

	assert!(harness.stop(), "compositor stopped cleanly");
}

/// `XKB_KEY_Shift_L`.
fn smithay_keysym_shift_l() -> u32 {
	0xffe1
}

/// `XKB_KEY_Tab`.
fn smithay_keysym_tab() -> u32 {
	0xff09
}
