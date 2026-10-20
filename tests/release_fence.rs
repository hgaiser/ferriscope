//! GPU integration test for the acquire fence.
//!
//! Every test here is `#[ignore]`d: it needs a render node, and it drives
//! `Compositor::launch` with a real client. Run them one at a time:
//!
//! ```text
//! cargo test -p ferriscope --test release_fence -- --ignored --test-threads=1
//! ```
//!
//! The tests exercise what `examples/stream.rs` does not:
//!
//! - **acquire fence** — the fd handed out with a composited DMA-BUF is a
//!   real, pollable dma-fence that signals, not an opaque handle. The client
//!   draws into `wl_shm`, so its frames are always composited; a client's own
//!   DMA-BUF could be passed through, and that frame carries no fence.
//!
//! A consumer returns a buffer by dropping its frame; there is no public
//! explicit release, so there is no release-fence path to test from outside.

use std::{
	os::fd::OwnedFd,
	time::{Duration, Instant},
};

use ferriscope::{CompositorHandles, DmaBufFrame, Frame, FrameData, RecvTimeoutError};
use smithay::reexports::rustix::{
	event::{PollFd, PollFlags, Timespec, poll},
	io::Errno,
};
use support::ClientOptions;

mod support;

/// Budget for a single DMA-BUF frame.
const FRAME_TIMEOUT: Duration = Duration::from_secs(10);
/// Budget for an acquire fence to become pollable.
const ACQUIRE_FENCE_TIMEOUT: Duration = Duration::from_secs(2);
/// Per-iteration `poll(2)` slice while waiting on a fence.
const POLL_SLICE: Duration = Duration::from_millis(20);

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
#[ignore = "requires GPU"]
fn acquire_fence_is_a_signalled_dma_fence() {
	let mut started = support::start_configured(ClientOptions::default(), |_| {});
	let dmabuf = recv_dmabuf(&started._handles);

	// The frame must carry a fence, and that fence must be a real dma-fence:
	// poll(2) reports it readable once the render completes.
	let fence = dmabuf
		.acquire_fence
		.as_ref()
		.expect("composited frame carries an acquire fence");
	assert!(
		dma_fence_signals(fence, ACQUIRE_FENCE_TIMEOUT),
		"acquire fence did not become readable within {ACQUIRE_FENCE_TIMEOUT:?}"
	);

	// Dropping the frame returns its slot; the compositor must shut down cleanly.
	drop(dmabuf);
	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}
