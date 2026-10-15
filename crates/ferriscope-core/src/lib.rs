//! Core implementation of the ferriscope compositor.
//!
//! ferriscope is a **headless, single-application compositor whose output is an
//! encoder**. It hosts one fullscreen application, composites its output, and
//! hands frames to a `sink`.
//!
//! # Using it as a library
//!
//! [`Compositor::launch`] starts the compositor on its own thread from a
//! [`CompositorConfig`] and returns the producer handle plus the consumer
//! endpoints ([`CompositorHandles`]): DMA-BUF frames, input injection and a
//! readiness handshake. [`run`] is the same path for a standalone process.
//!
//! # Features
//!
//! `test-support` adds a CPU PPM recorder output (`FrameOutput::Recorder`) and
//! a text control socket (`CompositorConfig::control_socket`). It exists for
//! the `ferriscope` binary and the integration tests; embedders leave it off. It has no display, no modeset, no scanout and no
//! display-derived timing: it owns the only clock in the path.
//!
//! # Module map
//!
//! | Concern | Module |
//! |-----------------------------|--------------------|
//! | In-process library boundary | `compositor` |
//! | Launch configuration | `config` |
//! | Frame delivery policy | `delivery` |
//! | Frame contract | `frame` |
//! | Frame sink contract | `sink` |
//! | CPU recorders, text control socket (`test-support`) | `recorder`, `control` |
//! | Wayland server + shell | `shell` |
//! | Seat / input control stream | `seat` |
//! | Focus selection (gamescope port) + pointer capture | `focus` |
//! | Output fill scaling | `scaling` |
//! | gamescope action bindings (hotkeys) | `action_binding` |
//! | gamescope WSI protocols (override surfaces) | `swapchain` |
//! | Cursor position and client cursor images | `cursor` |
//! | Second X connection: Steam atoms in, focus contract out | `overlay` |
//! | Optional `--steam` mode | `steam` |
//! | Touch click modes | `touch` |
//! | Pen input via `zwp_tablet_v2` (ferriscope extension) | `shell::pen` |
//! | Steam on-screen keyboard (`gamescope_input_method`) | `ime` |
//! | Colour / HDR metadata | `colour_meta` |
//! | Colour management protocols (`wp_color_management_v1`, frog) | `colour_management` |
//! | HDR content classification (swapchain + protocols) | `hdr` |

#![deny(missing_docs)]

mod action_binding;
mod colour_management;
mod colour_meta;
mod compositor;
mod config;
#[cfg(feature = "test-support")]
mod control;
mod cursor;
mod delivery;
mod error;
mod focus;
mod frame;
mod hdr;
mod ime;
mod keyboard;
mod overlay;
mod process;
#[cfg(feature = "test-support")]
mod recorder;
mod scaling;
mod seat;
mod shell;
mod sink;
mod steam;
mod swapchain;
mod touch;

pub use colour_meta::{
	Chromaticity, ColourMetadata, ColourPrimaries, ColourRange, ContentLightLevel, MasteringDisplay,
	MatrixCoefficients, TransferFunction,
};
pub use compositor::{Compositor, CompositorHandles, InputSender, Ready, StopHandle, run};
#[cfg(feature = "test-support")]
pub use config::RecorderOutput;
pub use config::{ChannelOutput, CompositorConfig, FrameOutput, SeatConfig};
pub use delivery::{FrameReceiver, RecvError, RecvTimeoutError, TryRecvError};
pub use error::{Error, Result};
pub use frame::{DmaBufFrame, Frame, FrameData, FrameInfo};
#[cfg(feature = "test-support")]
pub use frame::{PixelBuffer, PixelFormat};
pub use keyboard::KeyboardConfig;
pub use seat::{InputEvent, PenEvent, PenTool, pen_tilt};
#[cfg(feature = "test-support")]
pub use steam::display_id_to_server_id;
pub use touch::TouchClickMode;

// Re-export the smithay buffer types exposed by the public frame API, so a
// consumer does not need to depend on the same smithay git revision. `Dmabuf`
// and `Format` (aka `DrmFormat`) are what `DmaBufFrame::dmabuf` uses; `Fourcc`
// and `Modifier` describe pixel format and layout.
pub use smithay::backend::allocator::dmabuf::Dmabuf;
pub use smithay::backend::allocator::{Format, Fourcc, Modifier};
