//! GPU integration test for the Steam-facing capabilities ferriscope advertises
//! over Xwayland: XRandR mode emulation (`-force-xrandr-emulation`, gamescope's
//! `force_xrandr_emulation`, `wlserver.cpp:1878`) and Steam's keyboard layout
//! (`GAMESCOPE_KEYBOARD_LAYOUT`, `steamcompmgr.cpp:7364`).
//!
//! It is `#[ignore]`d: it needs a render node and Xwayland.
//!
//! ```text
//! cargo test -p ferriscope-core --test steam_capabilities -- --ignored --nocapture
//! ```

use std::{
	thread,
	time::{Duration, Instant},
};

use ferriscope_core::{Compositor, CompositorConfig, KeyboardConfig};
use x11rb::{
	connection::Connection,
	protocol::{randr::ConnectionExt as _, xproto::*},
	rust_connection::RustConnection,
	wrapper::ConnectionExt as _,
};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const READY_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_SLICE: Duration = Duration::from_millis(50);
const KEYMAP_TIMEOUT: Duration = Duration::from_secs(10);
/// The X keycode of `KEY_Q` (evdev 16 + 8).
const KEY_Q: u8 = 16 + 8;

fn launch(keyboard: KeyboardConfig) -> (Compositor, RustConnection) {
	let config = CompositorConfig {
		width: WIDTH,
		height: HEIGHT,
		xwayland: true,
		seat: ferriscope_core::SeatConfig {
			keyboard,
			..ferriscope_core::SeatConfig::default()
		},
		..CompositorConfig::default()
	};
	let (compositor, handles) = Compositor::launch(config).expect("launch compositor");
	let ready = handles.ready.recv_timeout(READY_TIMEOUT).expect("ready handshake");
	let display = ready.xdisplay.expect("Xwayland display");
	let (conn, _) = x11rb::connect(Some(&format!(":{display}"))).expect("connect to Xwayland");
	(compositor, conn)
}

/// The keysym the `KEY_Q` key produces at level 0 of the first group.
fn keysym_of_q(conn: &RustConnection) -> u32 {
	let reply = conn
		.get_keyboard_mapping(KEY_Q, 1)
		.expect("get_keyboard_mapping")
		.reply()
		.expect("keyboard mapping reply");
	reply.keysyms[0]
}

fn wait_for_q(conn: &RustConnection, keysym: u32) -> bool {
	let deadline = Instant::now() + KEYMAP_TIMEOUT;
	while Instant::now() < deadline {
		if keysym_of_q(conn) == keysym {
			return true;
		}
		thread::sleep(POLL_SLICE);
	}
	false
}

#[test]
#[ignore = "requires GPU and Xwayland"]
fn xrandr_lists_emulated_modes_and_accepts_one() {
	let (compositor, conn) = launch(KeyboardConfig::default());
	let root = conn.setup().roots[0].root;
	let resources = conn
		.randr_get_screen_resources(root)
		.expect("get_screen_resources")
		.reply()
		.expect("resources reply");

	let native = resources
		.modes
		.iter()
		.find(|mode| u32::from(mode.width) == WIDTH && u32::from(mode.height) == HEIGHT)
		.expect("the native mode is listed");
	let emulated = resources
		.modes
		.iter()
		.find(|mode| mode.id != native.id)
		.expect("emulated modes are listed beside the native one");

	let crtc = resources.crtcs[0];
	let info = conn
		.randr_get_crtc_info(crtc, resources.config_timestamp)
		.expect("get_crtc_info")
		.reply()
		.expect("crtc info reply");
	let status = conn
		.randr_set_crtc_config(
			crtc,
			x11rb::CURRENT_TIME,
			resources.config_timestamp,
			info.x,
			info.y,
			emulated.id,
			info.rotation,
			&info.outputs,
		)
		.expect("set_crtc_config")
		.reply()
		.expect("set_crtc_config reply")
		.status;
	assert_eq!(
		status,
		x11rb::protocol::randr::SetConfig::SUCCESS,
		"an emulated mode is accepted"
	);

	let after = conn
		.randr_get_crtc_info(crtc, x11rb::CURRENT_TIME)
		.expect("get_crtc_info")
		.reply()
		.expect("crtc info reply");
	assert_eq!(after.mode, emulated.id, "the client sees the emulated mode as current");

	compositor.stop().expect("stop");
}

#[test]
#[ignore = "requires GPU and Xwayland"]
fn keyboard_layout_follows_config_and_steam() {
	let (compositor, conn) = launch(KeyboardConfig {
		layout: "us".into(),
		..KeyboardConfig::default()
	});
	let atom = conn
		.intern_atom(false, b"GAMESCOPE_KEYBOARD_LAYOUT")
		.expect("intern")
		.reply()
		.expect("intern reply")
		.atom;
	let root = conn.setup().roots[0].root;

	assert!(wait_for_q(&conn, u32::from(b'q')), "the configured us layout types q");

	// Steam pushes a French layout: the key at Q's position types `a`.
	conn.change_property8(PropMode::REPLACE, root, atom, AtomEnum::STRING, b"fr")
		.expect("set layout");
	conn.flush().expect("flush");
	assert!(
		wait_for_q(&conn, u32::from(b'a')),
		"GAMESCOPE_KEYBOARD_LAYOUT=fr switches the layout"
	);

	// An empty value returns to the configured base.
	conn.change_property8(PropMode::REPLACE, root, atom, AtomEnum::STRING, b"")
		.expect("clear layout");
	conn.flush().expect("flush");
	assert!(
		wait_for_q(&conn, u32::from(b'q')),
		"an empty value restores the base layout"
	);

	compositor.stop().expect("stop");
}

/// Proton's winex11 takes gamescope's code paths (no pointer-grab dance on
/// focus changes, direct foreground, no iconify) only when the window manager
/// is named `steamcompmgr` on the `_NET_SUPPORTING_WM_CHECK` window; without
/// them a Proton game stopped seeing mouse buttons.
#[test]
#[ignore = "requires GPU and Xwayland"]
fn the_window_manager_is_named_steamcompmgr() {
	let (compositor, conn) = launch(KeyboardConfig::default());
	let root = conn.setup().roots[0].root;
	let atom = |name: &[u8]| conn.intern_atom(false, name).unwrap().reply().unwrap().atom;
	let check = conn
		.get_property(false, root, atom(b"_NET_SUPPORTING_WM_CHECK"), AtomEnum::WINDOW, 0, 1)
		.unwrap()
		.reply()
		.unwrap()
		.value32()
		.and_then(|mut values| values.next())
		.expect("a _NET_SUPPORTING_WM_CHECK window");
	let name = conn
		.get_property(false, check, atom(b"_NET_WM_NAME"), atom(b"UTF8_STRING"), 0, 64)
		.unwrap()
		.reply()
		.unwrap()
		.value;
	assert_eq!(String::from_utf8_lossy(&name), "steamcompmgr");
	let _ = compositor.stop();
}
