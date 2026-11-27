//! GPU integration test for an output with no window.
//!
//! A streaming client gives up if it receives no video in its first few
//! seconds. An application can take much longer to show its first window. The
//! compositor must send frames before any client connects. This test is
//! ignored because it needs a render node. Run it with:
//!
//! ```text
//! cargo test -p ferriscope --test empty_output -- --ignored
//! ```

use std::{
	path::PathBuf,
	sync::mpsc,
	thread,
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use ferriscope::{Compositor, CompositorConfig, FrameOutput, RecorderOutput};

/// Output width. Small enough for software rendering to keep up.
const WIDTH: u32 = 320;
/// Output height.
const HEIGHT: u32 = 240;
/// The first frame comes immediately; the second follows the one-second static keepalive.
const FRAMES: u64 = 2;
/// Time allowed to receive both frames.
const RUN_TIMEOUT: Duration = Duration::from_secs(5);

struct RecordDir(PathBuf);

impl RecordDir {
	fn create() -> Self {
		let nanos = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.map(|d| d.as_nanos())
			.unwrap_or(0);
		let path = std::env::temp_dir().join(format!("ferriscope-empty-output-{}-{nanos}", std::process::id()));
		std::fs::create_dir_all(&path).expect("create record dir");
		Self(path)
	}
}

impl Drop for RecordDir {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

#[test]
#[ignore = "requires a GPU render node"]
fn an_output_with_no_window_keeps_delivering_black_frames() {
	let record_dir = RecordDir::create();
	let config = CompositorConfig {
		width: WIDTH,
		height: HEIGHT,
		xwayland: false,
		output: FrameOutput::Recorder(RecorderOutput {
			dir: Some(record_dir.0.clone()),
			stop_after_frames: Some(FRAMES),
			..RecorderOutput::default()
		}),
		..CompositorConfig::default()
	};
	let (compositor, _handles) = Compositor::launch(config).expect("launch compositor");
	let stop = compositor.stop_handle();
	let (exited_tx, exited) = mpsc::channel();
	thread::spawn(move || {
		let _ = exited_tx.send(compositor.wait());
	});

	let run = exited.recv_timeout(RUN_TIMEOUT);
	stop.stop();

	run.expect("the compositor stops at its frame limit")
		.expect("compositor run");
	let mut black = format!("P6\n{WIDTH} {HEIGHT}\n255\n").into_bytes();
	black.resize(black.len() + WIDTH as usize * HEIGHT as usize * 3, 0);
	for sequence in 0..FRAMES {
		let frame = std::fs::read(record_dir.0.join(format!("frame_{sequence:06}.ppm"))).expect("recorded frame");
		assert!(frame == black, "frame {sequence} is not black");
	}
}
