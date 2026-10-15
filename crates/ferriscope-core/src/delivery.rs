//! Frame delivery: a depth-1 mailbox between the compositor and the consumer.
//!
//! The compositor must never block on the consumer, so offering a frame never
//! blocks: it replaces any undelivered frame, which is dropped. The consumer
//! only ever sees the newest frame, which is what streaming wants.
//!
//! The mailbox sits behind [`crate::sink::Sink`]: the compositor's private
//! `ChannelSink` offers frames into a [`FrameSender`] and exposes the matching
//! [`FrameReceiver`] on the public handles.
//!
//! # Pool interaction
//!
//! An evicted frame is just a [`Frame`]; dropping it frees its DMA-BUF pool
//! slot through the frame's private release guard, so the pool needs no new
//! machinery.
//!
//! # Disconnection
//!
//! When the consumer drops its [`FrameReceiver`], `offer` reports
//! [`Disconnected`], matching a `sync_channel`'s disconnected send. When the
//! producer drops its [`FrameSender`], the receiver reports the `Disconnected`
//! variant of each receive method after any already-queued frame is drained.
//!
//! # Poisoning
//!
//! The mailbox is a mutex shared across threads. A panic while holding it
//! poisons the lock, but delivery is not worth a cascading panic: the inner
//! state is recovered rather than unwrapped on every lock, and a poisoned
//! mailbox keeps working.

use std::{
	sync::{Arc, Condvar, Mutex, MutexGuard},
	time::{Duration, Instant},
};

use crate::frame::Frame;

/// What an [`FrameSender::offer`] did with the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OfferOutcome {
	/// The frame was accepted; no earlier frame was displaced.
	Enqueued,
	/// The frame was accepted and an older undelivered frame was evicted.
	Replaced,
}

/// The peer is gone: the receiver was dropped before an [`FrameSender::offer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Disconnected;

/// Error from [`FrameReceiver::recv`]: the sender was dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvError {
	/// The producer is gone and no frame remains queued.
	Disconnected,
}

/// Error from [`FrameReceiver::recv_timeout`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvTimeoutError {
	/// The deadline elapsed with no frame available.
	Timeout,
	/// The producer is gone and no frame remains queued.
	Disconnected,
}

/// Error from [`FrameReceiver::try_recv`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TryRecvError {
	/// The mailbox is open but empty right now.
	Empty,
	/// The producer is gone and no frame remains queued.
	Disconnected,
}

/// The shared depth-1 mailbox.
struct Shared {
	frame: Mutex<Mailbox>,
	ready: Condvar,
}

/// Mutable state of a [`Shared`], guarded by its mutex.
struct Mailbox {
	/// The newest undelivered frame, if any.
	pending: Option<Frame>,
	/// Whether the producer half still exists.
	sender_alive: bool,
	/// Whether the consumer half still exists.
	receiver_alive: bool,
}

/// The producer half of a frame mailbox.
///
/// Held by the compositor's private sink. `offer` is non-blocking.
pub(crate) struct FrameSender {
	state: Arc<Shared>,
}

/// The consumer half of a frame mailbox.
///
/// Exposed as `CompositorHandles::frames`.
pub struct FrameReceiver {
	state: Arc<Shared>,
}

/// Create a mailbox.
///
/// The two halves may live on different threads; both are `Send`.
pub(crate) fn channel() -> (FrameSender, FrameReceiver) {
	let state = Arc::new(Shared {
		frame: Mutex::new(Mailbox {
			pending: None,
			sender_alive: true,
			receiver_alive: true,
		}),
		ready: Condvar::new(),
	});
	(
		FrameSender {
			state: Arc::clone(&state),
		},
		FrameReceiver { state },
	)
}

impl FrameSender {
	/// Offer one frame, without blocking.
	///
	/// This replaces any undelivered frame (returning
	/// [`OfferOutcome::Replaced`]) or stores the frame
	/// ([`OfferOutcome::Enqueued`]).
	///
	/// Returns [`Disconnected`] once the receiver has been dropped. An evicted
	/// frame is released, so its pool slot returns automatically.
	pub fn offer(&mut self, frame: Frame) -> Result<OfferOutcome, Disconnected> {
		let state = &self.state;
		let (replaced, evicted) = {
			let mut mailbox = lock(state);
			if !mailbox.receiver_alive {
				return Err(Disconnected);
			}
			let evicted = mailbox.pending.replace(frame);
			(evicted.is_some(), evicted)
		};
		// Wake the consumer and release the evicted frame outside the lock; the
		// lock only ever guards the mailbox fields.
		state.ready.notify_one();
		drop(evicted);
		if replaced {
			Ok(OfferOutcome::Replaced)
		} else {
			Ok(OfferOutcome::Enqueued)
		}
	}
}

impl FrameReceiver {
	/// Receive the next frame, blocking until one is available or the producer
	/// is gone.
	pub fn recv(&self) -> Result<Frame, RecvError> {
		let state = &self.state;
		let mut mailbox = lock(state);
		loop {
			if let Some(frame) = mailbox.pending.take() {
				return Ok(frame);
			}
			if !mailbox.sender_alive {
				return Err(RecvError::Disconnected);
			}
			mailbox = state
				.ready
				.wait(mailbox)
				.unwrap_or_else(|poisoned| poisoned.into_inner());
		}
	}

	/// Receive the next frame, waiting at most `timeout`.
	///
	/// Returns [`RecvTimeoutError::Timeout`] when the deadline passes with the
	/// mailbox open and empty, or [`RecvTimeoutError::Disconnected`] when the
	/// producer is gone and no frame remains queued. Spurious wakeups are
	/// handled by re-checking in a loop.
	pub fn recv_timeout(&self, timeout: Duration) -> Result<Frame, RecvTimeoutError> {
		let state = &self.state;
		let mut mailbox = lock(state);
		let deadline = Instant::now().checked_add(timeout);
		loop {
			if let Some(frame) = mailbox.pending.take() {
				return Ok(frame);
			}
			if !mailbox.sender_alive {
				return Err(RecvTimeoutError::Disconnected);
			}
			match deadline {
				Some(deadline) if Instant::now() >= deadline => {
					return Err(RecvTimeoutError::Timeout);
				},
				Some(deadline) => {
					let remaining = deadline.saturating_duration_since(Instant::now());
					let (guard, _) = state
						.ready
						.wait_timeout(mailbox, remaining)
						.unwrap_or_else(|poisoned| poisoned.into_inner());
					mailbox = guard;
				},
				// A timeout too large to represent on the monotonic clock: wait
				// without a deadline rather than panicking.
				None => {
					mailbox = state
						.ready
						.wait(mailbox)
						.unwrap_or_else(|poisoned| poisoned.into_inner());
				},
			}
		}
	}

	/// Receive a frame only if one is already available.
	pub fn try_recv(&self) -> Result<Frame, TryRecvError> {
		let mut mailbox = lock(&self.state);
		if let Some(frame) = mailbox.pending.take() {
			Ok(frame)
		} else if mailbox.sender_alive {
			Err(TryRecvError::Empty)
		} else {
			Err(TryRecvError::Disconnected)
		}
	}
}

impl Drop for FrameSender {
	fn drop(&mut self) {
		lock(&self.state).sender_alive = false;
		self.state.ready.notify_all();
	}
}

impl Drop for FrameReceiver {
	fn drop(&mut self) {
		lock(&self.state).receiver_alive = false;
	}
}

/// Lock the mailbox, recovering rather than panicking on a poisoned mutex.
///
/// Delivery is best-effort: a thread that panicked while offering or receiving
/// must not turn every later call into a panic. The inner state is always
/// consistent (the mutex only ever guards the `pending`/alive flags), so the
/// recovered guard is used directly.
fn lock(state: &Shared) -> MutexGuard<'_, Mailbox> {
	state.frame.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A cheap CPU frame whose only meaningful field is its sequence. Pixel
	/// frames exist only with `test-support`, so the tests that use one do too.
	#[cfg(feature = "test-support")]
	fn pixel_frame(sequence: u64) -> Frame {
		use std::time::Instant;

		use crate::{
			colour_meta::ColourMetadata,
			frame::{FrameData, FrameInfo, PixelBuffer, PixelFormat},
		};

		Frame {
			info: FrameInfo {
				sequence,
				captured_at: Instant::now(),
				width: 1,
				height: 1,
				colour: ColourMetadata::sdr(),
			},
			data: FrameData::Pixels(PixelBuffer {
				format: PixelFormat::Rgba8,
				stride: 4,
				data: vec![0; 4],
			}),
		}
	}

	#[cfg(feature = "test-support")]
	#[test]
	fn latest_keeps_only_the_newest() {
		let (mut sender, receiver) = channel();

		assert_eq!(sender.offer(pixel_frame(0)).unwrap(), OfferOutcome::Enqueued);
		assert_eq!(sender.offer(pixel_frame(1)).unwrap(), OfferOutcome::Replaced);
		assert_eq!(sender.offer(pixel_frame(2)).unwrap(), OfferOutcome::Replaced);

		let frame = receiver
			.recv_timeout(Duration::from_millis(100))
			.expect("newest frame is available");
		assert_eq!(frame.info.sequence, 2, "the newest offer must survive");
		assert!(
			matches!(receiver.try_recv(), Err(TryRecvError::Empty)),
			"older frames were evicted"
		);
	}

	#[test]
	fn latest_recv_timeout_then_none() {
		let (_sender, receiver) = channel();
		assert!(matches!(
			receiver.recv_timeout(Duration::from_millis(20)),
			Err(RecvTimeoutError::Timeout)
		));
		assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
	}

	#[cfg(feature = "test-support")]
	#[test]
	fn receiver_drop_makes_offer_disconnected() {
		let (mut sender, receiver) = channel();
		drop(receiver);
		assert_eq!(sender.offer(pixel_frame(0)).unwrap_err(), Disconnected);
	}

	#[test]
	fn sender_drop_makes_recv_disconnected() {
		let (sender, receiver) = channel();
		drop(sender);
		assert!(matches!(
			receiver.recv_timeout(Duration::from_millis(50)),
			Err(RecvTimeoutError::Disconnected)
		));
		assert!(matches!(receiver.try_recv(), Err(TryRecvError::Disconnected)));
		assert!(matches!(receiver.recv(), Err(RecvError::Disconnected)));
	}

	#[test]
	fn sender_and_receiver_are_send() {
		fn assert_send<T: Send>() {}
		assert_send::<FrameSender>();
		assert_send::<FrameReceiver>();
	}
}
