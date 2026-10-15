//! Explicit sync and buffer-readiness gating.
//!
//! Three cooperating pieces keep the compositor from sampling a buffer the
//! client is still writing, and keep the client from overwriting one ferriscope
//! is still reading, without ever blocking the event loop:
//!
//! - **Acquire** (`linux-drm-syncobj-v1`): a commit carrying an acquire point
//!   gets a `DrmSyncPointBlocker` so it does not become current until the
//!   client's GPU work has signalled the point. The point's eventfd is an
//!   event-loop source; when it fires it clears the blocker and the
//!   transaction is applied in the same dispatch.
//! - **Readiness** (implicit sync): a dma-buf commit without a syncobj waits
//!   for the buffer's implicit write fences, so the stream shows the newest
//!   *finished* frame instead of queueing behind the client's unfinished GPU
//!   work. The fences are snapshotted at commit time as sync files
//!   (`DMA_BUF_IOCTL_EXPORT_SYNC_FILE`), as Mutter and KWin do: polling the
//!   dma-buf itself also waits for fences attached after the commit, which
//!   amdgpu does for later work on the same buffer. Kernels without the ioctl
//!   fall back to smithay's `DmabufBlocker`, which polls the dma-buf.
//! - **Release**: smithay signals a buffer's release point (and sends
//!   `wl_buffer.release`) when the last clone of its `Buffer` drops. After each
//!   composite [`FerriscopeState::hold_frame_buffers`] keeps the buffers the
//!   frame read alive until the composite's GPU work has finished, so the
//!   release is not signalled while the GPU may still be reading.
//!
//! Every wake-up is an event-loop source, and a cleared blocker applies the
//! commit inside the dispatch, so [`FerriscopeState::service`] runs right after
//! and the scheduler delivers promptly.

use std::{
	os::fd::OwnedFd,
	sync::{
		Arc,
		atomic::{AtomicUsize, Ordering},
	},
};

use smithay::{
	backend::{
		allocator::dmabuf::Dmabuf,
		renderer::{
			sync::SyncPoint,
			utils::{Buffer, RendererSurfaceStateUserData},
		},
	},
	desktop::PopupManager,
	reexports::{
		calloop::{Interest, Mode, PostAction, generic::Generic},
		rustix::{
			event::{PollFd, PollFlags, Timespec, poll},
			ioctl::{Opcode, Updater, ioctl, opcode::read_write},
		},
		wayland_server::{Resource, protocol::wl_surface::WlSurface},
	},
	wayland::{
		compositor::{
			Blocker, BlockerState, BufferAssignment, CompositorHandler, SurfaceAttributes, TraversalAction,
			add_blocker, add_pre_commit_hook, with_states, with_surface_tree_downward,
		},
		dmabuf::get_dmabuf,
		drm_syncobj::{DrmSyncobjCachedState, DrmSyncobjHandler, DrmSyncobjState},
	},
};

use crate::shell::FerriscopeState;

impl DrmSyncobjHandler for FerriscopeState {
	fn drm_syncobj_state(&mut self) -> Option<&mut DrmSyncobjState> {
		self.syncobj_state.as_mut()
	}
}

/// Install the readiness gate on a new surface.
///
/// Runs as a pre-commit hook, so the blocker is attached to the transaction
/// that carries the buffer.
pub(in crate::shell) fn install_commit_gate(surface: &WlSurface) {
	add_pre_commit_hook::<FerriscopeState, _>(surface, |state, _dh, surface| {
		let mut acquire_point = None;
		let dmabuf = with_states(surface, |states| {
			acquire_point.clone_from(
				&states
					.cached_state
					.get::<DrmSyncobjCachedState>()
					.pending()
					.acquire_point,
			);
			match &states.cached_state.get::<SurfaceAttributes>().pending().buffer {
				Some(BufferAssignment::NewBuffer(buffer)) => get_dmabuf(buffer).ok().cloned(),
				_ => None,
			}
		});
		let Some(client) = surface.client() else {
			return;
		};

		// Explicit sync: the acquire point is the only readiness signal.
		if let Some(acquire_point) = acquire_point {
			tracing::trace!(
				target: "ferriscope::sync",
				surface = surface.id().protocol_id(),
				point = acquire_point.point(),
				"commit gated on acquire point"
			);
			match acquire_point.generate_blocker() {
				Ok((blocker, source)) => {
					let inserted = state.loop_handle.insert_source(source, move |_, _, state| {
						tracing::trace!(target: "ferriscope::sync", "acquire point signalled");
						let dh = state.display_handle.clone();
						state.client_compositor_state(&client).blocker_cleared(state, &dh);
						Ok(())
					});
					match inserted {
						Ok(_) => add_blocker(surface, blocker),
						Err(e) => tracing::warn!(error = ?e, "cannot watch acquire point; not gating commit"),
					}
				},
				Err(e) => tracing::warn!(error = %e, "cannot create acquire-point blocker; not gating commit"),
			}
			return;
		}

		// Implicit sync: wait for the dma-buf's fences.
		if let Some(dmabuf) = dmabuf {
			gate_on_dmabuf(state, surface, dmabuf);
		}
	});
}

/// Block `surface`'s pending commit on the readiness of `dmabuf`.
fn gate_on_dmabuf(state: &mut FerriscopeState, surface: &WlSurface, dmabuf: Dmabuf) {
	let Some(client) = surface.client() else {
		return;
	};
	let Some(fences) = snapshot_write_fences(&dmabuf) else {
		tracing::trace!(target: "ferriscope::sync", "commit gated by polling the dma-buf");
		gate_on_dmabuf_poll(state, surface, dmabuf, client);
		return;
	};
	if fences.is_empty() {
		return;
	}
	let pending = Arc::new(AtomicUsize::new(fences.len()));
	let mut watched = 0;
	for fence in fences {
		let pending_fence = pending.clone();
		let client = client.clone();
		let source = Generic::new(fence, Interest::READ, Mode::Level);
		let inserted = state.loop_handle.insert_source(source, move |_, _, state| {
			// A sync file becomes readable when its fences signal.
			if pending_fence.fetch_sub(1, Ordering::SeqCst) == 1 {
				let dh = state.display_handle.clone();
				state.client_compositor_state(&client).blocker_cleared(state, &dh);
			}
			Ok(PostAction::Remove)
		});
		match inserted {
			Ok(_) => watched += 1,
			Err(e) => {
				tracing::warn!(error = ?e, "cannot watch dma-buf fence; not waiting for it");
				pending.fetch_sub(1, Ordering::SeqCst);
			},
		}
	}
	if watched > 0 {
		tracing::trace!(target: "ferriscope::sync", fences = watched, "commit gated on dma-buf fences");
		add_blocker(surface, FenceBlocker(pending));
	}
}

/// The pre-sync-file fallback: smithay polls the dma-buf itself.
fn gate_on_dmabuf_poll(
	state: &mut FerriscopeState,
	surface: &WlSurface,
	dmabuf: Dmabuf,
	client: smithay::reexports::wayland_server::Client,
) {
	// `Err` means the buffer is already ready: nothing to wait for.
	let Ok((blocker, source)) = dmabuf.generate_blocker(Interest::READ) else {
		return;
	};
	let inserted = state.loop_handle.insert_source(source, move |_, _, state| {
		let dh = state.display_handle.clone();
		state.client_compositor_state(&client).blocker_cleared(state, &dh);
		Ok(())
	});
	match inserted {
		Ok(_) => add_blocker(surface, blocker),
		Err(e) => tracing::warn!(error = ?e, "cannot watch dma-buf readiness; not gating commit"),
	}
}

/// Holds a commit until every snapshotted fence has signalled.
struct FenceBlocker(Arc<AtomicUsize>);

impl Blocker for FenceBlocker {
	fn state(&self) -> BlockerState {
		if self.0.load(Ordering::SeqCst) == 0 {
			BlockerState::Released
		} else {
			BlockerState::Pending
		}
	}
}

/// `struct dma_buf_export_sync_file` (`linux/dma-buf.h`).
#[repr(C)]
struct DmaBufExportSyncFile {
	flags: u32,
	fd: i32,
}

/// `DMA_BUF_SYNC_READ`: the fences a reader must wait for, i.e. the writers.
const DMA_BUF_SYNC_READ: u32 = 1 << 0;
const DMA_BUF_IOCTL_EXPORT_SYNC_FILE: Opcode = read_write::<DmaBufExportSyncFile>(b'b', 2);

/// The unsignalled write fences of every plane of `dmabuf`, as sync files, or
/// `None` when the kernel cannot export them (before Linux 6.0).
fn snapshot_write_fences(dmabuf: &Dmabuf) -> Option<Vec<OwnedFd>> {
	let mut fences = Vec::new();
	for handle in dmabuf.handles() {
		let mut request = DmaBufExportSyncFile {
			flags: DMA_BUF_SYNC_READ,
			fd: -1,
		};
		// SAFETY: the opcode and the struct match `linux/dma-buf.h`, and the
		// kernel only writes `fd`.
		let exported = unsafe { ioctl(handle, Updater::<DMA_BUF_IOCTL_EXPORT_SYNC_FILE, _>::new(&mut request)) };
		if let Err(e) = exported {
			tracing::debug!(target: "ferriscope::sync", error = %e, "cannot export dma-buf fences; polling the dma-buf");
			return None;
		}
		// SAFETY: on success the kernel returned a new sync file we now own.
		let fence = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(request.fd) };
		if !is_signalled(&fence) {
			fences.push(fence);
		}
	}
	Some(fences)
}

/// Whether a sync file has already signalled (it polls readable).
fn is_signalled(fence: &OwnedFd) -> bool {
	let mut fds = [PollFd::new(fence, PollFlags::IN)];
	// A zero timeout: `None` would block until the fence signals.
	let now = Timespec { tv_sec: 0, tv_nsec: 0 };
	matches!(poll(&mut fds, Some(&now)), Ok(n) if n > 0) && fds[0].revents().contains(PollFlags::IN)
}

impl FerriscopeState {
	/// Keep the buffers the frame just composited alive until its GPU work
	/// has finished.
	///
	/// A client's release point is signalled when smithay drops the last clone
	/// of the buffer, which happens as soon as the client commits a
	/// replacement, possibly while the composite is still reading it. Holding a
	/// clone until `sync` signals defers the release to the right moment. The
	/// fence is watched from the event loop; nothing waits.
	pub(in crate::shell) fn hold_frame_buffers(&mut self, sync: &SyncPoint) {
		if sync.is_reached() {
			return;
		}
		let buffers = self.frame_buffers();
		if buffers.is_empty() {
			return;
		}
		let Some(fence) = sync.export() else {
			// No exportable fence: the only safe option is a (rare) blocking wait.
			if let Err(e) = sync.wait() {
				tracing::warn!("composite fence wait: {e:?}");
			}
			return;
		};
		let mut held = Some(buffers);
		let source = Generic::new(fence, Interest::READ, Mode::Level);
		let inserted = self.loop_handle.insert_source(source, move |_, _, _| {
			// A sync file becomes readable when it signals.
			held.take();
			Ok(PostAction::Remove)
		});
		if let Err(e) = inserted {
			tracing::warn!(error = ?e, "cannot watch composite fence; buffers released early");
		}
	}

	/// The client buffers currently attached to every surface the frame draws.
	fn frame_buffers(&self) -> Vec<Buffer> {
		let mut roots: Vec<WlSurface> = self
			.space
			.elements()
			.filter_map(|window| self.window_content_surface(window))
			.collect();
		roots.extend(
			self.overlays
				.iter()
				.filter_map(|overlay| self.window_content_surface(&overlay.window)),
		);
		if let Some((cursor, _)) = self.cursor_render_target() {
			roots.push(cursor);
		}
		let popups: Vec<WlSurface> = roots
			.iter()
			.flat_map(PopupManager::popups_for_surface)
			.map(|(popup, _)| popup.wl_surface().clone())
			.collect();
		roots.extend(popups);

		let mut buffers = Vec::new();
		for root in &roots {
			with_surface_tree_downward(
				root,
				(),
				|_, _, _| TraversalAction::DoChildren(()),
				|_, states, _| {
					// `states` is already locked by the traversal; re-entering
					// `with_states` on the same surface would deadlock.
					if let Some(data) = states.data_map.get::<RendererSurfaceStateUserData>() {
						if let Some(buffer) = data.lock().unwrap().buffer() {
							buffers.push(buffer.clone());
						}
					}
				},
				|_, _, _| true,
			);
		}
		buffers
	}
}
