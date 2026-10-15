//! GPU integration test for the acquire fence.
//!
//! Every test here is `#[ignore]`d: it needs a render node and `vkcube`, and it
//! drives `Compositor::launch` with a real client. Run them one at a time:
//!
//! ```text
//! cargo test -p ferriscope-core --test release_fence -- --ignored --test-threads=1
//! ```
//!
//! The tests exercise what `examples/stream.rs` does not:
//!
//! - **acquire fence** — the fd handed out with a DMA-BUF is a real, pollable
//!   dma-fence that signals, not an opaque handle.
//!
//! A consumer returns a buffer by dropping its frame; there is no public
//! explicit release, so there is no release-fence path to test from outside.

use std::{
	os::fd::OwnedFd,
	process::{Child, Command, Stdio},
	time::{Duration, Instant},
};

use ferriscope_core::{
	Compositor, CompositorConfig, CompositorHandles, DmaBufFrame, Frame, FrameData, RecvTimeoutError, Result,
};
use smithay::reexports::rustix::{
	event::{PollFd, PollFlags, Timespec, poll},
	io::Errno,
};

/// Output width, small enough that a software-render fallback still keeps up.
const WIDTH: u32 = 320;
/// Output height.
const HEIGHT: u32 = 240;
/// Handshake budget.
const READY_TIMEOUT: Duration = Duration::from_secs(10);
/// Budget for a single DMA-BUF frame.
const FRAME_TIMEOUT: Duration = Duration::from_secs(10);
/// Budget for an acquire fence to become pollable.
const ACQUIRE_FENCE_TIMEOUT: Duration = Duration::from_secs(2);
/// Per-iteration `poll(2)` slice while waiting on a fence.
const POLL_SLICE: Duration = Duration::from_millis(20);

/// Kills the client and stops the compositor on drop.
///
/// A failed assertion must not leak a compositor thread or an `vkcube`.
struct CompositorGuard {
	compositor: Option<Compositor>,
	child: Option<Child>,
}

impl CompositorGuard {
	/// Stop the hosted client, then the compositor, and report the join result.
	fn stop(&mut self) -> Result<()> {
		self.kill_child();
		match self.compositor.take() {
			Some(compositor) => compositor.stop(),
			None => Ok(()),
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

impl Drop for CompositorGuard {
	fn drop(&mut self) {
		// The explicit `stop` is the asserted path; drop is the safety net.
		let _ = self.stop();
	}
}

/// Launch a compositor and point a `vkcube` at its Wayland socket.
fn launch_with_vkcube() -> (CompositorGuard, CompositorHandles) {
	let config = CompositorConfig {
		width: WIDTH,
		height: HEIGHT,
		xwayland: false,
		command: None,
		..CompositorConfig::default()
	};

	let (compositor, handles) = Compositor::launch(config).expect("launch compositor");
	let mut guard = CompositorGuard {
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

/// Receive the next DMA-BUF frame, skipping any CPU-pixel frames.
fn recv_dmabuf(handles: &CompositorHandles) -> DmaBufFrame {
	let deadline = Instant::now() + FRAME_TIMEOUT;
	loop {
		let remaining = deadline.saturating_duration_since(Instant::now());
		assert!(!remaining.is_zero(), "no DMA-BUF frame within {FRAME_TIMEOUT:?}");
		match handles.frames.recv_timeout(remaining) {
			Ok(Frame {
				data: FrameData::DmaBuf(dmabuf),
				..
			}) => return dmabuf,
			Ok(_) => {},
			Err(RecvTimeoutError::Timeout) => panic!("no DMA-BUF frame within {FRAME_TIMEOUT:?}"),
			Err(RecvTimeoutError::Disconnected) => panic!("frame mailbox disconnected"),
		}
	}
}

/// Poll `fd` until readable, the way a `sync_file` reports a fired fence.
fn dma_fence_signals(fd: &OwnedFd, timeout: Duration) -> bool {
	let deadline = Instant::now() + timeout;
	loop {
		let mut fds = [PollFd::new(fd, PollFlags::IN)];
		let slice = Timespec::try_from(POLL_SLICE).expect("poll slice fits a timespec");
		match poll(&mut fds, Some(&slice)) {
			Ok(_) if fds[0].revents().contains(PollFlags::IN) => return true,
			Ok(_) => {},
			Err(e) if e == Errno::INTR => {},
			Err(_) => return false,
		}
		if Instant::now() >= deadline {
			return false;
		}
	}
}

#[test]
#[ignore = "requires GPU and vkcube"]
fn acquire_fence_is_a_signalled_dma_fence() {
	let (mut guard, handles) = launch_with_vkcube();
	let dmabuf = recv_dmabuf(&handles);

	// The frame must carry a fence, and that fence must be a real dma-fence:
	// poll(2) reports it readable once the render completes.
	let fence = dmabuf
		.acquire_fence
		.as_ref()
		.expect("DMA-BUF frame carries an acquire fence");
	assert!(
		dma_fence_signals(fence, ACQUIRE_FENCE_TIMEOUT),
		"acquire fence did not become readable within {ACQUIRE_FENCE_TIMEOUT:?}"
	);

	// Dropping the frame returns its slot; the compositor must shut down cleanly.
	drop(dmabuf);
	guard.stop().expect("compositor stopped cleanly");
}
