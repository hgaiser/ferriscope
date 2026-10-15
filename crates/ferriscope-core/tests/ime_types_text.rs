//! End-to-end test for the `gamescope_input_method` global (Steam's on-screen
//! keyboard).
//!
//! The test binds the global as a second Wayland client, sends text and
//! actions, and checks the keyboard events the window's `wl_keyboard` sees: a
//! character the seat keymap can type is a plain key (with Shift when needed),
//! anything else arrives through a temporary keymap that is reset after an
//! idle period.
//!
//! It is `#[ignore]`d: it needs a render node. Run it alone:
//!
//! ```text
//! timeout 120 cargo test -p ferriscope-core --test ime_types_text -- --ignored --nocapture
//! ```

use std::{
	io::{BufRead, BufReader},
	os::unix::net::UnixStream,
	path::Path,
	process::{Command, Stdio},
	sync::{Arc, Mutex},
	time::{Duration, Instant},
};

use ferriscope_core::{Compositor, CompositorConfig, Ready};

use smithay::input::keyboard::{
	keysyms,
	xkb::{self, CONTEXT_NO_FLAGS, KEYMAP_COMPILE_NO_FLAGS, KEYMAP_FORMAT_TEXT_V1, Keycode},
};
use wayland_client::{
	Connection, Dispatch, QueueHandle, delegate_noop,
	globals::{GlobalListContents, registry_queue_init},
	protocol::{wl_registry, wl_seat},
};

use support::{KeyboardEvent, Target};

mod support;

#[allow(
	dead_code,
	non_camel_case_types,
	unused_unsafe,
	unused_variables,
	non_snake_case,
	unused_imports
)]
#[allow(missing_docs, clippy::all)]
mod protocol {
	pub mod client {
		use wayland_client;
		use wayland_client::protocol::*;

		pub mod __interfaces {
			use wayland_client::protocol::__interfaces::*;
			wayland_scanner::generate_interfaces!("protocol/gamescope-input-method.xml");
		}
		use self::__interfaces::*;

		wayland_scanner::generate_client_code!("protocol/gamescope-input-method.xml");
	}
}

use protocol::client::{
	gamescope_input_method::{self, Action, GamescopeInputMethod},
	gamescope_input_method_manager::GamescopeInputMethodManager,
};

/// Budget for the window to map and receive keyboard focus.
const FOCUS_TIMEOUT: Duration = Duration::from_secs(15);
/// Budget for typed keys to surface.
const EVENT_TIMEOUT: Duration = Duration::from_secs(10);

const KEY_A: u32 = 30;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_BACKSPACE: u32 = 14;

/// The second client: binds the manager, creates an input method and keeps the
/// serial from its `done` event.
struct ImeClient {
	serial: Option<u32>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ImeClient {
	fn event(
		_: &mut Self,
		_: &wl_registry::WlRegistry,
		_: wl_registry::Event,
		_: &GlobalListContents,
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
	}
}

delegate_noop!(ImeClient: ignore wl_seat::WlSeat);
delegate_noop!(ImeClient: ignore GamescopeInputMethodManager);

impl Dispatch<GamescopeInputMethod, ()> for ImeClient {
	fn event(
		state: &mut Self,
		_: &GamescopeInputMethod,
		event: gamescope_input_method::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		if let gamescope_input_method::Event::Done { serial } = event {
			state.serial = Some(serial);
		}
	}
}

struct Ime {
	queue: wayland_client::EventQueue<ImeClient>,
	state: ImeClient,
	method: GamescopeInputMethod,
	// Keeps the connection (and so the objects) alive.
	_connection: Connection,
}

impl Ime {
	fn connect(socket: &std::path::Path) -> Self {
		let stream = UnixStream::connect(socket).expect("connect to the compositor");
		let connection = Connection::from_socket(stream).expect("wayland connection");
		let (globals, mut queue) = registry_queue_init::<ImeClient>(&connection).expect("registry");
		let qh = queue.handle();
		let manager: GamescopeInputMethodManager = globals
			.bind(&qh, 1..=3, ())
			.expect("the compositor advertises gamescope_input_method_manager");
		let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ()).expect("wl_seat");
		let method = manager.create_input_method(&seat, &qh, ());
		let mut state = ImeClient { serial: None };
		queue.roundtrip(&mut state).expect("roundtrip");
		assert_eq!(state.serial, Some(1), "no done event after create_input_method");
		Self {
			queue,
			state,
			method,
			_connection: connection,
		}
	}

	fn commit_text(&mut self, text: &str) {
		self.method.set_string(text.to_string());
		self.method.commit(self.state.serial.expect("serial"));
		self.queue.roundtrip(&mut self.state).expect("roundtrip");
	}

	fn commit_action(&mut self, action: Action) {
		self.method.set_action(action);
		self.method.commit(self.state.serial.expect("serial"));
		self.queue.roundtrip(&mut self.state).expect("roundtrip");
	}
}

/// The keysym an evdev key types under `keymap_text`.
fn keysym_in(keymap_text: &str, evdev: u32) -> u32 {
	let context = xkb::Context::new(CONTEXT_NO_FLAGS);
	let keymap = xkb::Keymap::new_from_string(
		&context,
		keymap_text.to_string(),
		KEYMAP_FORMAT_TEXT_V1,
		KEYMAP_COMPILE_NO_FLAGS,
	)
	.expect("the client's keymap compiles");
	keymap
		.key_get_syms_by_level(Keycode::new(evdev + 8), 0, 0)
		.first()
		.expect("a keysym")
		.raw()
}

#[test]
#[ignore = "requires GPU; native Wayland client for the Steam on-screen keyboard"]
fn input_method_types_text_into_the_focused_client() {
	let mut started = support::start(support::ClientOptions::default());
	let parent = started.client.surface_id(Target::Parent).expect("parent surface id");
	assert!(
		support::wait_log(&started.client, FOCUS_TIMEOUT, |log| log
			.keyboard_enter
			.contains(&parent)),
		"the window did not receive keyboard focus"
	);
	let initial_keymaps = started.client.snapshot().keymaps.len();
	assert!(initial_keymaps >= 1, "the client got no initial keymap");

	let mut ime = Ime::connect(&started.wayland_socket);

	// A plain lowercase letter is its own key on the seat keymap: no keymap
	// switch.
	ime.commit_text("a");
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log.keys
			== [(KEY_A, true), (KEY_A, false)]),
		"typing \"a\": keys were {:?}",
		started.client.snapshot().keys
	);
	assert_eq!(started.client.snapshot().keymaps.len(), initial_keymaps);

	// An uppercase letter holds Shift around the key.
	ime.commit_text("A");
	let expected = [
		(KEY_A, true),
		(KEY_A, false),
		(KEY_LEFTSHIFT, true),
		(KEY_A, true),
		(KEY_A, false),
		(KEY_LEFTSHIFT, false),
	];
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log.keys == expected),
		"typing \"A\": keys were {:?}",
		started.client.snapshot().keys
	);

	// An editing action presses its key.
	ime.commit_action(Action::DeleteLeft);
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log
			.keys
			.ends_with(&[(KEY_BACKSPACE, true), (KEY_BACKSPACE, false)])),
		"DeleteLeft: keys were {:?}",
		started.client.snapshot().keys
	);

	// Characters the seat keymap cannot type go through a temporary keymap,
	// which is reset once typing stops.
	let before = started.client.snapshot().keys.len();
	ime.commit_text("€é");
	assert!(
		support::wait_log(&started.client, EVENT_TIMEOUT, |log| log.keymaps.len()
			== initial_keymaps + 2),
		"the temporary keymap was not installed and reset"
	);
	let log = started.client.snapshot();
	assert_eq!(
		log.keymaps.last(),
		log.keymaps.first(),
		"the reset keymap is not the original one"
	);
	let temporary = initial_keymaps;
	let typed: Vec<u32> = log
		.keyboard_events
		.iter()
		.skip_while(|event| **event != KeyboardEvent::Keymap(temporary))
		.take_while(|event| **event != KeyboardEvent::Keymap(temporary + 1))
		.filter_map(|event| match event {
			KeyboardEvent::Key { evdev, pressed: true } => Some(keysym_in(&log.keymaps[temporary], *evdev)),
			_ => None,
		})
		.collect();
	assert_eq!(
		typed,
		[keysyms::KEY_EuroSign, keysyms::KEY_eacute],
		"keys after the temporary keymap (keys before: {before}, now: {:?})",
		log.keys
	);

	ime.method.destroy();
	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

/// The path Steam's keyboard takes in practice: an X11 client under Xwayland
/// receives the text, including a character that needs the temporary keymap.
#[test]
#[ignore = "requires GPU, Xwayland and xev"]
fn input_method_types_into_an_x11_client() {
	let config = CompositorConfig {
		width: 320,
		height: 240,
		xwayland: true,
		..CompositorConfig::default()
	};
	let (compositor, handles) = Compositor::launch(config).expect("launch compositor");
	let ready: Ready = handles
		.ready
		.recv_timeout(Duration::from_secs(20))
		.expect("ready handshake");
	let xdisplay = ready.xdisplay.expect("xwayland reports an X display");

	let mut xev = Command::new("xev")
		.env("DISPLAY", format!(":{xdisplay}"))
		.stdout(Stdio::piped())
		.stderr(Stdio::null())
		.spawn()
		.expect("spawn xev");
	let stdout = xev.stdout.take().expect("xev stdout");
	let output = Arc::new(Mutex::new(String::new()));
	let reader_output = Arc::clone(&output);
	let reader = std::thread::spawn(move || {
		for line in BufReader::new(stdout).lines().map_while(Result::ok) {
			let mut output = reader_output.lock().expect("output lock");
			output.push_str(&line);
			output.push('\n');
		}
	});
	let wait = |needle: &str| {
		let deadline = Instant::now() + Duration::from_secs(15);
		while Instant::now() < deadline {
			if output.lock().expect("output lock").contains(needle) {
				return true;
			}
			std::thread::sleep(Duration::from_millis(50));
		}
		false
	};
	let result = (|| {
		if !wait("Outer window is") {
			return Err("xev did not start".to_string());
		}
		let runtime_dir = std::env::var("XDG_RUNTIME_DIR").map_err(|e| e.to_string())?;
		let mut ime = Ime::connect(&Path::new(&runtime_dir).join(&ready.wayland_display));
		ime.commit_text("a");
		if !wait("keysym 0x61, a") {
			return Err("no plain 'a' in xev".to_string());
		}
		ime.commit_text("\u{20ac}");
		if !wait("keysym 0x20ac, EuroSign") {
			return Err("no EuroSign in xev".to_string());
		}
		// After the idle reset the original keymap is back.
		std::thread::sleep(Duration::from_millis(400));
		ime.commit_text("b");
		if !wait("keysym 0x62, b") {
			return Err("no 'b' after the keymap reset".to_string());
		}
		Ok(())
	})();
	let _ = xev.kill();
	let _ = xev.wait();
	let _ = reader.join();
	let log = output.lock().expect("output lock").clone();
	let _ = compositor.stop();
	if let Err(message) = result {
		panic!("{message}; xev output was:\n{log}");
	}
}
