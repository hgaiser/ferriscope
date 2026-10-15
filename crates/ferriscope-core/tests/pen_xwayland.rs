//! Pen input reaches an X11 client through Xwayland.
//!
//! Xwayland binds `zwp_tablet_manager_v2` and exposes the tablet and its tools
//! as XI2 slave devices. The test maps a window, selects XI2 events for every
//! device on it and injects pen samples with [`InputEvent::Pen`]; the window
//! must see a button press from the stylus device, and the device list must
//! contain it.
//!
//! It is `#[ignore]`d: it needs a render node and Xwayland. Run it alone:
//!
//! ```text
//! cargo test -p ferriscope-core --test pen_xwayland -- --ignored --nocapture
//! ```

use std::time::{Duration, Instant};

use ferriscope_core::{Compositor, CompositorConfig, InputEvent, PenEvent, PenTool, Ready};
use x11rb::{
	COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT,
	connection::Connection,
	protocol::{
		Event,
		xinput::{self, ConnectionExt as _, XIEventMask},
		xproto::{ConnectionExt as _, CreateWindowAux, EventMask, WindowClass},
	},
};

/// Output width.
const WIDTH: u32 = 320;
/// Output height.
const HEIGHT: u32 = 240;
/// Budget for the `Ready` handshake (Xwayland startup dominates).
const READY_TIMEOUT: Duration = Duration::from_secs(20);
/// Budget for the pen events to surface.
const EVENT_TIMEOUT: Duration = Duration::from_secs(15);
/// Watchdog budget for `Compositor::stop` not to hang.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

fn sample(contact: bool, x: f64, y: f64) -> InputEvent {
	InputEvent::Pen(PenEvent {
		tool: PenTool::Pen,
		x,
		y,
		contact,
		pressure: contact.then_some(0.6),
		distance: (!contact).then_some(0.3),
		tilt: None,
		buttons: 0,
	})
}

#[test]
#[ignore = "requires GPU and Xwayland"]
fn xwayland_exposes_the_tablet_and_delivers_pen_events() {
	let config = CompositorConfig {
		width: WIDTH,
		height: HEIGHT,
		xwayland: true,
		command: None,
		..CompositorConfig::default()
	};
	let (compositor, handles) = Compositor::launch(config).expect("launch compositor");
	let ready: Ready = handles
		.ready
		.recv_timeout(READY_TIMEOUT)
		.expect("ready handshake within timeout");
	let display = ready.xdisplay.expect("xwayland reports an X display");

	let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
		let (conn, screen_num) = x11rb::connect(Some(&format!(":{display}"))).expect("connect to Xwayland");
		let screen = &conn.setup().roots[screen_num];
		let window = conn.generate_id().expect("window id");
		conn.create_window(
			COPY_DEPTH_FROM_PARENT,
			window,
			screen.root,
			0,
			0,
			WIDTH as u16,
			HEIGHT as u16,
			0,
			WindowClass::INPUT_OUTPUT,
			COPY_FROM_PARENT,
			&CreateWindowAux::new()
				.background_pixel(screen.white_pixel)
				.event_mask(EventMask::STRUCTURE_NOTIFY),
		)
		.expect("create window");
		conn.xinput_xi_query_version(2, 2)
			.expect("xi query")
			.reply()
			.expect("xi 2.2");
		conn.xinput_xi_select_events(
			window,
			&[xinput::EventMask {
				deviceid: 1, // XIAllDevices
				mask: vec![
					XIEventMask::MOTION
						| XIEventMask::BUTTON_PRESS
						| XIEventMask::BUTTON_RELEASE
						| XIEventMask::ENTER
						| XIEventMask::LEAVE,
				],
			}],
		)
		.expect("select xi events");
		conn.map_window(window).expect("map window");
		conn.flush().expect("flush");

		let devices = conn
			.xinput_xi_query_device(0u16) // XIAllDevices
			.expect("query devices")
			.reply()
			.expect("device list");
		let names: Vec<String> = devices
			.infos
			.iter()
			.map(|info| String::from_utf8_lossy(&info.name).into_owned())
			.collect();
		println!("XI2 devices: {names:?}");
		let stylus = devices
			.infos
			.iter()
			.find(|info| String::from_utf8_lossy(&info.name).contains("tablet stylus"))
			.map(|info| info.deviceid)
			.unwrap_or_else(|| panic!("no tablet stylus device in {names:?}"));

		// Give the xwm time to map the window, then drive the pen across it.
		std::thread::sleep(Duration::from_secs(1));
		let deadline = Instant::now() + EVENT_TIMEOUT;
		let mut seen_press = false;
		let mut step = 0;
		while Instant::now() < deadline && !seen_press {
			step += 1;
			let x = 100.0 + f64::from(step % 5);
			handles.input.send(sample(false, x, 90.0)).expect("hover");
			handles.input.send(sample(true, x, 90.0)).expect("contact");
			handles.input.send(sample(false, x, 90.0)).expect("lift");
			std::thread::sleep(Duration::from_millis(200));
			while let Some(event) = conn.poll_for_event().expect("poll") {
				match event {
					Event::XinputButtonPress(press) => {
						println!("XI button press from device {}", press.sourceid);
						assert_eq!(press.sourceid, stylus, "the press did not come from the stylus");
						seen_press = true;
					},
					Event::XinputMotion(motion) => println!("XI motion from device {}", motion.sourceid),
					_ => {},
				}
			}
		}
		assert!(seen_press, "no XI2 button press reached the X client");
	}));

	let (done_tx, done_rx) = std::sync::mpsc::channel();
	std::thread::spawn(move || {
		let _ = done_tx.send(compositor.stop());
	});
	let stopped = done_rx.recv_timeout(STOP_TIMEOUT).is_ok_and(|result| result.is_ok());
	if let Err(panic) = outcome {
		std::panic::resume_unwind(panic);
	}
	assert!(stopped, "compositor stopped cleanly");
}
