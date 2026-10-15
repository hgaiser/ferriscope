//! ferriscope binary entry point: CLI parsing, logging setup, and hand-off to
//! `ferriscope-core`.
//!
//! ferriscope is a headless, single-application compositor whose output is an
//! encoder. It hosts one application, composites (or
//! direct-scans-out) its output, and hands frames to a sink.

use std::path::PathBuf;

use clap::Parser;

/// A headless, single-application streaming compositor.
#[derive(Debug, Parser)]
#[command(name = "ferriscope", version, about)]
struct Cli {
	/// Output width in pixels.
	#[arg(long, default_value_t = 1920)]
	width: u32,

	/// Output height in pixels.
	#[arg(long, default_value_t = 1080)]
	height: u32,

	/// Session refresh rate in Hz (the compositor-owned clock).
	#[arg(long, default_value_t = 60.0)]
	rate: f64,

	/// DRM render node used for offscreen rendering.
	#[arg(long, default_value = "/dev/dri/renderD128")]
	render_node: PathBuf,

	/// Directory for the Wayland socket (default: `$XDG_RUNTIME_DIR`).
	#[arg(long)]
	runtime_dir: Option<PathBuf>,

	/// Start Xwayland and host one fullscreen X11 window.
	#[arg(long)]
	xwayland: bool,

	/// Enable Steam integration, off by default (gamescope's `-e`/`--steam`).
	///
	/// Publishes the focused app id to Steam over the `GAMESCOPE_FOCUSED_APP`
	/// and `GAMESCOPE_FOCUSED_APP_GFX` root properties and derives the app id
	/// from the Steam atoms (`STEAM_GAME`/`STEAM_BIGPICTURE`). Atom
	/// classification still applies when off.
	#[arg(long)]
	steam: bool,

	/// Let Steam choose the base layer (gamescope's `steamcontrolled`
	/// virtual-connector strategy).
	///
	/// Steam names the focus window or app through
	/// `GAMESCOPECTRL_BASELAYER_WINDOW` / `GAMESCOPECTRL_BASELAYER_APPID` on the
	/// root window, and only windows with an app id (or the Steam UI) are
	/// considered. Implied by `--steam`, as gamescope's `-e` implies it.
	#[arg(long)]
	steam_controlled_focus: bool,

	/// XKB rules file for the seat keyboard (default: `XKB_DEFAULT_RULES`).
	#[arg(long, default_value = "")]
	xkb_rules: String,

	/// XKB keyboard model (default: `XKB_DEFAULT_MODEL`).
	#[arg(long, default_value = "")]
	xkb_model: String,

	/// XKB layout (default: `XKB_DEFAULT_LAYOUT`); Steam's
	/// `GAMESCOPE_KEYBOARD_LAYOUT` overrides it at runtime.
	#[arg(long, default_value = "")]
	xkb_layout: String,

	/// XKB variant (default: `XKB_DEFAULT_VARIANT`).
	#[arg(long, default_value = "")]
	xkb_variant: String,

	/// XKB options (default: `XKB_DEFAULT_OPTIONS`).
	#[arg(long, default_value = "")]
	xkb_options: String,

	/// What a touch does (gamescope's `--default-touch-mode`): 0 hover, 1 left
	/// click, 2 right click, 3 middle click, 4 passthrough as `wl_touch`,
	/// 5 disabled, 6 trackpad. Steam changes it at runtime.
	#[arg(long, default_value_t = 1)]
	default_touch_mode: u32,

	/// Unix socket carrying the input control stream.
	#[arg(long)]
	control_socket: Option<PathBuf>,

	/// Record composited frames as PPM into this directory.
	#[arg(long)]
	record_dir: Option<PathBuf>,

	/// Render frames into a DMA-BUF before readback.
	#[arg(long)]
	dmabuf: bool,

	/// Produce HDR frames (10-bit PQ or FP16 scRGB) when the focused content is
	/// HDR, and advertise HDR to the hosted application. Implies `--dmabuf`
	/// rendering for HDR content.
	#[arg(long)]
	hdr: bool,

	/// Stop recording after this many frames.
	#[arg(long)]
	record_max_frames: Option<u64>,

	/// Exit after compositing this many frames (useful for CI runs).
	#[arg(long)]
	stop_after_frames: Option<u64>,

	/// Hide the cursor after this many milliseconds without pointer motion
	/// (gamescope's `-C`, default 10000).
	#[arg(long, default_value_t = 10_000)]
	cursor_hide_time: u64,

	/// The application to host, argv-style (after `--`).
	#[arg(trailing_var_arg = true)]
	command: Vec<String>,
}

fn main() -> ferriscope_core::Result<()> {
	tracing_subscriber::fmt()
		.with_env_filter(
			tracing_subscriber::EnvFilter::try_from_default_env()
				.unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
		)
		.init();

	let cli = Cli::parse();

	let config = ferriscope_core::CompositorConfig {
		width: cli.width,
		height: cli.height,
		rate: cli.rate,
		render_node: cli.render_node,
		runtime_dir: cli.runtime_dir,
		command: (!cli.command.is_empty()).then_some(cli.command),
		xwayland: cli.xwayland,
		steam: cli.steam,
		// gamescope's `-e` promotes the default strategy to Steam-controlled
		// focus (`main.cpp:787`).
		focus_strategy: if cli.steam || cli.steam_controlled_focus {
			ferriscope_core::FocusStrategy::SteamControlled
		} else {
			ferriscope_core::FocusStrategy::SingleApplication
		},
		hdr: cli.hdr,
		seat: ferriscope_core::SeatConfig {
			keyboard: ferriscope_core::KeyboardConfig {
				rules: cli.xkb_rules,
				model: cli.xkb_model,
				layout: cli.xkb_layout,
				variant: cli.xkb_variant,
				options: cli.xkb_options,
			},
			touch_click_mode: ferriscope_core::TouchClickMode::from_raw(cli.default_touch_mode),
			cursor_hide_time: std::time::Duration::from_millis(cli.cursor_hide_time),
		},
		// The binary has no DMA-BUF consumer: frames are read back and recorded.
		output: ferriscope_core::FrameOutput::Recorder(ferriscope_core::RecorderOutput {
			dir: cli.record_dir,
			max_frames: cli.record_max_frames,
			stop_after_frames: cli.stop_after_frames,
			render_to_dmabuf: cli.dmabuf,
		}),
		control_socket: cli.control_socket,
	};
	tracing::info!(
		width = config.width,
		height = config.height,
		rate = config.rate,
		"starting ferriscope"
	);
	ferriscope_core::run(config)
}
