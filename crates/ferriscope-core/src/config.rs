//! Launch configuration.
//!
//! [`CompositorConfig`] is a plain struct of public fields with a sensible
//! [`Default`]; build one with struct-update syntax
//! (`CompositorConfig { width: 1280, ..CompositorConfig::default() }`) so that
//! a field added later does not break the call site. The related options are
//! grouped: [`FrameOutput`] chooses where frames go and how they are produced,
//! [`SeatConfig`] covers the keyboard, touch and cursor.

use std::{path::PathBuf, time::Duration};

use crate::{cursor::DEFAULT_CURSOR_HIDE_TIME, keyboard::KeyboardConfig, touch::TouchClickMode};

/// Default [`ChannelOutput::pool_size`].
pub(crate) const DEFAULT_POOL_SIZE: usize = 3;

/// Configuration for one compositor run.
#[derive(Debug, Clone)]
pub struct CompositorConfig {
	/// Output width in pixels.
	pub width: u32,
	/// Output height in pixels.
	pub height: u32,
	/// Session refresh rate, in Hz.
	pub rate: f64,
	/// DRM render node used for offscreen rendering.
	pub render_node: PathBuf,
	/// Directory the Wayland socket is created in; `$XDG_RUNTIME_DIR` when
	/// `None`.
	///
	/// The process environment is never modified. [`crate::Ready::runtime_dir`]
	/// reports the directory in use, and the hosted application spawned by
	/// [`Self::command`] is given it as `XDG_RUNTIME_DIR`.
	pub runtime_dir: Option<PathBuf>,
	/// Application to spawn and host, argv-style; `None` leaves launching it to
	/// the embedder, which connects to the socket named in [`crate::Ready`].
	pub command: Option<Vec<String>>,
	/// Start Xwayland and host X11 clients.
	pub xwayland: bool,
	/// Optional Steam integration, default off.
	///
	/// When on, the focused-app backchannel (`GAMESCOPE_FOCUSED_APP`,
	/// `GAMESCOPE_FOCUSED_APP_GFX`) is published to the X11 root so Steam can
	/// follow focus; the app id is the atom-derived Steam identity
	/// (`STEAM_GAME`/`STEAM_BIGPICTURE`). When off, nothing is published and no
	/// app id is derived, while the atom classification (overlay, game tier)
	/// still applies.
	///
	/// It also selects gamescope's `steamcontrolled` focus (`gamescope -e`):
	/// Steam chooses the base layer through `GAMESCOPECTRL_BASELAYER_WINDOW` and
	/// `GAMESCOPECTRL_BASELAYER_APPID` on the root window, and only windows with
	/// an app id or the Steam UI are focusable. When off, the best-ranked window
	/// is presented (gamescope's default).
	pub steam: bool,
	/// Produce HDR frames when the focused content is HDR; default off.
	///
	/// When on, the colour management protocols advertise HDR (so Mesa's
	/// Wayland WSI and vkd3d-proton offer HDR swapchains), the hosted
	/// application is told HDR is supported (`DXVK_HDR`, and
	/// `STEAM_GAMESCOPE_HDR_SUPPORTED` with [`Self::steam`]), and the pool
	/// follows the base layer's encoding: 10-bit PQ or FP16 scRGB frames, with
	/// `FrameInfo::colour` describing them. When off the output is always SDR
	/// 8-bit. The embedder enables it only when its client can display HDR.
	pub hdr: bool,
	/// Keyboard, touch and cursor behaviour.
	pub seat: SeatConfig,
	/// Where composited frames go and how they are produced.
	pub output: FrameOutput,
	/// Unix socket the compositor connects to for the text input control
	/// stream (see the `control` module); `None` for no control stream.
	///
	/// Only with the `test-support` feature: the in-process embedder injects
	/// input through [`crate::CompositorHandles::input`].
	#[cfg(feature = "test-support")]
	pub control_socket: Option<PathBuf>,
}

impl Default for CompositorConfig {
	fn default() -> Self {
		Self {
			width: 1920,
			height: 1080,
			rate: 60.0,
			render_node: PathBuf::from("/dev/dri/renderD128"),
			runtime_dir: None,
			command: None,
			xwayland: false,
			steam: false,
			hdr: false,
			seat: SeatConfig::default(),
			output: FrameOutput::default(),
			#[cfg(feature = "test-support")]
			control_socket: None,
		}
	}
}

/// The seat: keyboard layout, touch behaviour and cursor auto-hide.
#[derive(Debug, Clone)]
pub struct SeatConfig {
	/// XKB rule names for the seat keyboard. Steam's `GAMESCOPE_KEYBOARD_LAYOUT`
	/// overrides the layout and variant at runtime (see [`KeyboardConfig`]).
	pub keyboard: KeyboardConfig,
	/// The touch click mode at startup (gamescope's `--default-touch-mode`).
	///
	/// [`TouchClickMode::Left`], the default, is gamescope's: a touch moves the
	/// pointer and holds the left button. Steam changes it at runtime through
	/// `STEAM_TOUCH_CLICK_MODE` on the root window.
	pub touch_click_mode: TouchClickMode,
	/// Hide the cursor after this long without pointer motion or a button
	/// press (gamescope's `cursorHideTime`, `-C`; default 10 s,
	/// `steamcompmgr.cpp:1171`). A hidden cursor is not composited and does not
	/// damage the scene. When the input focus window is the Steam UI the
	/// pointer is also warped to its bottom-right corner (`:2029-2035`).
	pub cursor_hide_time: Duration,
}

impl Default for SeatConfig {
	fn default() -> Self {
		Self {
			keyboard: KeyboardConfig::default(),
			touch_click_mode: TouchClickMode::default(),
			cursor_hide_time: DEFAULT_CURSOR_HIDE_TIME,
		}
	}
}

/// Where composited frames go; chosen once, at launch.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum FrameOutput {
	/// Hand DMA-BUF frames to the in-process consumer over
	/// [`crate::CompositorHandles::frames`]. This is the product path.
	Channel(ChannelOutput),
	/// Read frames back to CPU pixels and write them to disk, for the
	/// standalone binary and the integration tests. `test-support` feature only.
	/// [`crate::CompositorHandles::frames`] then never carries a frame.
	#[cfg(feature = "test-support")]
	Recorder(RecorderOutput),
}

impl Default for FrameOutput {
	fn default() -> Self {
		Self::Channel(ChannelOutput::default())
	}
}

/// The DMA-BUF channel output (see [`FrameOutput::Channel`]).
///
/// Frames are composited RGB in the pool's format and delivered over a depth-1
/// mailbox: an undelivered frame is replaced by a newer one, so the consumer
/// only ever sees the newest frame and the compositor never blocks on it.
#[derive(Debug, Clone)]
pub struct ChannelOutput {
	/// Number of DMA-BUF buffers: the most the consumer may hold at once.
	///
	/// A slot stays busy from delivery until the consumer drops the frame.
	/// Slot availability is the only bound on frames in flight:
	/// with every slot held, no frame is rendered, and the instant a slot is
	/// released a pending frame is. One buffer is enough for correctness (a
	/// frame is rendered the moment the previous one is released); more lets
	/// the compositor render ahead while the consumer is still reading. Clamped
	/// to at least one.
	pub pool_size: usize,
}

impl Default for ChannelOutput {
	fn default() -> Self {
		Self {
			pool_size: DEFAULT_POOL_SIZE,
		}
	}
}

/// The PPM file recorder output (see [`FrameOutput::Recorder`]).
#[cfg(feature = "test-support")]
#[derive(Debug, Clone, Default)]
pub struct RecorderOutput {
	/// Directory frames are written to as `frame_<sequence>.ppm`; `None`
	/// discards them.
	pub dir: Option<PathBuf>,
	/// Stop writing after this many frames; later frames are accepted and
	/// dropped, which keeps a long run from filling the disk.
	pub max_frames: Option<u64>,
	/// Exit after this many composited frames (useful for CI runs).
	pub stop_after_frames: Option<u64>,
	/// Render into a GBM DMA-BUF and map it, instead of an offscreen
	/// renderbuffer; both are read back to the same CPU pixels.
	pub render_to_dmabuf: bool,
}

/// The output settings the render loop runs on, resolved from [`FrameOutput`].
pub(crate) struct OutputPlan {
	/// DMA-BUF pool size.
	pub pool_size: usize,
	/// Whether to render into a DMA-BUF even if the sink reads CPU pixels.
	pub render_to_dmabuf: bool,
	/// Exit after this many delivered frames.
	pub stop_after_frames: Option<u64>,
}

impl FrameOutput {
	/// Resolve the settings the render loop needs.
	///
	pub(crate) fn plan(&self) -> OutputPlan {
		match self {
			Self::Channel(channel) => OutputPlan {
				pool_size: channel.pool_size,
				render_to_dmabuf: true,
				stop_after_frames: None,
			},
			#[cfg(feature = "test-support")]
			Self::Recorder(recorder) => OutputPlan {
				pool_size: DEFAULT_POOL_SIZE,
				render_to_dmabuf: recorder.render_to_dmabuf,
				stop_after_frames: recorder.stop_after_frames,
			},
		}
	}
}
