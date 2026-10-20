//! Runnable example of the in-process library API.
//!
//! Launches the compositor on its own thread, spawns `vkcube` against the
//! resulting Wayland socket, consumes frames from the bounded channel, and
//! drops each DMA-BUF frame to return its pool slot, until 60 frames or ~10 s
//! elapse.
//!
//! Builds with `cargo build --offline --example stream`; it is not run in CI.

use std::{
	process::{Child, Command, Stdio},
	time::{Duration, Instant},
};

use ferriscope::{Compositor, CompositorConfig, CompositorHandles, Error, FrameData, RecvTimeoutError, Result};

/// Output width in pixels.
const WIDTH: u32 = 640;
/// Output height in pixels.
const HEIGHT: u32 = 480;

fn main() -> Result<()> {
	let config = CompositorConfig {
		width: WIDTH,
		height: HEIGHT,
		xwayland: false,
		command: None,
		..CompositorConfig::default()
	};

	let (compositor, handles) = Compositor::launch(config)?;

	// The socket is live once the one-shot handshake fires; children need its name.
	let ready = handles
		.ready
		.recv_timeout(Duration::from_secs(5))
		.map_err(|e| Error::Wayland(format!("ready handshake: {e}")))?;
	println!("compositor ready: WAYLAND_DISPLAY={}", ready.wayland_display);

	let mut child = spawn_vkcube(&ready.wayland_display)?;
	let summary = consume(&handles, Duration::from_secs(10), 60);

	let stop = compositor.stop();
	kill_child(&mut child);
	stop?;

	println!(
		"summary: {} frames ({} dmabuf, {} pixels)",
		summary.received, summary.dmabuf, summary.pixels,
	);
	Ok(())
}

/// Counts and storage kinds seen over one consumption pass.
#[derive(Default)]
struct Summary {
	received: u64,
	dmabuf: u64,
	pixels: u64,
}

/// Receive frames until `max_frames` or `deadline`, dropping each one.
fn consume(handles: &CompositorHandles, deadline: Duration, max_frames: u64) -> Summary {
	let start = Instant::now();
	let mut summary = Summary::default();

	while summary.received < max_frames && start.elapsed() < deadline {
		match handles.frames.recv_timeout(Duration::from_millis(200)) {
			Ok(frame) => {
				summary.received += 1;
				match frame.data {
					FrameData::DmaBuf(dmabuf) => {
						summary.dmabuf += 1;
						let stride = dmabuf.dmabuf.strides().next().unwrap_or(0);
						println!(
							"frame {:>4}: {}x{} dmabuf planes={} stride={} buffer_id={} acquire_fence={}",
							frame.info.sequence,
							frame.info.width,
							frame.info.height,
							dmabuf.dmabuf.num_planes(),
							stride,
							dmabuf.buffer_id,
							dmabuf.acquire_fence.is_some(),
						);
						// The pool slot is returned when the frame is dropped at the
						// end of this arm.
					},
					FrameData::Pixels(pixels) => {
						summary.pixels += 1;
						println!(
							"frame {:>4}: {}x{} pixels format={:?} stride={}",
							frame.info.sequence, frame.info.width, frame.info.height, pixels.format, pixels.stride,
						);
					},
				}
			},
			Err(RecvTimeoutError::Timeout) => {},
			Err(RecvTimeoutError::Disconnected) => {
				println!("frame channel disconnected");
				break;
			},
		}
	}
	summary
}

/// Spawn `vkcube` against the compositor's Wayland socket.
fn spawn_vkcube(wayland_display: &str) -> Result<Child> {
	let child = Command::new("vkcube")
		.args(["--wsi", "wayland"])
		.env("WAYLAND_DISPLAY", wayland_display)
		.stdout(Stdio::null())
		.stderr(Stdio::inherit())
		.spawn()
		.map_err(|e| Error::Config(format!("spawn vkcube: {e}")))?;
	println!("spawned vkcube (pid {})", child.id());
	Ok(child)
}

/// Stop the hosted child if it is still running.
fn kill_child(child: &mut Child) {
	if let Err(e) = child.kill() {
		eprintln!("kill vkcube: {e}");
	}
	let _ = child.wait();
}
