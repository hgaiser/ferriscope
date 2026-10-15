//! The frame contract: what the compositor hands to a sink.
//!
//! A frame is either an owned DMA-BUF with explicit synchronisation or
//! CPU-visible pixels (the file-recorder path). It is defined here, separately
//! from [`crate::sink`], so the storage contract can evolve without touching
//! the sink trait.
//!
//! Downstream of a DMA-BUF frame the consumer owns the borrow on the
//! compositor's pool slot, so the pool is bounded by explicit
//! [`Release`]s rather than by a blocking wait.
//!
//! # Abandonment
//!
//! A DMA-BUF frame carries a private release guard: if the consumer drops the
//! frame without sending a [`Release`], the guard returns the slot
//! automatically (with no release fence, since none is known). This is what
//! keeps a dropped frame from leaking a pool slot until the pool is exhausted.
//!
//! A fence-carrying release must be sent **before** the frame is dropped: the
//! guard fires on drop with no fence, so if it arrived first it would free the
//! slot and the later fence would be a stale duplicate.

use std::{os::fd::OwnedFd, time::Instant};

use smithay::{backend::allocator::dmabuf::Dmabuf, reexports::calloop};

use crate::{
	colour_meta::ColourMetadata,
	error::{Error, Result},
};

/// CPU-visible pixel layouts produced by readback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
	/// Bytes in memory order B, G, R, A.
	Bgra8,
	/// Bytes in memory order R, G, B, A.
	Rgba8,
}

impl PixelFormat {
	/// Bytes per pixel.
	pub const fn bytes_per_pixel(self) -> usize {
		4
	}

	/// Reorder one pixel into RGB (dropping alpha).
	#[cfg(feature = "test-support")]
	pub(crate) fn to_rgb(self, pixel: &[u8]) -> [u8; 3] {
		match self {
			PixelFormat::Bgra8 => [pixel[2], pixel[1], pixel[0]],
			PixelFormat::Rgba8 => [pixel[0], pixel[1], pixel[2]],
		}
	}
}

/// Metadata attached to every frame, independent of its storage.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameInfo {
	/// Monotonic frame sequence number.
	pub sequence: u64,
	/// When the compositor captured the frame.
	pub captured_at: Instant,
	/// Frame width in pixels.
	pub width: u32,
	/// Frame height in pixels.
	pub height: u32,
	/// Colour space and HDR metadata.
	pub colour: ColourMetadata,
}

/// CPU-readable pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PixelBuffer {
	/// Pixel layout.
	pub format: PixelFormat,
	/// Bytes per row.
	pub stride: usize,
	/// Pixel data, `stride * height` bytes.
	pub data: Vec<u8>,
}

/// A DMA-BUF frame with explicit synchronisation.
pub struct DmaBufFrame {
	/// Opaque handle identifying the pool slot and its delivery generation.
	///
	/// It encodes `(slot, generation)` so a late [`Release`] for an earlier
	/// delivery cannot free a slot that has since been reused. The consumer
	/// sends it back unchanged; it must not be interpreted or constructed.
	///
	/// The slot stays held until a [`Release`] with a current handle arrives or
	/// the frame is dropped (the private guard below returns it).
	pub buffer_id: u64,
	/// The composited buffer.
	pub dmabuf: Dmabuf,
	/// Fence the consumer waits on before reading the buffer.
	///
	/// `None` reliably means the render was already complete when the frame was
	/// handed over: if `SyncPoint::export` could not provide a native fence but
	/// the GPU work was still in flight, the compositor waits for completion
	/// before handing the frame off.
	pub acquire_fence: Option<OwnedFd>,
	/// Auto-releases the slot when the consumer drops the frame without a
	/// [`Release`]; installed by the delivering sink. Private by design.
	pub(crate) release: Option<ReleaseGuard>,
}

/// Returns buffers to the compositor.
///
/// A send wakes the compositor's event loop, so a released buffer is available
/// for the next frame at once; there is no polling interval to wait out.
#[derive(Clone)]
pub struct ReleaseSender(calloop::channel::Sender<Release>);

impl ReleaseSender {
	/// A connected sender and the compositor's end of the channel.
	pub(crate) fn channel() -> (Self, calloop::channel::Channel<Release>) {
		let (tx, rx) = calloop::channel::channel();
		(Self(tx), rx)
	}

	/// Return a buffer to the compositor.
	///
	/// Fails once the compositor has shut down and its release source is gone.
	pub fn send(&self, release: Release) -> Result<()> {
		self.0
			.send(release)
			.map_err(|_| Error::Sink("release channel closed".into()))
	}
}

/// Returns a pool slot to the compositor if the frame is abandoned.
///
/// Installed by a sink that owns the release channel (see
/// [`crate::compositor::ChannelSink`]). Sends [`Release`] with no fence on
/// drop; a stale or duplicate handle is ignored by the compositor, so an
/// explicit release followed by the guard is harmless.
pub(crate) struct ReleaseGuard {
	releases: ReleaseSender,
	buffer_id: u64,
}

impl ReleaseGuard {
	/// Create a guard that releases `buffer_id` on drop.
	pub(crate) fn new(releases: ReleaseSender, buffer_id: u64) -> Self {
		Self { releases, buffer_id }
	}
}

impl Drop for ReleaseGuard {
	fn drop(&mut self) {
		let _ = self.releases.send(Release {
			buffer_id: self.buffer_id,
			fence: None,
		});
	}
}

/// Sent by the consumer when it is done with a buffer.
///
/// This is the complement of [`DmaBufFrame::buffer_id`]: the compositor frees
/// the pool slot named by `buffer_id`, but only if the handle is still current.
/// A release for an already-reused slot is ignored.
///
/// The release fence travels here, not on the frame. The optional `fence` is a
/// GPU-completion fence guarding slot reuse: the compositor watches it from its
/// event loop without blocking and frees the slot the moment it signals, so a
/// fence that never signals leaves that one slot busy instead of wedging the
/// event loop. `None` means
/// the consumer's work is already complete. A fence must be a pollable
/// dma-fence (a `sync_file`).
pub struct Release {
	/// The buffer handle being returned; see [`DmaBufFrame::buffer_id`].
	pub buffer_id: u64,
	/// Optional GPU-completion fence guarding reuse of the slot.
	pub fence: Option<OwnedFd>,
}

/// The storage of a frame.
pub enum FrameData {
	/// CPU-visible pixels (the file-recorder path).
	Pixels(PixelBuffer),
	/// A DMA-BUF.
	DmaBuf(DmaBufFrame),
}

/// One composited frame.
pub struct Frame {
	/// Frame metadata.
	pub info: FrameInfo,
	/// Frame storage.
	pub data: FrameData,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn frames_and_releases_cross_threads() {
		fn assert_send<T: Send>() {}
		assert_send::<Frame>();
		assert_send::<FrameData>();
		assert_send::<DmaBufFrame>();
		assert_send::<Release>();
	}

	/// Everything queued on the compositor's end of a release channel.
	fn drain(channel: calloop::channel::Channel<Release>) -> Vec<Release> {
		use calloop::{EventLoop, channel::Event};
		let mut event_loop: EventLoop<Vec<Release>> = EventLoop::try_new().expect("event loop");
		event_loop
			.handle()
			.insert_source(channel, |event, _, out: &mut Vec<Release>| {
				if let Event::Msg(release) = event {
					out.push(release);
				}
			})
			.expect("insert channel");
		let mut out = Vec::new();
		event_loop
			.dispatch(Some(std::time::Duration::ZERO), &mut out)
			.expect("dispatch");
		out
	}

	#[test]
	fn release_guard_emits_exactly_one_release_on_drop() {
		let (tx, rx) = ReleaseSender::channel();
		{
			let _guard = ReleaseGuard::new(tx, 0x1_0000_0007);
		}
		let releases = drain(rx);
		// Dropping the guard once must enqueue exactly one release.
		assert_eq!(releases.len(), 1);
		assert_eq!(releases[0].buffer_id, 0x1_0000_0007);
		assert!(releases[0].fence.is_none());
	}

	#[test]
	fn release_guard_drop_is_silent_when_receiver_is_gone() {
		let (tx, rx) = ReleaseSender::channel();
		drop(rx);
		// Must not panic even though the compositor end is gone.
		drop(ReleaseGuard::new(tx, 1));
	}
}
