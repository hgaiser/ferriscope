use thiserror::Error;

/// Result type used throughout the core library.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Core library error type.
///
/// One typed enum for the whole port; the binary uses it directly at its
/// boundary. No catch-all error crate.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
	/// Underlying I/O failure.
	#[error(transparent)]
	Io(#[from] std::io::Error),

	/// Neither `CompositorConfig::runtime_dir` nor `$XDG_RUNTIME_DIR` names a
	/// directory for the Wayland socket.
	#[error("$XDG_RUNTIME_DIR is not set")]
	MissingRuntimeDir,

	// --- configuration ----------------------------------------------------
	/// Invalid or contradictory configuration.
	#[error("invalid configuration: {0}")]
	Config(String),

	// --- gpu / render -----------------------------------------------------
	/// A GPU (Vulkan or GBM) resource could not be created or used.
	#[error("gpu: {0}")]
	Gpu(String),

	/// A render pass failed.
	#[error("render: {0}")]
	Render(String),

	// --- wayland / compositor --------------------------------------------
	/// The Wayland display or a protocol global could not be set up.
	#[error("wayland: {0}")]
	Wayland(String),

	// --- xwayland ---------------------------------------------------------
	/// Xwayland could not be started or its window manager failed.
	#[error("xwayland: {0}")]
	Xwayland(String),

	// --- input ------------------------------------------------------------
	/// The input control stream could not be read or decoded.
	#[error("input: {0}")]
	Input(String),

	// --- sink -------------------------------------------------------------
	/// A frame could not be delivered to the sink.
	#[error("sink: {0}")]
	Sink(String),
}
