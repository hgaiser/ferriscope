//! The compositor's private frame sink.
//!
//! A [`Sink`] consumes composited frames. It is a trait, not a socket call
//! buried in the compositor, so the render loop is independent of where frames
//! go. The product sink is the in-process channel in [`crate::compositor`],
//! which hands DMA-BUF frames to the embedder; the `test-support` feature adds
//! the CPU recorders in [`crate::recorder`].

use crate::{error::Result, frame::Frame};

/// A consumer of composited frames.
pub(crate) trait Sink {
	/// Whether this sink consumes [`crate::frame::FrameData::DmaBuf`] rather than pixels.
	///
	/// The default is `false`, keeping the CPU readback path.
	fn wants_dmabuf(&self) -> bool {
		false
	}

	/// Deliver one frame. Ownership of buffers and fences (and, for a DMA-BUF,
	/// its pool slot) passes to the sink.
	fn submit(&mut self, frame: Frame) -> Result<()>;
}
