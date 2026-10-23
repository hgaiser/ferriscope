//! The public library boundary: launch the compositor in-process.
//!
//! A host links `ferriscope` directly. [`Compositor::launch`]
//! runs the whole compositor on a dedicated thread — calloop's `EventLoop` is
//! not `Send`, so it is created and driven there — and returns a
//! [`Compositor`] (the producer/stop handle) plus [`CompositorHandles`] (the
//! consumer endpoints):
//!
//! - **frames** — owned DMA-BUFs with an optional acquire fence, over a
//!   depth-1 [`crate::delivery`] mailbox that keeps only the newest frame and
//!   never stalls the compositor. Dropping a frame returns its pool slot, which
//!   wakes the compositor to render a pending frame into it at once.
//! - **input** — control-stream events injected into the compositor's seat.
//! - **ready** — a one-shot handshake once the Wayland socket (and, with
//!   Xwayland, the X display) exists.
//!
//! [`run`] is the standalone form: the same launch, then block until the
//! compositor exits.

use std::path::PathBuf;
use std::sync::{
	Arc,
	atomic::{AtomicBool, Ordering},
	mpsc::{self, Receiver},
};

use smithay::reexports::calloop::{self, LoopSignal};

use crate::{
	config::{CompositorConfig, FrameOutput},
	delivery::{self, FrameReceiver, FrameSender, OfferOutcome},
	error::{Error, Result},
	frame::{Frame, FrameData, ReleaseGuard, ReleaseSender},
	seat::InputEvent,
	shell::{Endpoints, serve},
	sink::Sink,
};

/// Handshake reported once the compositor can accept clients.
#[derive(Debug, Clone)]
pub struct Ready {
	/// The Wayland display name clients should connect to.
	pub wayland_display: String,
	/// The directory holding the Wayland socket. A client needs it as
	/// `XDG_RUNTIME_DIR` when it differs from its own
	/// ([`CompositorConfig::runtime_dir`]).
	pub runtime_dir: PathBuf,
	/// The X11 display number, when Xwayland is running.
	pub xdisplay: Option<u32>,
	/// Whether HDR output is enabled ([`CompositorConfig::hdr`]).
	///
	/// This says the session *can* carry HDR, not that the current frame is:
	/// each frame's `FrameInfo::colour` says what it holds, and its pixel format
	/// (`Abgr2101010` for PQ, `Abgr16161616f` for scRGB) follows the focused
	/// content. Frames are SDR until an application presents HDR.
	pub hdr: bool,
	/// The environment the embedder must apply to the hosted application, on top
	/// of `WAYLAND_DISPLAY`/`DISPLAY`/`XDG_RUNTIME_DIR`.
	///
	/// This is the gamescope-impersonation block: every value is
	/// computed by the compositor from the real socket and X display in this
	/// handshake, so the embedder launches the app with the same environment the
	/// standalone `spawn_app` path uses. The session identity
	/// (`XDG_CURRENT_DESKTOP=gamescope`, `GAMESCOPE_WAYLAND_DISPLAY`,
	/// `STEAM_GAME_DISPLAY_0` with Xwayland) is always present, exactly as
	/// gamescope advertises it without `-e`; only the `STEAM_GAMESCOPE_*`
	/// capability vars are gated on `CompositorConfig::steam`.
	/// See `steam::app_env` for the exact set and the advertised-vs-backed
	/// caveats.
	pub env: Vec<(String, String)>,
}

impl Ready {
	/// Build the handshake and derive the hosted-app environment.
	pub(crate) fn new(
		wayland_display: String,
		runtime_dir: PathBuf,
		xdisplay: Option<u32>,
		hdr: bool,
		steam: bool,
	) -> Self {
		let mut env = crate::steam::app_env(steam, &wayland_display, xdisplay, hdr);
		env.push((
			"XDG_RUNTIME_DIR".to_string(),
			runtime_dir.to_string_lossy().into_owned(),
		));
		Self {
			wayland_display,
			runtime_dir,
			xdisplay,
			hdr,
			env,
		}
	}
}

/// The consumer end of an in-process compositor.
pub struct CompositorHandles {
	/// Composited frames; only the newest undelivered frame is kept. Drop a
	/// frame once you are done with its buffer to return the pool slot.
	///
	/// With the `test-support` recorder output no frame is ever sent here.
	pub frames: FrameReceiver,
	/// Inject keyboard/pointer events into the compositor's seat.
	pub input: InputSender,
	/// One-shot readiness handshake; fires after the socket is live.
	pub ready: Receiver<Ready>,
}

/// Clonable handle for injecting input into the compositor's seat.
///
/// Wraps the internal calloop channel so no calloop type appears in the public
/// API.
#[derive(Clone)]
pub struct InputSender(calloop::channel::Sender<InputEvent>);

impl InputSender {
	/// Send one input event.
	///
	/// Fails once the compositor has shut down and its input source is gone.
	pub fn send(&self, event: InputEvent) -> Result<()> {
		self.0
			.send(event)
			.map_err(|_| Error::Input("input channel closed".into()))
	}
}

/// Clonable handle that stops a [`Compositor`] from any thread.
///
/// Obtained from [`Compositor::stop_handle`]. It lets one thread block in
/// [`Compositor::wait`] (and so observe the exit, whether the compositor
/// stopped on its own or was stopped) while another, such as an embedder's
/// shutdown path, asks it to stop.
#[derive(Clone)]
pub struct StopHandle {
	signal: LoopSignal,
	/// Guard that survives the event loop's own stop-flag reset, so a `stop`
	/// issued while the compositor is still setting up is not lost.
	stop: Arc<AtomicBool>,
}

impl StopHandle {
	/// Ask the compositor to stop. Does not block; the owner's
	/// [`Compositor::wait`] returns once the thread has exited.
	///
	/// Harmless if the compositor has already exited or was already asked to
	/// stop.
	pub fn stop(&self) {
		self.stop.store(true, Ordering::Release);
		self.signal.stop();
		self.signal.wakeup();
	}
}

/// The producer end: owns the compositor thread and its stop signal.
pub struct Compositor {
	thread: Option<std::thread::JoinHandle<Result<()>>>,
	handle: StopHandle,
}

impl Compositor {
	/// Start the compositor on a dedicated thread.
	///
	/// Returns once the thread's calloop signal is available; use
	/// [`CompositorHandles::ready`] to wait for the Wayland socket (and, with
	/// `config.xwayland`, the X display) to exist.
	pub fn launch(config: CompositorConfig) -> Result<(Compositor, CompositorHandles)> {
		let (frames_tx, frames_rx) = delivery::channel();
		let (releases_tx, releases_rx) = ReleaseSender::channel();
		let (input_tx, input_rx) = calloop::channel::channel::<InputEvent>();
		let (ready_tx, ready_rx) = mpsc::sync_channel::<Ready>(1);
		let (signal_tx, signal_rx) = mpsc::sync_channel::<LoopSignal>(1);
		let stop = Arc::new(AtomicBool::new(false));
		let thread_stop = Arc::clone(&stop);

		let thread = std::thread::Builder::new()
			.name("ferriscope-compositor".to_string())
			.spawn(move || {
				// Hold a sender for the whole run. The release channel closes
				// when its last sender drops and the shell reads that as the
				// consumer going away; no consumer sender exists any more, so
				// only the compositor's own sinks may keep it open.
				let _release_channel = releases_tx.clone();
				let sink = build_sink(&config.output, frames_tx, releases_tx)?;
				let endpoints = Endpoints {
					sink,
					releases: releases_rx,
					input: input_rx,
					ready: ready_tx,
					signal: signal_tx,
					stop: thread_stop,
				};
				let result = serve(config, endpoints);
				// Surface post-startup exits: `launch` only observes this when
				// `stop`/Drop joins us, so without this a mid-session failure is
				// silent and the embedder sees only the frame channel closing.
				match &result {
					Err(e) => tracing::error!("compositor exited with error: {e}"),
					Ok(()) => tracing::debug!("compositor exited"),
				}
				result
			})
			.map_err(|e| Error::Wayland(format!("spawn compositor thread: {e}")))?;

		let signal = match signal_rx.recv() {
			Ok(signal) => signal,
			// The thread died before handing over its signal; surface the cause.
			Err(_) => {
				return match thread.join() {
					Ok(Ok(())) => Err(Error::Wayland("compositor exited during startup".into())),
					Ok(Err(e)) => Err(e),
					Err(_) => Err(Error::Wayland("compositor thread panicked during startup".into())),
				};
			},
		};

		Ok((
			Compositor {
				thread: Some(thread),
				handle: StopHandle { signal, stop },
			},
			CompositorHandles {
				frames: frames_rx,
				input: InputSender(input_tx),
				ready: ready_rx,
			},
		))
	}

	/// Stop the compositor and wait for its thread to exit.
	pub fn stop(mut self) -> Result<()> {
		self.shutdown()
	}

	/// Wait for the compositor to exit and return its result.
	///
	/// Returns when the compositor exits on its own (the hosted application
	/// exited, a frame limit was reached, the consumer went away, or it failed)
	/// or when a [`StopHandle`] stops it. A panic on the compositor thread is
	/// reported as an error.
	pub fn wait(mut self) -> Result<()> {
		self.join()
	}

	/// A handle that stops this compositor from another thread.
	pub fn stop_handle(&self) -> StopHandle {
		self.handle.clone()
	}

	fn join(&mut self) -> Result<()> {
		match self.thread.take() {
			Some(thread) => thread
				.join()
				.unwrap_or_else(|_| Err(Error::Wayland("compositor thread panicked".into()))),
			None => Ok(()),
		}
	}

	fn shutdown(&mut self) -> Result<()> {
		self.handle.stop();
		self.join()
	}
}

impl Drop for Compositor {
	fn drop(&mut self) {
		if self.thread.is_some()
			&& let Err(e) = self.shutdown()
		{
			tracing::warn!("compositor shutdown: {e}");
		}
	}
}

/// Run the compositor on a dedicated thread and block until it exits.
///
/// This is the standalone form of [`Compositor::launch`]: the same code path
/// with the consumer endpoints held internally. It returns when the hosted
/// application exits or a recorder frame limit is reached.
pub fn run(config: CompositorConfig) -> Result<()> {
	let (compositor, handles) = Compositor::launch(config)?;
	let result = compositor.wait();
	drop(handles);
	result
}

/// Build the sink for the configured output.
fn build_sink(output: &FrameOutput, frames: FrameSender, releases: ReleaseSender) -> Result<Box<dyn Sink>> {
	match output {
		FrameOutput::Channel(_) => Ok(Box::new(ChannelSink::new(frames, releases))),
		#[cfg(feature = "test-support")]
		FrameOutput::Recorder(recorder) => {
			// The frame mailbox has no producer: the recorder reads pixels back.
			drop((frames, releases));
			match &recorder.dir {
				Some(dir) => Ok(Box::new(crate::recorder::FileRecorder::new(
					dir.clone(),
					recorder.max_frames,
				)?)),
				None => Ok(Box::new(crate::recorder::NullSink)),
			}
		},
	}
}

/// Bridges the [`Sink`] trait to a [`crate::delivery`] mailbox.
///
/// A newer frame replaces an undelivered one: the compositor must never block
/// on a slow consumer. A DMA-BUF frame gets a [`ReleaseGuard`] before it is
/// offered, so a frame the consumer drops (or that is dropped here) returns its
/// pool slot.
struct ChannelSink {
	frames: FrameSender,
	releases: ReleaseSender,
}

impl ChannelSink {
	fn new(frames: FrameSender, releases: ReleaseSender) -> Self {
		Self { frames, releases }
	}

	/// Attach a release guard to a DMA-BUF frame, if not already present.
	fn install_release_guard(&self, mut frame: Frame) -> Frame {
		match &mut frame.data {
			FrameData::DmaBuf(dmabuf) => {
				if dmabuf.release.is_none() {
					dmabuf.release = Some(ReleaseGuard::new(self.releases.clone(), dmabuf.buffer_id));
				}
			},
			#[cfg(feature = "test-support")]
			FrameData::Pixels(_) => {},
		}
		frame
	}
}

impl Sink for ChannelSink {
	fn wants_dmabuf(&self) -> bool {
		true
	}

	fn submit(&mut self, frame: Frame) -> Result<()> {
		let frame = self.install_release_guard(frame);
		match self.frames.offer(frame) {
			// `Replaced` evicted an older frame, but this frame was taken; the
			// eviction is accounted for by the evicted frame's release guard.
			Ok(OfferOutcome::Enqueued | OfferOutcome::Replaced) => Ok(()),
			Err(delivery::Disconnected) => Err(Error::Sink("frame mailbox disconnected".into())),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	// The sink tests build CPU-pixel frames, which exist only with `test-support`.
	#[cfg(feature = "test-support")]
	fn pixel_frame(sequence: u64) -> Frame {
		use std::time::Instant;

		use crate::{
			colour_meta::ColourMetadata,
			frame::{FrameInfo, PixelBuffer, PixelFormat},
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
	fn channel_sink_replaces_an_undelivered_frame() {
		let (tx, rx) = delivery::channel();
		let (releases, _release_rx) = ReleaseSender::channel();
		let mut sink = ChannelSink::new(tx, releases);
		assert!(sink.wants_dmabuf());
		sink.submit(pixel_frame(0)).unwrap();
		// The second frame replaces the first rather than blocking.
		sink.submit(pixel_frame(1)).unwrap();
		assert_eq!(rx.try_recv().expect("newest frame").info.sequence, 1);
	}

	#[cfg(feature = "test-support")]
	#[test]
	fn channel_sink_reports_disconnect_as_error() {
		let (tx, rx) = delivery::channel();
		let (releases, _release_rx) = ReleaseSender::channel();
		let mut sink = ChannelSink::new(tx, releases);
		drop(rx);
		assert!(matches!(sink.submit(pixel_frame(0)), Err(Error::Sink(_))));
	}

	#[test]
	fn ready_carries_the_steam_environment_when_enabled() {
		let ready = Ready::new(
			"wayland-1".to_string(),
			PathBuf::from("/run/user/1000"),
			Some(3),
			false,
			true,
		);
		assert!(
			ready
				.env
				.contains(&("XDG_CURRENT_DESKTOP".to_string(), "gamescope".to_string()))
		);
		assert!(
			ready
				.env
				.contains(&("GAMESCOPE_WAYLAND_DISPLAY".to_string(), "wayland-1".to_string()))
		);
		assert!(
			ready
				.env
				.contains(&("STEAM_GAME_DISPLAY_0".to_string(), ":3".to_string()))
		);
		assert!(!ready.env.iter().any(|(key, _)| key == "STEAM_GAMESCOPE_HDR_SUPPORTED"));
	}

	#[test]
	fn ready_environment_carries_the_identity_without_steam() {
		// The session identity is unconditional, like gamescope without `-e`;
		// only the Steam capability vars are gated on `CompositorConfig::steam`.
		let ready = Ready::new(
			"wayland-1".to_string(),
			PathBuf::from("/run/user/1000"),
			Some(3),
			true,
			false,
		);
		assert!(
			ready
				.env
				.contains(&("XDG_CURRENT_DESKTOP".to_string(), "gamescope".to_string()))
		);
		assert!(
			ready
				.env
				.contains(&("GAMESCOPE_WAYLAND_DISPLAY".to_string(), "wayland-1".to_string()))
		);
		assert!(
			ready
				.env
				.contains(&("STEAM_GAME_DISPLAY_0".to_string(), ":3".to_string()))
		);
		assert!(!ready.env.iter().any(|(key, _)| key.starts_with("STEAM_GAMESCOPE_")));
	}

	#[cfg(feature = "test-support")]
	#[test]
	fn channel_sink_leaves_pixel_frames_unguarded() {
		// The guard is only meaningful for DMA-BUF frames; attaching it must be
		// a no-op for the pixel path and idempotent for a second call.
		let (tx, _rx) = delivery::channel();
		let (releases, _release_rx) = ReleaseSender::channel();
		let sink = ChannelSink::new(tx, releases);
		let frame = sink.install_release_guard(pixel_frame(0));
		assert!(matches!(frame.data, FrameData::Pixels(_)));
		let frame = sink.install_release_guard(frame);
		assert!(matches!(frame.data, FrameData::Pixels(_)));
	}
}
