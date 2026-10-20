//! GPU integration test for the `focus` gate.
//!
//! The focus contract: at all times exactly one fullscreen window is visible
//! and it is the one focused; transients, popups, override-redirect windows and
//! notifications are drawn over it and never take focus; when it goes away
//! focus is cleared, never left dangling.
//!
//! This test exercises the X11 (Xwayland) side with the tooling available on
//! the host — `xev`, `xdotool`, `xprop` — and creates the override-redirect
//! window through `x11rb` (re-exported by smithay, so no new dependency):
//!
//! - **focus on map**: the first X11 window becomes `_NET_ACTIVE_WINDOW`; its
//!   `_NET_WM_STATE` is left to the client, as under gamescope;
//! - **activation**: `xdotool windowactivate` (`_NET_ACTIVE_WINDOW` as a
//!   `ClientMessage`) keeps the window active;
//! - **override-redirect**: an override-redirect window maps over the window
//!   and does not take focus (`_NET_ACTIVE_WINDOW` and input focus are
//!   unchanged);
//! - **dialog**: a parented transient/dialog takes `_NET_ACTIVE_WINDOW` and the
//!   X input focus and is not made fullscreen; unmapping it returns focus to
//!   the window;
//! - **interactive overlay**: a narrow `STEAM_OVERLAY` without
//!   `STEAM_INPUT_FOCUS` is a notification and does not take focus, while one
//!   with `STEAM_INPUT_FOCUS` takes `_NET_ACTIVE_WINDOW` and input focus and
//!   returns focus to the window on unmap;
//! - **no dangling focus / recovery**: destroying the window clears input
//!   focus, and the next normal window takes the single fullscreen-window role
//!   (which only happens if the role was released).
//!
//! Pointer lock/confine is **not** tested here: it needs a native Wayland client. That
//! half lives in `tests/wayland_focus.rs`, which uses the test-only
//! `wayland-client` dev-dependency. Pointer `confine` region clamping remains
//! untested; the pure capture state machine is unit-tested in `focus.rs`.
//!
//! It is `#[ignore]`d: it needs a render node, Xwayland, `xev`, `xdotool` and
//! `xprop`. Run it alone:
//!
//! ```text
//! cargo test -p ferriscope --test focus_policy -- --ignored --exact x11_focus_policy --nocapture
//! ```

use std::{
	process::{Child, Command, Stdio},
	sync::mpsc,
	time::{Duration, Instant},
};

use ferriscope::{Compositor, CompositorConfig, CompositorHandles, Ready};
use smithay::reexports::x11rb::{
	self, COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, connection::Connection, protocol::xproto::*,
	rust_connection::RustConnection, wrapper::ConnectionExt as _,
};

/// Output width, small enough that the software render fallback keeps up.
const WIDTH: u32 = 320;
/// Output height.
const HEIGHT: u32 = 240;
/// Budget for the `Ready` handshake (Xwayland startup dominates).
const READY_TIMEOUT: Duration = Duration::from_secs(20);
/// Budget for `xev`'s window to appear and be focused.
const MAP_TIMEOUT: Duration = Duration::from_secs(20);
/// Budget for focus to clear after the window goes away.
const UNMAP_TIMEOUT: Duration = Duration::from_secs(15);
/// Watchdog budget for `Compositor::stop` not to hang.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// Poll slice while waiting for X state.
const POLL_SLICE: Duration = Duration::from_millis(50);
/// Settle time after mapping the override-redirect window before checking focus.
const OVERRIDE_SETTLE: Duration = Duration::from_millis(600);
/// Budget for a dialog or interactive overlay to take focus, and for focus to
/// return to the window.
const FOCUS_TIMEOUT: Duration = Duration::from_secs(15);

/// Kills `xev` and stops the compositor on drop, so a failed assertion does not
/// leak a compositor thread or an `xev`.
struct FocusGuard {
	compositor: Option<Compositor>,
	children: Vec<Child>,
}

impl FocusGuard {
	/// Kill the clients, then stop the compositor under a watchdog.
	fn stop(&mut self, timeout: Duration) -> bool {
		self.kill_children();
		let (status_tx, status_rx) = mpsc::channel();
		let compositor = self.compositor.take();
		let stopper = std::thread::spawn(move || {
			let result = match compositor {
				Some(compositor) => compositor.stop(),
				None => Ok(()),
			};
			let _ = status_tx.send(result);
		});
		match status_rx.recv_timeout(timeout) {
			Ok(result) => {
				let _ = stopper.join();
				result.is_ok()
			},
			Err(_) => false,
		}
	}

	/// Kill and reap every client.
	fn kill_children(&mut self) {
		for mut child in self.children.drain(..) {
			let _ = child.kill();
			let _ = child.wait();
		}
		// Safety net: nothing should be left, but never leak a client.
		kill_exact("xev");
	}
}

impl Drop for FocusGuard {
	fn drop(&mut self) {
		let _ = self.stop(STOP_TIMEOUT);
	}
}

/// Kill this test process's children with the exact name `name`.
///
/// `-x` forbids substring matches and `-P` limits the kill to our own
/// children, so concurrent runs (other test binaries, a desktop session) keep
/// their own Xwayland and clients.
fn kill_exact(name: &str) {
	let parent = std::process::id().to_string();
	let _ = Command::new("pkill")
		.args(["-9", "-x", "-P", &parent, name])
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.status();
}

/// Run an X11 tool against display `:N`, returning trimmed stdout on success.
fn x_tool(display: u32, program: &str, args: &[&str]) -> Option<String> {
	let output = Command::new(program)
		.env("DISPLAY", format!(":{display}"))
		.args(args)
		.stdout(Stdio::piped())
		.stderr(Stdio::null())
		.output()
		.ok()?;
	if !output.status.success() {
		return None;
	}
	Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The current `_NET_ACTIVE_WINDOW`, if any.
fn active_window(display: u32) -> Option<String> {
	x_tool(display, "xdotool", &["getactivewindow"]).filter(|id| !id.is_empty() && id != "0")
}

/// The window with input focus, if any.
fn focused_window(display: u32) -> Option<String> {
	x_tool(display, "xdotool", &["getwindowfocus"]).filter(|id| !id.is_empty() && id != "0")
}

/// The `(width, height)` of `window` as `xdotool` reports it.
fn window_size(display: u32, window: &str) -> Option<(i32, i32)> {
	let output = x_tool(display, "xdotool", &["getwindowgeometry", "--shell", window])?;
	let mut width = None;
	let mut height = None;
	for line in output.lines() {
		if let Some(value) = line.strip_prefix("WIDTH=") {
			width = value.trim().parse().ok();
		}
		if let Some(value) = line.strip_prefix("HEIGHT=") {
			height = value.trim().parse().ok();
		}
	}
	match (width, height) {
		(Some(width), Some(height)) => Some((width, height)),
		_ => None,
	}
}

/// Wait, bounded, for `window` to reach `want`; returns the last seen size on
/// timeout for the assertion message.
fn wait_for_window_size(
	display: u32,
	window: &str,
	want: (i32, i32),
	timeout: Duration,
) -> Result<(), Option<(i32, i32)>> {
	let deadline = Instant::now() + timeout;
	loop {
		let size = window_size(display, window);
		if size == Some(want) {
			return Ok(());
		}
		if Instant::now() >= deadline {
			return Err(size);
		}
		std::thread::sleep(POLL_SLICE);
	}
}

/// Whether `_NET_WM_STATE` of `window` contains `state`.
fn has_net_wm_state(display: u32, window: &str, state: &str) -> bool {
	x_tool(display, "xprop", &["-id", window, "_NET_WM_STATE"])
		.map(|value| value.contains(state))
		.unwrap_or(false)
}

/// Wait, bounded, for a visible `xev` window to appear.
fn wait_for_xev_window(display: u32, timeout: Duration) -> Option<String> {
	let deadline = Instant::now() + timeout;
	loop {
		if let Some(ids) = x_tool(
			display,
			"xdotool",
			&["search", "--onlyvisible", "--name", "Event Tester"],
		) {
			if let Some(id) = ids.lines().next() {
				let id = id.trim().to_string();
				if !id.is_empty() {
					return Some(id);
				}
			}
		}
		if Instant::now() >= deadline {
			return None;
		}
		std::thread::sleep(POLL_SLICE);
	}
}

/// Wait, bounded, for input focus to no longer name `window`.
fn wait_for_focus_away(display: u32, window: &str, timeout: Duration) -> bool {
	let deadline = Instant::now() + timeout;
	loop {
		if focused_window(display).as_deref() != Some(window) {
			return true;
		}
		if Instant::now() >= deadline {
			return false;
		}
		std::thread::sleep(POLL_SLICE);
	}
}

/// Wait, bounded, for `_NET_ACTIVE_WINDOW` to become `window`.
fn wait_for_active(display: u32, window: &str, timeout: Duration) -> bool {
	let deadline = Instant::now() + timeout;
	loop {
		if active_window(display).as_deref() == Some(window) {
			return true;
		}
		if Instant::now() >= deadline {
			return false;
		}
		std::thread::sleep(POLL_SLICE);
	}
}

/// Wait, bounded, for the X input focus to become `window`.
fn wait_for_focused(display: u32, window: &str, timeout: Duration) -> bool {
	let deadline = Instant::now() + timeout;
	loop {
		if focused_window(display).as_deref() == Some(window) {
			return true;
		}
		if Instant::now() >= deadline {
			return false;
		}
		std::thread::sleep(POLL_SLICE);
	}
}

/// Create and map an override-redirect window on `:N`, staying connected for as
/// long as the returned connection lives.
fn create_override_redirect_window(display: u32) -> x11rb::rust_connection::RustConnection {
	let (conn, screen_num) = x11rb::connect(Some(&format!(":{display}"))).expect("connect to Xwayland");
	let screen = &conn.setup().roots[screen_num];
	let window = conn.generate_id().expect("window id");
	conn.create_window(
		COPY_DEPTH_FROM_PARENT,
		window,
		screen.root,
		10,
		10,
		80,
		60,
		0,
		WindowClass::INPUT_OUTPUT,
		COPY_FROM_PARENT,
		&CreateWindowAux::new()
			.override_redirect(1)
			.background_pixel(screen.white_pixel)
			.event_mask(EventMask::STRUCTURE_NOTIFY),
	)
	.expect("create override-redirect window");
	conn.map_window(window).expect("map override-redirect window");
	conn.flush().expect("flush");
	conn
}

/// Create and map a dialog (a parented transient) on `:N`.
///
/// Sets `WM_TRANSIENT_FOR` and `_NET_WM_WINDOW_TYPE_DIALOG`, the order a real
/// dialog uses (properties before the map request). The returned connection
/// must stay alive for the window to exist.
fn create_dialog_window(display: u32, parent: &str) -> (RustConnection, Window) {
	let (conn, screen_num) = x11rb::connect(Some(&format!(":{display}"))).expect("connect to Xwayland");
	let screen = &conn.setup().roots[screen_num];
	let window = conn.generate_id().expect("dialog window id");
	conn.create_window(
		COPY_DEPTH_FROM_PARENT,
		window,
		screen.root,
		40,
		40,
		200,
		150,
		0,
		WindowClass::INPUT_OUTPUT,
		COPY_FROM_PARENT,
		&CreateWindowAux::new()
			.background_pixel(screen.white_pixel)
			.event_mask(EventMask::STRUCTURE_NOTIFY),
	)
	.expect("create dialog window");
	let transient_for = conn
		.intern_atom(false, b"WM_TRANSIENT_FOR")
		.expect("intern")
		.reply()
		.expect("reply")
		.atom;
	let window_type = conn
		.intern_atom(false, b"_NET_WM_WINDOW_TYPE")
		.expect("intern")
		.reply()
		.expect("reply")
		.atom;
	let dialog = conn
		.intern_atom(false, b"_NET_WM_WINDOW_TYPE_DIALOG")
		.expect("intern")
		.reply()
		.expect("reply")
		.atom;
	let parent: Window = parent.parse().expect("parent window id");
	conn.change_property32(PropMode::REPLACE, window, transient_for, AtomEnum::WINDOW, &[parent])
		.expect("set WM_TRANSIENT_FOR");
	conn.change_property32(PropMode::REPLACE, window, window_type, AtomEnum::ATOM, &[dialog])
		.expect("set _NET_WM_WINDOW_TYPE");
	conn.map_window(window).expect("map dialog window");
	conn.flush().expect("flush");
	(conn, window)
}

/// Create and map a `STEAM_OVERLAY` window on `:N`, optionally with
/// `STEAM_INPUT_FOCUS`.
fn create_steam_overlay_window(display: u32, width: u16, input_focus: bool) -> (RustConnection, Window) {
	let (conn, screen_num) = x11rb::connect(Some(&format!(":{display}"))).expect("connect to Xwayland");
	let screen = &conn.setup().roots[screen_num];
	let window = conn.generate_id().expect("overlay window id");
	conn.create_window(
		COPY_DEPTH_FROM_PARENT,
		window,
		screen.root,
		10,
		10,
		width,
		60,
		0,
		WindowClass::INPUT_OUTPUT,
		COPY_FROM_PARENT,
		&CreateWindowAux::new()
			.background_pixel(screen.white_pixel)
			.event_mask(EventMask::STRUCTURE_NOTIFY),
	)
	.expect("create overlay window");
	let overlay = conn
		.intern_atom(false, b"STEAM_OVERLAY")
		.expect("intern")
		.reply()
		.expect("reply")
		.atom;
	conn.change_property32(PropMode::REPLACE, window, overlay, AtomEnum::CARDINAL, &[1])
		.expect("set STEAM_OVERLAY");
	if input_focus {
		let input = conn
			.intern_atom(false, b"STEAM_INPUT_FOCUS")
			.expect("intern")
			.reply()
			.expect("reply")
			.atom;
		conn.change_property32(PropMode::REPLACE, window, input, AtomEnum::CARDINAL, &[1])
			.expect("set STEAM_INPUT_FOCUS");
	}
	conn.map_window(window).expect("map overlay window");
	conn.flush().expect("flush");
	(conn, window)
}

#[test]
#[ignore = "requires GPU, Xwayland, xev, xdotool and xprop"]
fn x11_focus_policy() {
	let config = CompositorConfig {
		width: WIDTH,
		height: HEIGHT,
		xwayland: true,
		command: None,
		..CompositorConfig::default()
	};

	let (compositor, handles): (Compositor, CompositorHandles) = Compositor::launch(config).expect("launch compositor");
	let mut guard = FocusGuard {
		compositor: Some(compositor),
		children: Vec::new(),
	};

	let ready: Ready = handles
		.ready
		.recv_timeout(READY_TIMEOUT)
		.expect("ready handshake within timeout");
	let xdisplay = ready.xdisplay.expect("xwayland reports an X display");

	// Spawn `xev`; the xwm maps it fullscreen and focuses it.
	guard.children.push(spawn_xev(xdisplay));

	let xev_window = wait_for_xev_window(xdisplay, MAP_TIMEOUT)
		.unwrap_or_else(|| panic!("xev window did not appear within {MAP_TIMEOUT:?}"));

	// Focus on map: the first window is the active, focused window.
	assert!(
		wait_for_active(xdisplay, &xev_window, MAP_TIMEOUT),
		"first X11 window did not become _NET_ACTIVE_WINDOW within {MAP_TIMEOUT:?}"
	);
	// `_NET_WM_STATE` is the client's, as under gamescope: Proton under
	// `steamcompmgr` waits for exactly the state it asked for, and an added
	// FOCUSED or FULLSCREEN kept it from restoring a minimized game.
	assert!(
		!has_net_wm_state(xdisplay, &xev_window, "_NET_WM_STATE_FOCUSED"),
		"the compositor wrote _NET_WM_STATE_FOCUSED on the client's window"
	);
	assert!(
		!has_net_wm_state(xdisplay, &xev_window, "_NET_WM_STATE_FULLSCREEN"),
		"the compositor wrote _NET_WM_STATE_FULLSCREEN on the client's window"
	);

	// Activation request (`_NET_ACTIVE_WINDOW` ClientMessage): still active.
	if x_tool(xdisplay, "xdotool", &["windowactivate", &xev_window]).is_some() {
		assert!(
			wait_for_active(xdisplay, &xev_window, MAP_TIMEOUT),
			"windowactivate did not leave the window active"
		);
	}

	// Override-redirect windows are drawn over the window and never take focus.
	let _override = create_override_redirect_window(xdisplay);
	std::thread::sleep(OVERRIDE_SETTLE);
	assert_eq!(
		active_window(xdisplay).as_deref(),
		Some(xev_window.as_str()),
		"an override-redirect window took _NET_ACTIVE_WINDOW"
	);
	assert_eq!(
		focused_window(xdisplay).as_deref(),
		Some(xev_window.as_str()),
		"an override-redirect window took input focus"
	);

	// A parented transient/dialog takes keyboard focus and `_NET_ACTIVE_WINDOW`
	// while the window keeps the fullscreen role; unmapping it returns focus to
	// the window.
	let (dialog_conn, dialog) = create_dialog_window(xdisplay, &xev_window);
	let dialog_id = format!("{dialog}");
	assert!(
		wait_for_active(xdisplay, &dialog_id, FOCUS_TIMEOUT),
		"the dialog did not take _NET_ACTIVE_WINDOW within {FOCUS_TIMEOUT:?}"
	);
	assert!(
		wait_for_focused(xdisplay, &dialog_id, FOCUS_TIMEOUT),
		"the dialog did not take the X input focus within {FOCUS_TIMEOUT:?}"
	);
	assert!(
		!has_net_wm_state(xdisplay, &format!("0x{dialog:x}"), "_NET_WM_STATE_FULLSCREEN"),
		"the dialog became a fullscreen window"
	);
	dialog_conn.unmap_window(dialog).expect("unmap dialog");
	dialog_conn.flush().expect("flush");
	assert!(
		wait_for_active(xdisplay, &xev_window, FOCUS_TIMEOUT),
		"focus did not return to the window after the dialog unmapped within {FOCUS_TIMEOUT:?}"
	);
	assert!(
		wait_for_focused(xdisplay, &xev_window, FOCUS_TIMEOUT),
		"input focus did not return to the window after the dialog unmapped within {FOCUS_TIMEOUT:?}"
	);

	// A narrow STEAM_OVERLAY without STEAM_INPUT_FOCUS is a notification and
	// never takes focus; with STEAM_INPUT_FOCUS it is the interactive overlay
	// and takes focus, returning it to the window on unmap.
	let (notification_conn, notification) = create_steam_overlay_window(xdisplay, 80, false);
	std::thread::sleep(OVERRIDE_SETTLE);
	assert_eq!(
		active_window(xdisplay).as_deref(),
		Some(xev_window.as_str()),
		"a narrow notification took _NET_ACTIVE_WINDOW"
	);
	assert_eq!(
		focused_window(xdisplay).as_deref(),
		Some(xev_window.as_str()),
		"a narrow notification took input focus"
	);
	notification_conn
		.unmap_window(notification)
		.expect("unmap notification");
	notification_conn.flush().expect("flush");

	let (interactive_conn, interactive) = create_steam_overlay_window(xdisplay, 80, true);
	let interactive_id = format!("{interactive}");
	assert!(
		wait_for_active(xdisplay, &interactive_id, FOCUS_TIMEOUT),
		"the interactive overlay did not take _NET_ACTIVE_WINDOW within {FOCUS_TIMEOUT:?}"
	);
	assert!(
		wait_for_focused(xdisplay, &interactive_id, FOCUS_TIMEOUT),
		"the interactive overlay did not take the X input focus within {FOCUS_TIMEOUT:?}"
	);
	interactive_conn
		.unmap_window(interactive)
		.expect("unmap interactive overlay");
	interactive_conn.flush().expect("flush");
	assert!(
		wait_for_active(xdisplay, &xev_window, FOCUS_TIMEOUT),
		"focus did not return to the window after the interactive overlay unmapped within {FOCUS_TIMEOUT:?}"
	);

	// Destroying the window must clear input focus, not leave it dangling.
	guard.kill_children();
	assert!(
		wait_for_focus_away(xdisplay, &xev_window, UNMAP_TIMEOUT),
		"input focus still named the destroyed window after {UNMAP_TIMEOUT:?}"
	);
	// Give the event loop a beat to process the destroy before the next window
	// maps, so this tests recovery rather than racing it.
	std::thread::sleep(OVERRIDE_SETTLE);

	// The single-window role must have been released with it: the next normal
	// window becomes active and focused. This only holds if the
	// previous window's role was cleared (`focus::FocusState::unmap`).
	guard.children.push(spawn_xev(xdisplay));
	let next_window = wait_for_xev_window(xdisplay, MAP_TIMEOUT)
		.unwrap_or_else(|| panic!("the second xev window did not appear within {MAP_TIMEOUT:?}"));
	assert!(
		wait_for_active(xdisplay, &next_window, MAP_TIMEOUT),
		"the next window did not take _NET_ACTIVE_WINDOW within {MAP_TIMEOUT:?}"
	);
	assert_eq!(
		focused_window(xdisplay).as_deref(),
		Some(next_window.as_str()),
		"the next window did not receive input focus"
	);

	assert!(guard.stop(STOP_TIMEOUT), "compositor stopped cleanly");
}

/// The below-output behaviour: the single-window role holder keeps
/// the size the client asked for (gamescope scales the committed content at
/// composite time instead of resizing the window), and a parented dialog and a
/// Steam overlay/notification keep their own size too. The composited fill is
/// asserted by `tests/output_fill.rs`.
///
/// Run it alone:
///
/// ```text
/// cargo test -p ferriscope --test focus_policy -- --ignored --exact x11_below_output_window_keeps_its_size --nocapture
/// ```
#[test]
#[ignore = "requires GPU, Xwayland, xev, xdotool and xprop"]
fn x11_below_output_window_keeps_its_size() {
	let config = CompositorConfig {
		width: WIDTH,
		height: HEIGHT,
		xwayland: true,
		command: None,
		..CompositorConfig::default()
	};

	let (compositor, handles): (Compositor, CompositorHandles) = Compositor::launch(config).expect("launch compositor");
	let mut guard = FocusGuard {
		compositor: Some(compositor),
		children: Vec::new(),
	};

	let ready: Ready = handles
		.ready
		.recv_timeout(READY_TIMEOUT)
		.expect("ready handshake within timeout");
	let xdisplay = ready.xdisplay.expect("xwayland reports an X display");

	// (a) A window that maps below the output keeps its own size; the compositor
	// scales its content to fill the output, it is not resized.
	guard.children.push(spawn_xev_at(xdisplay, "160x120"));
	let window = wait_for_xev_window(xdisplay, MAP_TIMEOUT)
		.unwrap_or_else(|| panic!("xev window did not appear within {MAP_TIMEOUT:?}"));
	assert!(
		wait_for_active(xdisplay, &window, MAP_TIMEOUT),
		"the below-output window did not become active"
	);
	wait_for_window_size(xdisplay, &window, (160, 120), MAP_TIMEOUT).unwrap_or_else(|size| {
		panic!("the role holder was resized instead of kept at its own size: {size:?} (want (160, 120))")
	});

	// (b) A ConfigureRequest for a smaller size is honoured: the client keeps
	// control of its own geometry and the compositor scales whatever it commits.
	assert!(
		x_tool(xdisplay, "xdotool", &["windowsize", &window, "80", "60"]).is_some(),
		"xdotool windowsize failed"
	);
	wait_for_window_size(xdisplay, &window, (80, 60), MAP_TIMEOUT)
		.unwrap_or_else(|size| panic!("a client ConfigureRequest was not honoured: {size:?} (want (80, 60))"));

	// (c) A parented transient/dialog keeps its requested size.
	let (dialog_conn, dialog) = create_dialog_window(xdisplay, &window);
	let dialog_id = format!("{dialog}");
	wait_for_window_size(xdisplay, &dialog_id, (200, 150), FOCUS_TIMEOUT)
		.unwrap_or_else(|size| panic!("a parented dialog did not keep its requested size: {size:?} (want (200, 150))"));
	dialog_conn.unmap_window(dialog).expect("unmap dialog");
	dialog_conn.flush().expect("flush");

	// (d) A Steam overlay/notification keeps its size.
	let (notification_conn, notification) = create_steam_overlay_window(xdisplay, 80, false);
	let notification_id = format!("{notification}");
	wait_for_window_size(xdisplay, &notification_id, (80, 60), FOCUS_TIMEOUT)
		.unwrap_or_else(|size| panic!("a notification did not keep its size: {size:?} (want (80, 60))"));
	notification_conn
		.unmap_window(notification)
		.expect("unmap notification");
	notification_conn.flush().expect("flush");

	assert!(guard.stop(STOP_TIMEOUT), "compositor stopped cleanly");
}

/// Spawn `xev` against display `:N`.
fn spawn_xev(display: u32) -> Child {
	let mut xev = Command::new("xev");
	xev.env("DISPLAY", format!(":{display}"))
		.stdout(Stdio::null())
		.stderr(Stdio::null());
	if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") {
		xev.env("XDG_RUNTIME_DIR", runtime_dir);
	}
	xev.spawn().expect("spawn xev")
}

/// Spawn `xev` at a requested initial geometry, e.g. below the output size.
fn spawn_xev_at(display: u32, geometry: &str) -> Child {
	let mut xev = Command::new("xev");
	xev.env("DISPLAY", format!(":{display}"))
		.args(["-geometry", geometry])
		.stdout(Stdio::null())
		.stderr(Stdio::null());
	if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") {
		xev.env("XDG_RUNTIME_DIR", runtime_dir);
	}
	xev.spawn().expect("spawn xev")
}
