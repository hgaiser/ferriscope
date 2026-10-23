//! GPU integration test for the slot bound.
//!
//! The gate: *the consumer never holds more than `pool_size` buffers; the app
//! is paced at the target rate with no buffer accumulation and no starvation.*
//! A slot is taken at delivery and returned when the consumer drops the frame,
//! so the bound holds across ticks, and the drop itself wakes the compositor to
//! render the next frame.
//!
//! These tests are `#[ignore]`d: they need a render node and `vkcube`, and drive
//! `Compositor::launch` with a real client. Run them one at a time:
//!
//! ```text
//! cargo test -p ferriscope --test slot_bound -- --ignored --test-threads=1
//! ```
//!
//! `vkcube` is a deliberately bursty producer: left unbounded it renders as fast
//! as it can. If the bound were not enforced, a second frame would arrive while
//! the first is unreleased.

use std::{
	process::{Child, Command, Stdio},
	sync::mpsc,
	time::{Duration, Instant},
};

use ferriscope::{Compositor, CompositorConfig, CompositorHandles, DmaBufFrame, Frame, FrameData, RecvTimeoutError};

/// Output width, small enough that a software-render fallback still keeps up.
const WIDTH: u32 = 320;
/// Output height.
const HEIGHT: u32 = 240;
/// Handshake budget.
const READY_TIMEOUT: Duration = Duration::from_secs(10);
/// Budget for a single DMA-BUF frame.
const FRAME_TIMEOUT: Duration = Duration::from_secs(10);
/// Window in which no further frame may arrive while one is unreleased.
const NO_FRAME_WINDOW: Duration = Duration::from_millis(1500);
/// Budget for the next frame after a release (the bound must not starve).
const NEXT_FRAME_TIMEOUT: Duration = Duration::from_secs(2);
/// Number of hold/release cycles in the `pool_size == 1` test.
const CYCLES: usize = 4;
/// Watchdog budget for `Compositor::stop` not to hang.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Kills the client and stops the compositor on drop.
///
/// A failed assertion must not leak a compositor thread or a `vkcube`.
struct SlotBoundGuard {
	compositor: Option<Compositor>,
	child: Option<Child>,
}

impl SlotBoundGuard {
	/// Kill the hosted client, then stop the compositor under a watchdog.
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

		match status_rx.recv_timeout(timeout) {
			Ok(result) => {
				let _ = stopper.join();
				result.is_ok()
			},
			Err(_) => {
				// A wedged event loop would otherwise hang the harness. The
				// stuck thread is intentionally left unjoined; the test has
				// already failed, and leaking one thread beats hanging.
				self.kill_child();
				false
			},
		}
	}

	/// Kill and reap the client if it is still running.
	fn kill_child(&mut self) {
		if let Some(mut child) = self.child.take() {
			let _ = child.kill();
			let _ = child.wait();
		}
	}
}

impl Drop for SlotBoundGuard {
	fn drop(&mut self) {
		// The explicit `stop` is the asserted path; drop is the safety net.
		let _ = self.stop(STOP_TIMEOUT);
	}
}

/// Launch a compositor with `pool_size` buffers and point `vkcube` at it.
fn launch_with_vkcube(pool_size: usize) -> (SlotBoundGuard, CompositorHandles) {
	let config = CompositorConfig {
		width: WIDTH,
		height: HEIGHT,
		xwayland: false,
		command: None,
		output: ferriscope::FrameOutput::Channel(ferriscope::ChannelOutput { pool_size }),
		..CompositorConfig::default()
	};

	let (compositor, handles) = Compositor::launch(config).expect("launch compositor");
	let mut guard = SlotBoundGuard {
		compositor: Some(compositor),
		child: None,
	};

	let ready = handles
		.ready
		.recv_timeout(READY_TIMEOUT)
		.expect("ready handshake within timeout");

	let mut command = Command::new("vkcube");
	command
		.args(["--wsi", "wayland"])
		.env("WAYLAND_DISPLAY", &ready.wayland_display)
		.stdout(Stdio::null())
		.stderr(Stdio::null());
	if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") {
		command.env("XDG_RUNTIME_DIR", runtime_dir);
	}
	guard.child = Some(command.spawn().expect("spawn vkcube"));
	(guard, handles)
}

/// Receive the next DMA-BUF frame, or `None` if none arrives within `timeout`.
///
/// A CPU-pixel frame is skipped: the in-process sink reports `wants_dmabuf`, so
/// a correct run only ever hands over DMA-BUFs.
fn recv_dmabuf(handles: &CompositorHandles, timeout: Duration) -> Option<DmaBufFrame> {
	let deadline = Instant::now() + timeout;
	loop {
		let remaining = deadline.saturating_duration_since(Instant::now());
		if remaining.is_zero() {
			return None;
		}
		match handles.frames.recv_timeout(remaining) {
			Ok(Frame {
				data: FrameData::DmaBuf(dmabuf),
				..
			}) => return Some(dmabuf),
			Ok(_) => {},
			Err(RecvTimeoutError::Timeout) => return None,
			Err(RecvTimeoutError::Disconnected) => panic!("frame mailbox disconnected"),
		}
	}
}

#[test]
#[ignore = "requires GPU and vkcube"]
fn one_slot_bounds_delivery_and_release_rearms() {
	let (mut guard, handles) = launch_with_vkcube(1);
	let mut dmabuf = recv_dmabuf(&handles, FRAME_TIMEOUT).expect("first DMA-BUF frame within timeout");

	for cycle in 0..CYCLES {
		// The frame is delivered but unreleased: the bound is saturated, so no
		// further frame may arrive. Without the coupling the next pool slot
		// would have been handed over by now.
		let extra = recv_dmabuf(&handles, NO_FRAME_WINDOW);
		assert!(
			extra.is_none(),
			"cycle {cycle}: a frame arrived while one was unreleased; pool_size <= 1 is not enforced"
		);

		// Dropping the frame returns the permit, so the next frame must follow.
		drop(dmabuf);
		dmabuf = recv_dmabuf(&handles, NEXT_FRAME_TIMEOUT).unwrap_or_else(|| {
			panic!("cycle {cycle}: no frame within {NEXT_FRAME_TIMEOUT:?} after release; the bound starves delivery")
		});
	}

	// Return the last frame.
	drop(dmabuf);
	assert!(guard.stop(STOP_TIMEOUT), "compositor stopped cleanly");
}

#[test]
#[ignore = "requires GPU and vkcube"]
fn two_slots_allow_two_unreleased_frames() {
	let (mut guard, handles) = launch_with_vkcube(2);
	let first = recv_dmabuf(&handles, FRAME_TIMEOUT).expect("first DMA-BUF frame within timeout");
	let second = recv_dmabuf(&handles, FRAME_TIMEOUT).expect("second DMA-BUF frame within timeout");
	assert_ne!(
		first.buffer_id, second.buffer_id,
		"two in-flight frames use distinct deliveries"
	);

	// Two permits are held: a third frame may not arrive until one returns.
	let extra = recv_dmabuf(&handles, NO_FRAME_WINDOW);
	assert!(
		extra.is_none(),
		"a third frame arrived while two were unreleased; pool_size <= 2 is not enforced"
	);

	// One release frees one permit, so a third frame must follow.
	drop(first);
	let third = recv_dmabuf(&handles, NEXT_FRAME_TIMEOUT)
		.expect("no frame within the window after releasing one of two; the deeper bound starves delivery");

	// Return both remaining frames.
	drop(second);
	drop(third);
	assert!(guard.stop(STOP_TIMEOUT), "compositor stopped cleanly");
}
