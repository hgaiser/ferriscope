//! GPU integration test for the explicit fence path.
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
//! - **release fence** — a real DRM syncobj exported as a `sync_file` is
//!   imported and waited on, and the compositor keeps delivering afterwards.
//! - **invalid release fence** — a valid-but-not-a-fence fd must not wedge the
//!   compositor: the event loop keeps delivering and `stop` still returns.

use std::{
	fs::File,
	os::fd::{AsFd, BorrowedFd, OwnedFd},
	process::{Child, Command, Stdio},
	sync::mpsc,
	time::{Duration, Instant},
};

use ferriscope_core::{
	Compositor, CompositorConfig, CompositorHandles, DmaBufFrame, Frame, FrameData, RecvTimeoutError, Release, Result,
};
use smithay::reexports::{
	drm::{self, control::Device as _},
	rustix::{
		event::{PollFd, PollFlags, Timespec, poll},
		io::Errno,
		pipe::pipe,
	},
};

/// Output width, small enough that a software-render fallback still keeps up.
const WIDTH: u32 = 320;
/// Output height.
const HEIGHT: u32 = 240;
/// Render node used both by the compositor and by the syncobj tests.
const RENDER_NODE: &str = "/dev/dri/renderD128";
/// Handshake budget.
const READY_TIMEOUT: Duration = Duration::from_secs(10);
/// Budget for a single DMA-BUF frame.
const FRAME_TIMEOUT: Duration = Duration::from_secs(10);
/// Budget for an acquire fence to become pollable.
const ACQUIRE_FENCE_TIMEOUT: Duration = Duration::from_secs(2);
/// Per-iteration `poll(2)` slice while waiting on a fence.
const POLL_SLICE: Duration = Duration::from_millis(20);
/// Budget for continued delivery after a valid release fence.
const CONTINUE_TIMEOUT: Duration = Duration::from_secs(5);
/// Frames expected after a valid release fence.
const CONTINUE_FRAMES: usize = 5;
/// Watchdog budget for `Compositor::stop` not to hang.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

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

/// A DRM render node opened without taking DRM master.
///
/// The `drm` crate's `DrmDeviceFd` warns when opened on a render node because
/// it assumes a primary node; the traits have no required methods, so a thin
/// wrapper avoids that.
struct RenderNode(File);

impl AsFd for RenderNode {
	fn as_fd(&self) -> BorrowedFd<'_> {
		self.0.as_fd()
	}
}

impl drm::Device for RenderNode {}

impl drm::control::Device for RenderNode {}

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

/// Stop the compositor under a watchdog, so a wedged event loop cannot hang the
/// test harness.
///
/// `Compositor::stop` joins the compositor thread; if that thread is blocked in
/// a sub-`tick` wait, the join never returns. Run `stop` on a helper thread and
/// bound the wait. On timeout the child is killed (which a healthy event loop
/// would notice) and the stuck thread is intentionally left unjoined — the test
/// has already failed, and leaking one thread is preferable to hanging.
fn stop_with_watchdog(guard: &mut CompositorGuard, timeout: Duration) -> bool {
	let (status_tx, status_rx) = mpsc::channel();
	let compositor = guard.compositor.take();
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
			guard.kill_child();
			false
		},
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

/// Open the render node for the syncobj tests.
fn open_render_node() -> RenderNode {
	RenderNode(
		File::options()
			.read(true)
			.write(true)
			.open(RENDER_NODE)
			.expect("open render node"),
	)
}

#[test]
#[ignore = "requires GPU and vkcube"]
fn acquire_fence_is_a_signalled_dma_fence() {
	let (_guard, handles) = launch_with_vkcube();
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

	handles
		.releases
		.send(Release {
			buffer_id: dmabuf.buffer_id,
			fence: None,
		})
		.expect("send release");
}

#[test]
#[ignore = "requires GPU and vkcube"]
fn syncobj_release_fence_is_accepted() {
	let (mut guard, handles) = launch_with_vkcube();
	let dmabuf = recv_dmabuf(&handles);

	// Hand back a genuine DRM dma-fence: a binary syncobj exported as a
	// pollable sync_file. A *pending* binary syncobj cannot be exported -- the
	// kernel has no fence to attach yet and `syncobj_to_fd` returns EINVAL --
	// and a pending timeline point is not reachable through the `drm` crate's
	// API, so the fence is created already signalled. This still drives the
	// compositor's real release-fence path: the fd is polled each tick and the
	// slot is freed once it reads as signalled, which must not error or wedge.
	let node = open_render_node();
	let syncobj = node.create_syncobj(true).expect("create signalled syncobj");
	let fence = node.syncobj_to_fd(syncobj, true).expect("export syncobj as sync_file");
	assert!(
		dma_fence_signals(&fence, ACQUIRE_FENCE_TIMEOUT),
		"exported sync_file does not poll as a signalled dma-fence"
	);

	handles
		.releases
		.send(Release {
			buffer_id: dmabuf.buffer_id,
			fence: Some(fence),
		})
		.expect("send syncobj release");
	drop(dmabuf);

	let mut received = 0;
	let deadline = Instant::now() + CONTINUE_TIMEOUT;
	while received < CONTINUE_FRAMES && Instant::now() < deadline {
		let timeout = deadline.saturating_duration_since(Instant::now());
		match handles.frames.recv_timeout(timeout) {
			Ok(Frame {
				data: FrameData::DmaBuf(dmabuf),
				..
			}) => {
				received += 1;
				handles
					.releases
					.send(Release {
						buffer_id: dmabuf.buffer_id,
						fence: None,
					})
					.expect("release follow-up frame");
			},
			Ok(_) => {},
			Err(e) => panic!("compositor stopped after a valid release fence: {e:?}"),
		}
	}
	assert!(
		received >= CONTINUE_FRAMES,
		"compositor delivered {received} of {CONTINUE_FRAMES} frames after a valid release fence"
	);

	guard.stop().expect("compositor stopped cleanly");
}

#[test]
#[ignore = "requires GPU and vkcube"]
fn invalid_release_fence_does_not_hang() {
	let (mut guard, handles) = launch_with_vkcube();
	let dmabuf = recv_dmabuf(&handles);

	// A pipe read end is a valid fd but not a dma-fence: it never becomes
	// readable while the write end is open. The compositor must stay responsive
	// and keep delivering from the remaining pool slots rather than block on it.
	let (read_end, _write_end) = pipe().expect("create pipe");

	handles
		.releases
		.send(Release {
			buffer_id: dmabuf.buffer_id,
			fence: Some(read_end),
		})
		.expect("send invalid release fence");
	drop(dmabuf);

	// The fenced slot is one of `pool_size`; the rest must keep flowing.
	let mut received = 0;
	let deadline = Instant::now() + CONTINUE_TIMEOUT;
	while received < CONTINUE_FRAMES && Instant::now() < deadline {
		let timeout = deadline.saturating_duration_since(Instant::now());
		match handles.frames.recv_timeout(timeout) {
			Ok(Frame {
				data: FrameData::DmaBuf(frame),
				..
			}) => {
				received += 1;
				handles
					.releases
					.send(Release {
						buffer_id: frame.buffer_id,
						fence: None,
					})
					.expect("release follow-up frame");
			},
			Ok(_) => {},
			Err(e) => panic!("compositor stopped after an invalid release fence: {e:?}"),
		}
	}
	assert!(
		received >= CONTINUE_FRAMES,
		"compositor delivered {received} of {CONTINUE_FRAMES} frames while a slot was fenced by a non-dma-fence fd"
	);

	// `stop` must return within the watchdog; a blocking wait on the pipe would
	// leave the event loop parked and join forever.
	assert!(
		stop_with_watchdog(&mut guard, STOP_TIMEOUT),
		"Compositor::stop did not return within {STOP_TIMEOUT:?} after an invalid release fence"
	);
}
