//! GPU integration test for Steam's screenshot request on the DMA-BUF channel
//! output, the path Moonshine uses.
//!
//! Steam asks for a screenshot by setting `GAMESCOPECTRL_REQUEST_SCREENSHOT` on
//! the X root window. The compositor must write `/tmp/gamescope.png` and delete
//! the property, or Steam shows no popup and stores nothing. The test sets the
//! property the way Steam does (`x11rb`, no real Steam) and waits for the
//! answer.
//!
//! It uses the fixed path `/tmp/gamescope.png`, so it must not run beside a
//! real gamescope or ferriscope session. It is `#[ignore]`d: it needs a render
//! node, Xwayland and `xev`.
//!
//! ```text
//! cargo test -p ferriscope --test steam_screenshot -- --ignored --nocapture
//! ```

use std::{
	fs::File,
	path::Path,
	thread,
	time::{Duration, Instant, SystemTime},
};

use ferriscope::{ChannelOutput, Compositor, CompositorConfig, FrameOutput};
use smithay::reexports::x11rb::{
	self,
	connection::Connection,
	protocol::xproto::{AtomEnum, ConnectionExt, PropMode},
	wrapper::ConnectionExt as _,
};

static FRAMES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;
const SCREENSHOT_PATH: &str = "/tmp/gamescope.png";
const READY_TIMEOUT: Duration = Duration::from_secs(20);
const ANSWER_TIMEOUT: Duration = Duration::from_secs(15);

#[test]
#[ignore = "needs a render node, Xwayland and xev"]
fn steam_screenshot_request_writes_a_png_and_clears_the_property() {
	let _ = tracing_subscriber::fmt().with_env_filter("ferriscope=debug").try_init();
	let _ = std::fs::remove_file(SCREENSHOT_PATH);
	let started = SystemTime::now();

	let config = CompositorConfig {
		width: WIDTH,
		height: HEIGHT,
		xwayland: true,
		command: Some(vec!["xev".into()]),
		output: FrameOutput::Channel(ChannelOutput::default()),
		..CompositorConfig::default()
	};
	let (compositor, handles) = Compositor::launch(config).expect("launch compositor");
	let ready = handles.ready.recv_timeout(READY_TIMEOUT).expect("ready handshake");
	let display = ready.xdisplay.expect("an X display");

	let (conn, screen) = x11rb::connect(Some(&format!(":{display}"))).expect("connect to Xwayland");
	let root = conn.setup().roots[screen].root;
	let request = conn
		.intern_atom(false, b"GAMESCOPECTRL_REQUEST_SCREENSHOT")
		.unwrap()
		.reply()
		.unwrap()
		.atom;

	// Consume frames like a stream does, so the pool never stalls, and give
	// the compositor a moment to settle before the request.
	let drain_until = |deadline: Instant| {
		while Instant::now() < deadline {
			if handles.frames.recv_timeout(Duration::from_millis(20)).is_ok() {
				FRAMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
			}
		}
	};
	drain_until(Instant::now() + Duration::from_secs(2));

	conn.change_property32(PropMode::REPLACE, root, request, AtomEnum::CARDINAL, &[1])
		.unwrap();
	conn.flush().unwrap();

	let deadline = Instant::now() + ANSWER_TIMEOUT;
	let mut cleared = false;
	while Instant::now() < deadline {
		drain_until(Instant::now() + Duration::from_millis(100));
		let reply = conn
			.get_property(false, root, request, AtomEnum::ANY, 0, 1)
			.unwrap()
			.reply()
			.unwrap();
		if reply.value_len == 0 {
			cleared = true;
			break;
		}
		thread::sleep(Duration::from_millis(10));
	}
	assert!(
		cleared,
		"the compositor never deleted the request property (frames seen: {})",
		FRAMES.load(std::sync::atomic::Ordering::Relaxed)
	);

	let metadata = std::fs::metadata(SCREENSHOT_PATH).expect("the screenshot file exists");
	assert!(metadata.modified().unwrap() >= started, "the screenshot is stale");
	let decoder = png::Decoder::new(std::io::BufReader::new(File::open(Path::new(SCREENSHOT_PATH)).unwrap()));
	let reader = decoder.read_info().expect("a valid PNG");
	let info = reader.info();
	assert_eq!((info.width, info.height), (WIDTH, HEIGHT));

	let _ = compositor.stop();
}
