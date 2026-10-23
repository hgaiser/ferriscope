//! Wayland server and compositor shell.
//!
//! Owns the Wayland display, the protocol globals (`wl_compositor`, `xdg_shell`,
//! `wl_shm`, `linux-dmabuf`, `viewporter`, `wl_output`, `wl_seat`), the
//! [`smithay::desktop::Space`] holding the single fullscreen toplevel, the
//! compositor-owned render loop, and the hand-off to a [`crate::sink::Sink`].
//!
//! There is no display and no scanout: frames are rendered into a DMA-BUF
//! offscreen and either read back for the file-recorder sink or handed to
//! the encoder sink. Frames are delivered as soon as the scene changes and a
//! pool slot is free, paced by the scheduler in [`schedule`] — never by a page
//! flip or presentation feedback.

use std::{
	path::{Path, PathBuf},
	process::{Child, Command, Stdio},
	sync::{Arc, atomic::AtomicBool},
	time::{Duration, Instant},
};

#[cfg(feature = "test-support")]
use smithay::backend::renderer::gles::GlesRenderbuffer;
use smithay::{
	backend::{
		allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
		drm::DrmDeviceFd,
		egl::{EGLContext, EGLDisplay},
		renderer::{
			ImportDma, ImportMemWl,
			gles::{GlesRenderer, GlesTexProgram},
		},
	},
	desktop::{PopupManager, Space, Window},
	input::{Seat, SeatState},
	output::{Mode as OutputMode, Output, PhysicalProperties, Scale as OutputScale, Subpixel},
	reexports::{
		calloop::{
			self, EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction, RegistrationToken, generic::Generic,
		},
		wayland_server::{
			Display, DisplayHandle, ListeningSocket,
			backend::{ClientData, ClientId, DisconnectReason, GlobalId},
			protocol::wl_surface::WlSurface,
		},
	},
	utils::{Buffer as BufferCoord, DeviceFd, Logical, Physical, Point, Size, Transform},
	wayland::{
		compositor::{CompositorClientState, CompositorState},
		dmabuf::{DmabufFeedbackBuilder, DmabufGlobal, DmabufState},
		drm_syncobj::{DrmSyncobjState, supports_syncobj_eventfd},
		output::OutputManagerState,
		pointer_constraints::PointerConstraintsState,
		relative_pointer::RelativePointerManagerState,
		selection::data_device::DataDeviceState,
		shell::xdg::XdgShellState,
		shm::ShmState,
		xdg_activation::XdgActivationState,
		xwayland_keyboard_grab::XWaylandKeyboardGrabState,
		xwayland_shell::XWaylandShellState,
	},
	xwayland::X11Wm,
};

use crate::{
	action_binding::ActionBindings,
	colour_management::ColourManagementGlobals,
	compositor::Ready,
	config::{CompositorConfig, OutputPlan},
	cursor::Cursor,
	cursor::{CursorState, CursorSuspension},
	error::{Error, Result},
	focus::{FocusControl, FocusDirty, PointerCapture, WindowId},
	frame::Release,
	hdr::ContentKind,
	overlay::SteamAtoms,
	seat::InputEvent,
	sink::Sink,
	swapchain::{FactoryRegistry, SwapchainGlobals},
};

mod content;
mod fifo;
mod focus_pass;
mod geometry;
mod handlers;
mod hdr;
mod ime;
mod input;
mod pen;
mod presentation;
mod render;
mod schedule;
mod sync;
mod touch;
mod windows;

use crate::shell::content::ContentOverrides;
use crate::shell::focus_pass::AppliedFocus;
use crate::shell::ime::ImeTyper;
use crate::shell::pen::PenState;
use crate::shell::render::damage::FrameDamage;
use crate::shell::render::elements::FrameScratch;
use crate::shell::render::elements::{RenderFormat, pick_render_format};
use crate::shell::render::pool::DmabufPool;
use crate::shell::schedule::Scheduler;
use crate::shell::touch::TouchState;
use crate::shell::windows::WindowTable;

/// Client data carried per connected Wayland client.
#[derive(Default)]
pub struct ClientState {
	/// Per-client compositor state (blockers, scale).
	pub compositor_state: CompositorClientState,
}

/// A Steam overlay or notification, tracked outside the [`Space`].
///
/// Overlays are drawn as their own layer over the fullscreen window and under
/// the cursor (`overlay`), so they are not mapped into `space`; they keep their
/// [`Window`] for frame callbacks and liveness, their output-space location, and
/// the [`OverlayLayer`] that fixes their layer. The list is rebuilt by every
/// focus pass from what [`crate::focus::pick`] chose.
struct OverlayWindow {
	id: WindowId,
	window: Window,
	role: OverlayLayer,
	location: Point<i32, Logical>,
}

/// Which Steam overlay layer a window is drawn in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::shell) enum OverlayLayer {
	/// A notification: drawn above the interactive overlay, never focused.
	Notification,
	/// The interactive overlay (`overlayWindow`): full output width or
	/// `STEAM_INPUT_FOCUS`.
	Interactive,
}

impl ClientData for ClientState {
	fn initialized(&self, _client_id: ClientId) {}

	fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

/// The compositor state.
pub struct FerriscopeState {
	config: CompositorConfig,
	start_time: Instant,
	display_handle: DisplayHandle,
	socket_name: std::ffi::OsString,
	loop_handle: LoopHandle<'static, Self>,
	loop_signal: LoopSignal,
	child: Option<Child>,
	/// Whether a pidfd source reports the child's exit (otherwise the frame
	/// timer polls it).
	child_watched: bool,

	compositor_state: CompositorState,
	xdg_shell_state: XdgShellState,
	shm_state: ShmState,
	dmabuf_state: DmabufState,
	/// `wp_linux_drm_syncobj_manager_v1`, present only when the render node
	/// supports syncobj timelines with eventfd (see [`sync`]).
	syncobj_state: Option<DrmSyncobjState>,
	#[allow(dead_code)]
	dmabuf_global: DmabufGlobal,
	#[allow(dead_code)]
	output_manager_state: OutputManagerState,
	#[allow(dead_code)]
	viewporter_state: smithay::wayland::viewporter::ViewporterState,
	seat_state: SeatState<Self>,
	seat: Seat<Self>,
	#[allow(dead_code)]
	pointer_constraints_state: PointerConstraintsState,
	#[allow(dead_code)]
	relative_pointer_state: RelativePointerManagerState,
	#[allow(dead_code)]
	xdg_activation_state: XdgActivationState,
	data_device_state: DataDeviceState,
	popups: PopupManager,
	#[allow(dead_code)]
	xwayland_shell_state: XWaylandShellState,
	#[allow(dead_code)]
	xwayland_keyboard_grab_state: XWaylandKeyboardGrabState,

	output: Output,
	/// The windows being drawn: the focus window and its override, nothing else.
	space: Space<Window>,
	/// Every tracked window and its cached metadata (see [`windows`]).
	windows: WindowTable,
	/// Process-derived app ids by client pid (see `AppIdCache`).
	pid_app_ids: crate::process::AppIdCache,
	/// The WSI layer's override surfaces (see [`content`]).
	content_overrides: ContentOverrides,
	/// What the last focus pass applied (see [`AppliedFocus`]).
	focus: AppliedFocus,
	/// Steam's `GAMESCOPECTRL_BASELAYER_*` choice, read from the root window.
	focus_control: FocusControl,
	/// `GAMESCOPE_FPS_LIMIT` in Hz, 0 for none (`g_nSteamCompMgrTargetFPS`).
	fps_limit: u32,
	/// Steam's `GAMESCOPE_KEYBOARD_LAYOUT` (`layout[:variant]`), empty when unset.
	steam_keyboard_layout: String,
	/// gamescope's focus dirty serial (`MakeFocusDirty`/`IsDirty`): the focus
	/// pass re-runs only while it is dirty.
	focus_dirty: FocusDirty,
	/// The overlay and notification the last pass picked (`overlay`), drawn
	/// between the fullscreen window and the cursor. They are deliberately not in
	/// `space`.
	overlays: Vec<OverlayWindow>,
	/// The Steam atoms, read over a second Xwayland connection.
	///
	/// `None` when Xwayland is off or the connection failed; classification then
	/// degrades to the plain ICCCM rule.
	steam: Option<SteamAtoms>,
	/// Active pointer capture (lock/confine) for the focused window.
	capture: PointerCapture,
	/// The surface the active capture belongs to, if any.
	capture_surface: Option<WlSurface>,
	/// The client's last locked-pointer cursor-position hint, restored when the
	/// lock is released.
	cursor_position_hint: Option<(WlSurface, Point<f64, Logical>)>,

	renderer: GlesRenderer,
	allocator: GbmAllocator<Arc<std::fs::File>>,
	/// The pool's current format: the SDR one, or an HDR one while the base
	/// layer is HDR (see [`Self::select_output_encoding`]).
	render_format: RenderFormat,
	/// The 8-bit format the pool uses for SDR output.
	sdr_format: RenderFormat,
	/// What the pool's buffers hold: SDR, or HDR10 PQ / scRGB when
	/// [`CompositorConfig::hdr`] is on and the base layer is HDR.
	output_encoding: ContentKind,
	/// Whether `GAMESCOPE_COLOR_APP_WANTS_HDR_FEEDBACK` currently says the
	/// base layer is HDR (`g_bAppWantsHDRCached`, `steamcompmgr.cpp:10398`).
	app_wants_hdr: bool,
	/// Steam's SDR white level in nits: how bright SDR content is drawn over HDR
	/// (`flSDROnHDRBrightness`).
	sdr_white_nits: f32,
	/// The SDR-to-HDR conversion shader; compiled when HDR output is on.
	sdr_program: Option<GlesTexProgram>,
	/// Whether the missing-HDR-format warning was logged.
	hdr_format_warned: bool,
	use_dmabuf: bool,
	/// The last passthrough decision (`Some(None)` is passthrough), so only
	/// changes are logged (see [`render::passthrough`]).
	passthrough_blocker: Option<Option<render::passthrough::Blocker>>,
	/// Exit after this many delivered frames.
	stop_after_frames: Option<u64>,
	/// The directory the Wayland socket lives in.
	runtime_dir: PathBuf,
	/// The DMA-BUF buffer pool and its delivery bookkeeping.
	pool: DmabufPool,
	/// The offscreen readback target of the non-DMA-BUF render path.
	#[cfg(feature = "test-support")]
	frame_target: Option<GlesRenderbuffer>,
	/// The damage tracker and the buffer age of each render target.
	damage: FrameDamage,
	/// Per-frame buffers kept across frames to avoid reallocating them.
	render_scratch: FrameScratch,
	frame_size: Size<i32, BufferCoord>,

	sink: Box<dyn Sink>,
	/// Pointer position and effective visibility, owned by `cursor`.
	cursor: Cursor,
	/// Client-supplied cursor image and its visibility policy.
	cursor_state: CursorState,
	/// Cursor auto-hide (`bCursorHidden`, `ulLastMovedCursorTime`).
	cursor_suspension: CursorSuspension,

	/// The text control stream, when `CompositorConfig::control_socket` is set.
	#[cfg(feature = "test-support")]
	control: Option<crate::control::UnixControlStream>,
	/// Slot releases from the consumer; taken when the sources are inserted.
	releases: Option<calloop::channel::Channel<Release>>,
	/// Input from the consumer; taken when the sources are inserted.
	input: Option<calloop::channel::Channel<InputEvent>>,
	/// One-shot readiness handshake; taken when it fires.
	ready: Option<std::sync::mpsc::SyncSender<Ready>>,
	/// Wayland display name, mirrored into [`Ready`].
	wayland_display: String,
	/// Whether the session can produce HDR frames ([`CompositorConfig::hdr`]).
	hdr: bool,
	/// External stop request from [`crate::compositor::Compositor::stop`].
	external_stop: Arc<AtomicBool>,
	xwm: Option<X11Wm>,
	/// The `gamescope_action_binding_manager` global Steam binds its keyboard
	/// hotkeys to. Kept so the global lives as
	/// long as the compositor.
	#[allow(dead_code)]
	action_binding_global: GlobalId,
	/// The `ferriscope_swapchain_factory_v1` global `ferriscope-wsi` binds, and
	/// the `gamescope_xwayland` and swapchain factory globals a stock gamescope
	/// layer binds.
	_swapchain_globals: SwapchainGlobals,
	/// The bound `ferriscope_swapchain_factory_v1` objects.
	swapchain_factories: FactoryRegistry,
	/// The identity of the Xwayland server, published on its root window as
	/// `GAMESCOPE_XWAYLAND_SERVER_ID` and `FERRISCOPE_XWAYLAND_SERVER_ID` and
	/// named by `override_window_content`. `None` before Xwayland is up.
	xwayland_server_id: Option<u32>,
	/// `wp_color_manager_v1` and `frog_color_management_factory_v1`.
	_colour_globals: ColourManagementGlobals,
	/// Typing state shared by the input methods (see [`ime`]).
	ime: ImeTyper,
	/// Touch routing state (see [`touch`]).
	touch: TouchState,
	/// Pen (graphics tablet) state (see [`pen`]).
	pen: PenState,
	/// The armed keyboard-hotkey bindings registered over that global.
	pub(crate) action_bindings: ActionBindings,
	/// When frames and frame callbacks go out (see [`Scheduler`]).
	scheduler: Scheduler,
	/// The single event-loop timer and the deadline it is armed for.
	timer: Option<(RegistrationToken, Instant)>,
}

/// Thread-owned endpoints [`serve`] runs against, built by
/// [`crate::compositor::Compositor::launch`].
pub(crate) struct Endpoints {
	/// Where composited frames go.
	pub sink: Box<dyn Sink>,
	/// Slot releases from the consumer.
	pub releases: calloop::channel::Channel<Release>,
	/// Input events from the consumer.
	pub input: calloop::channel::Channel<InputEvent>,
	/// One-shot ready signal.
	pub ready: std::sync::mpsc::SyncSender<Ready>,
	/// Hand-off point for the event loop's stop signal.
	pub signal: std::sync::mpsc::SyncSender<LoopSignal>,
	/// External stop flag, polled every dispatch.
	pub stop: Arc<AtomicBool>,
}

/// Run the compositor until it is stopped, the hosted application exits, or a
/// frame limit is reached.
///
/// Must be called on the thread that will drive the event loop: calloop's
/// `EventLoop` is not `Send`. The process environment is never modified: the
/// socket directory comes from [`CompositorConfig::runtime_dir`] (or
/// `$XDG_RUNTIME_DIR`) and the hosted application gets its environment through
/// `Command::env`.
pub(crate) fn serve(config: CompositorConfig, endpoints: Endpoints) -> Result<()> {
	let mut event_loop: EventLoop<FerriscopeState> =
		EventLoop::try_new().map_err(|e| Error::Wayland(format!("event loop: {e:?}")))?;

	// Hand the stop signal out before setup so `Compositor::launch` returns
	// promptly; the `Ready` handshake is what gates actual usability.
	let _ = endpoints.signal.send(event_loop.get_signal());

	let display: Display<FerriscopeState> = Display::new().map_err(|e| Error::Wayland(format!("display: {e:?}")))?;

	let mut state = FerriscopeState::new(&mut event_loop, display, config, endpoints)?;
	state.insert_sources(&mut event_loop)?;
	state.start_xwayland()?;

	// Without Xwayland the socket is the last thing a consumer waits for.
	if !state.config.xwayland {
		state.signal_ready(None);
	}

	if let Some(argv) = state.config.command.clone()
		&& !state.config.xwayland
	{
		state.spawn_app(&argv, None)?;
	}

	event_loop
		.run(Some(Duration::from_millis(50)), &mut state, |state| {
			state.post_dispatch()
		})
		.map_err(|e| Error::Wayland(format!("dispatch: {e:?}")))?;

	Ok(())
}

impl FerriscopeState {
	fn new(
		event_loop: &mut EventLoop<'static, Self>,
		display: Display<Self>,
		config: CompositorConfig,
		endpoints: Endpoints,
	) -> Result<Self> {
		let Endpoints {
			sink,
			releases,
			input,
			ready,
			signal: _,
			stop,
		} = endpoints;
		let OutputPlan {
			pool_size,
			render_to_dmabuf,
			stop_after_frames,
		} = config.output.plan();
		let runtime_dir = match &config.runtime_dir {
			Some(dir) => dir.clone(),
			None => std::env::var_os("XDG_RUNTIME_DIR")
				.filter(|dir| !dir.is_empty())
				.map(PathBuf::from)
				.ok_or(Error::MissingRuntimeDir)?,
		};
		let start_time = Instant::now();
		let display_handle = display.handle();

		// --- GPU: offscreen renderer -----------------------------
		let file = std::fs::OpenOptions::new()
			.read(true)
			.write(true)
			.open(&config.render_node)
			.map_err(|e| Error::Gpu(format!("open {}: {e}", config.render_node.display())))?;
		let syncobj_fd = file
			.try_clone()
			.map_err(|e| Error::Gpu(format!("dup {}: {e}", config.render_node.display())))?;
		let gbm = GbmDevice::new(Arc::new(file)).map_err(|e| Error::Gpu(format!("gbm device: {e:?}")))?;
		let display_egl =
			unsafe { EGLDisplay::new(gbm.clone()) }.map_err(|e| Error::Gpu(format!("egl display: {e:?}")))?;
		let context = EGLContext::new(&display_egl).map_err(|e| Error::Gpu(format!("egl context: {e:?}")))?;
		let mut renderer = unsafe { GlesRenderer::new(context) }.map_err(|e| Error::Gpu(format!("gles: {e:?}")))?;
		let allocator = GbmAllocator::new(gbm, GbmBufferFlags::RENDERING);
		let render_format = pick_render_format(&renderer)?;
		let sdr_program = if config.hdr {
			Some(render::hdr_shader::compile(&mut renderer)?)
		} else {
			None
		};

		// --- Wayland globals ------------------------------------------------
		let compositor_state = CompositorState::new::<Self>(&display_handle);
		let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
		let mut shm_state = ShmState::new::<Self>(&display_handle, vec![]);
		shm_state.update_formats(renderer.shm_formats());

		let mut dmabuf_state = DmabufState::new();
		// Advertise `zwp_linux_dmabuf_v1` with device feedback (version 6).
		// Xwayland only enables glamor/DRI3 when the compositor offers either
		// the legacy `wl_drm` global or `linux-dmabuf` v4+; without it Xwayland
		// falls back to software GLX and no DRI3, so every GPU client (a game,
		// DXVK, a Vulkan swapchain) is forced through llvmpipe and cannot reach
		// the session rate.
		let render_node_dev = {
			use std::os::unix::fs::MetadataExt;
			std::fs::metadata(&config.render_node)
				.map_err(|e| Error::Gpu(format!("stat {}: {e}", config.render_node.display())))?
				.rdev()
		};
		let default_feedback = DmabufFeedbackBuilder::new(render_node_dev, renderer.dmabuf_formats())
			.build()
			.map_err(|e| Error::Gpu(format!("dmabuf feedback: {e}")))?;
		let dmabuf_global =
			dmabuf_state.create_global_with_default_feedback::<Self>(&display_handle, &default_feedback);

		// Explicit sync. Advertised only when the kernel can signal a blocker
		// through an eventfd, like gamescope (`wlserver.cpp`): without that an
		// acquire point could not be waited on without blocking the loop.
		let import_device = DrmDeviceFd::new(DeviceFd::from(std::os::fd::OwnedFd::from(syncobj_fd)));
		let syncobj_state = if supports_syncobj_eventfd(&import_device) {
			Some(DrmSyncobjState::new::<Self>(&display_handle, import_device))
		} else {
			tracing::info!("render node lacks syncobj eventfd support; explicit sync not advertised");
			None
		};

		let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&display_handle);
		let viewporter_state = smithay::wayland::viewporter::ViewporterState::new::<Self>(&display_handle);
		// Nested compositors (kwin) refuse to start without single-pixel buffers.
		let _ = smithay::wayland::single_pixel_buffer::SinglePixelBufferState::new::<Self>(&display_handle);
		let _ = smithay::wayland::shell::xdg::decoration::XdgDecorationState::new::<Self>(&display_handle);
		let pointer_constraints_state = PointerConstraintsState::new::<Self>(&display_handle);
		let relative_pointer_state = RelativePointerManagerState::new::<Self>(&display_handle);
		// `wp_presentation` (`wlserver.cpp:1585`). The global lives as long as
		// the display, so the returned handle is not kept.
		let _ = smithay::wayland::presentation::PresentationState::new::<Self>(
			&display_handle,
			presentation::PRESENTATION_CLOCK_ID,
		);
		// `wp_fifo_v1`, managed by Smithay; the barriers are released on frame
		// delivery (see [`fifo`]).
		let _ = smithay::wayland::fifo::FifoManagerState::new::<Self>(&display_handle);
		let xdg_activation_state = XdgActivationState::new::<Self>(&display_handle);
		let data_device_state = DataDeviceState::new::<Self>(&display_handle);
		let popups = PopupManager::default();
		let xwayland_shell_state = XWaylandShellState::new::<Self>(&display_handle.clone());
		let xwayland_keyboard_grab_state = XWaylandKeyboardGrabState::new::<Self>(&display_handle.clone());

		// --- Seat -----------------------------------------------------------
		let mut seat_state = SeatState::new();
		let mut seat = seat_state.new_wl_seat(&display_handle, "ferriscope");
		seat.add_keyboard(config.seat.keyboard.xkb_config(), 200, 25)
			.map_err(|e| Error::Input(format!("add keyboard: {e:?}")))?;
		seat.add_pointer();
		seat.add_touch();
		let _ = smithay::wayland::tablet_manager::TabletManagerState::new::<Self>(&display_handle);

		// --- Output ---------------------------------------------------------
		let phys_size: Size<i32, Physical> = Size::from((config.width as i32, config.height as i32));
		let mode = OutputMode {
			size: phys_size,
			refresh: (config.rate * 1000.0) as i32,
		};
		let output = Output::new(
			"ferriscope-0".to_string(),
			PhysicalProperties {
				size: Size::<i32, smithay::utils::Raw>::from((0, 0)),
				subpixel: Subpixel::Unknown,
				make: "ferriscope".into(),
				model: "headless".into(),
				serial_number: "0".into(),
			},
		);
		output.create_global::<Self>(&display_handle);
		output.set_preferred(mode);
		output.change_current_state(Some(mode), Some(Transform::Normal), Some(OutputScale::Integer(1)), None);

		let mut space = Space::default();
		space.map_output(&output, (0, 0));

		// --- Sink ----------------------------------------------------------
		let wants_dmabuf = sink.wants_dmabuf();
		// HDR output switches the pool's format, so it needs the pool.
		let use_dmabuf = render_to_dmabuf || wants_dmabuf || config.hdr;
		let pool_size = pool_size.max(1);

		let loop_handle = event_loop.handle();
		let loop_signal = event_loop.get_signal();

		let socket_name = Self::init_wayland_listener(display, event_loop, &runtime_dir)?;
		let wayland_display = socket_name.to_string_lossy().into_owned();

		let frame_size = Size::from((config.width as i32, config.height as i32));
		let cursor_start = Point::from((f64::from(config.width) / 2.0, f64::from(config.height) / 2.0));

		// `gamescope_action_binding_manager`: Steam binds its
		// Guide/QAM keyboard hotkeys here. Always advertised, like gamescope.
		let action_binding_global = ActionBindings::global(&display_handle);
		let swapchain_globals = SwapchainGlobals::new(&display_handle);
		// `gamescope_input_method_manager`: Steam's on-screen keyboard.
		// A global outlives its id, so the id is not kept.
		let _ = crate::ime::global(&display_handle);

		#[cfg(feature = "test-support")]
		let control = config
			.control_socket
			.as_deref()
			.map(crate::control::UnixControlStream::connect)
			.transpose()?;

		let scheduler = Scheduler::new(config.rate, Instant::now());
		let cursor_hide_time = config.seat.cursor_hide_time;
		let touch_click_mode = config.seat.touch_click_mode;
		let config_hdr = config.hdr;
		let colour_globals = ColourManagementGlobals::new(&display_handle, config_hdr);

		let mut state = Self {
			config,
			start_time,
			display_handle,
			socket_name,
			loop_handle,
			loop_signal,
			child: None,
			child_watched: false,

			compositor_state,
			xdg_shell_state,
			shm_state,
			dmabuf_state,
			syncobj_state,
			dmabuf_global,
			output_manager_state,
			viewporter_state,
			seat_state,
			seat,
			pointer_constraints_state,
			relative_pointer_state,
			xdg_activation_state,
			data_device_state,
			popups,
			xwayland_shell_state,
			xwayland_keyboard_grab_state,

			output,
			space,
			windows: WindowTable::default(),
			pid_app_ids: crate::process::AppIdCache::default(),
			content_overrides: ContentOverrides::default(),
			focus: AppliedFocus::default(),
			focus_control: FocusControl::default(),
			fps_limit: 0,
			steam_keyboard_layout: String::new(),
			focus_dirty: FocusDirty::new(),
			overlays: Vec::new(),
			steam: None,
			capture: PointerCapture::None,
			capture_surface: None,
			cursor_position_hint: None,

			renderer,
			allocator,
			render_format: render_format.clone(),
			sdr_format: render_format,
			output_encoding: ContentKind::Sdr,
			app_wants_hdr: false,
			sdr_white_nits: crate::hdr::DEFAULT_SDR_WHITE_NITS,
			sdr_program,
			hdr_format_warned: false,
			use_dmabuf,
			passthrough_blocker: None,
			stop_after_frames,
			runtime_dir,
			pool: DmabufPool::new(pool_size),
			#[cfg(feature = "test-support")]
			frame_target: None,
			damage: FrameDamage::new(Size::from((frame_size.w, frame_size.h)), pool_size),
			render_scratch: FrameScratch::default(),
			frame_size,

			sink,
			// Start at the output centre rather than the top-left corner.
			cursor: Cursor {
				location: cursor_start,
				..Cursor::default()
			},
			cursor_state: CursorState::new(),
			cursor_suspension: CursorSuspension::new(cursor_hide_time),

			#[cfg(feature = "test-support")]
			control,
			releases: Some(releases),
			input: Some(input),
			ready: Some(ready),
			wayland_display,
			hdr: config_hdr,
			external_stop: stop,
			xwm: None,
			action_binding_global,
			_swapchain_globals: swapchain_globals,
			swapchain_factories: FactoryRegistry::default(),
			xwayland_server_id: None,
			_colour_globals: colour_globals,
			ime: ImeTyper::default(),
			touch: TouchState::new(touch_click_mode),
			pen: PenState::default(),
			action_bindings: ActionBindings::new(),
			scheduler,
			timer: None,
		};
		// A deliberate extension: gamescope has no tablet support. The tablet and
		// both tools exist before any client binds the manager, so every client
		// (Xwayland included) is told about them on `get_tablet_seat`.
		state.add_virtual_tablet();

		Ok(state)
	}

	/// Bind the first free `wayland-N` socket in `runtime_dir` and accept
	/// clients on it. Returns the socket's name.
	fn init_wayland_listener(
		display: Display<Self>,
		event_loop: &mut EventLoop<'static, Self>,
		runtime_dir: &Path,
	) -> Result<std::ffi::OsString> {
		// `wayland-0` is skipped: a client defaulting to it may reach another
		// compositor.
		let (socket_name, listening_socket) = (1..33)
			.find_map(|n| {
				let name = format!("wayland-{n}");
				ListeningSocket::bind_absolute(runtime_dir.join(&name))
					.ok()
					.map(|socket| (std::ffi::OsString::from(name), socket))
			})
			.ok_or_else(|| Error::Wayland(format!("no free wayland socket in {}", runtime_dir.display())))?;
		tracing::info!(name = ?socket_name, dir = %runtime_dir.display(), "created wayland socket");
		let handle = event_loop.handle();
		handle
			.insert_source(
				Generic::new(listening_socket, Interest::READ, Mode::Level),
				|_, socket, state: &mut Self| {
					loop {
						match socket.accept() {
							Ok(Some(client_stream)) => {
								if let Err(e) = state
									.display_handle
									.insert_client(client_stream, Arc::new(ClientState::default()))
								{
									tracing::warn!("failed to insert client: {e:?}");
								}
							},
							Ok(None) => break,
							Err(e) => {
								tracing::warn!("accept wayland client: {e}");
								break;
							},
						}
					}
					Ok(PostAction::Continue)
				},
			)
			.map_err(|e| Error::Wayland(format!("insert listener: {e:?}")))?;

		handle
			.insert_source(
				Generic::new(display, Interest::READ, Mode::Level),
				|_, display, state| {
					// SAFETY: the display is owned by the event loop for its lifetime.
					if let Err(e) = unsafe { display.get_mut().dispatch_clients(state) } {
						tracing::warn!("dispatch_clients: {e:?}");
					}
					Ok(PostAction::Continue)
				},
			)
			.map_err(|e| Error::Wayland(format!("insert display: {e:?}")))?;

		Ok(socket_name)
	}

	fn insert_sources(&mut self, event_loop: &mut EventLoop<'static, Self>) -> Result<()> {
		// The scheduler arms its single timer after every dispatch (see
		// `FerriscopeState::service`); releases wake the loop directly.
		if let Some(releases) = self.releases.take() {
			event_loop
				.handle()
				.insert_source(releases, |event, _, state: &mut Self| match event {
					calloop::channel::Event::Msg(release) => state.apply_release(release),
					calloop::channel::Event::Closed => {
						tracing::error!("release channel disconnected; consumer gone, stopping");
						state.request_stop();
					},
				})
				.map_err(|e| Error::Config(format!("insert release channel: {e:?}")))?;
		}

		#[cfg(feature = "test-support")]
		if let Some(control) = self.control.as_ref() {
			let stream = control
				.stream()
				.try_clone()
				.map_err(|e| Error::Input(format!("clone control stream: {e}")))?;
			event_loop
				.handle()
				.insert_source(
					Generic::new(stream, Interest::READ, Mode::Level),
					|_, _, state: &mut Self| {
						state.pump_control_stream();
						Ok(PostAction::Continue)
					},
				)
				.map_err(|e| Error::Input(format!("insert control stream: {e:?}")))?;
		}

		// In-process input arrives over a calloop channel rather than a socket.
		if let Some(input) = self.input.take() {
			event_loop
				.handle()
				.insert_source(input, |event, _, state: &mut Self| match event {
					calloop::channel::Event::Msg(event) => state.apply_input(event),
					calloop::channel::Event::Closed => {},
				})
				.map_err(|e| Error::Input(format!("insert input channel: {e:?}")))?;
		}

		Ok(())
	}

	fn spawn_app(&mut self, argv: &[String], x_display: Option<u32>) -> Result<()> {
		let (program, args) = argv
			.split_first()
			.ok_or_else(|| Error::Config("empty command".into()))?;
		let mut command = Command::new(program);
		// Children always get the display environment explicitly: the library
		// path never mutates the host environment. The gamescope-impersonation
		// block is applied here too, computed from the real
		// socket and X display: the session identity is unconditional, like
		// gamescope, and `--steam` adds only the Steam capability vars.
		command
			.args(args)
			.stdout(Stdio::null())
			.stderr(Stdio::inherit())
			.env("WAYLAND_DISPLAY", &self.socket_name)
			.env("XDG_RUNTIME_DIR", &self.runtime_dir)
			.envs(crate::steam::app_env(
				self.config.steam,
				&self.wayland_display,
				x_display,
				self.hdr,
			));
		if let Some(number) = x_display {
			command.env("DISPLAY", format!(":{number}"));
		}
		let child = command
			.spawn()
			.map_err(|e| Error::Config(format!("spawn {program}: {e}")))?;
		tracing::info!(program, x_display = x_display, "spawned hosted application");
		self.child = Some(child);
		self.watch_child();
		Ok(())
	}

	/// Run `f` once the current dispatch is over, for work that cannot happen
	/// inside a request handler (a destructor event on a just-created object).
	pub(crate) fn defer(&self, f: impl FnOnce() + 'static) {
		self.loop_handle.insert_idle(move |_| f());
	}

	/// Ask the event loop to exit; also used by [`crate::compositor::Compositor::stop`].
	fn request_stop(&mut self) {
		self.loop_signal.stop();
		self.loop_signal.wakeup();
	}

	/// Fire the one-shot readiness handshake, if a consumer is waiting.
	fn signal_ready(&mut self, xdisplay: Option<u32>) {
		if let Some(ready) = self.ready.take() {
			let _ = ready.send(Ready::new(
				self.wayland_display.clone(),
				self.runtime_dir.clone(),
				xdisplay,
				self.hdr,
				self.config.steam,
			));
		}
	}
}

impl Drop for FerriscopeState {
	/// The compositor owns the application it spawned: it does not outlive it.
	fn drop(&mut self) {
		self.terminate_child();
	}
}

impl std::fmt::Debug for FerriscopeState {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("FerriscopeState")
			.field("socket_name", &self.socket_name)
			.finish_non_exhaustive()
	}
}

#[cfg(test)]
mod tests {
	/// The compositor must advertise `zwp_linux_dmabuf_v1` with device feedback
	/// built from the render node's device id. Xwayland enables glamor/DRI3
	/// only when `linux-dmabuf` v4+ (or the legacy `wl_drm`) is offered; without
	/// it it runs software GLX with no DRI3, every GPU client is forced through
	/// llvmpipe, and the session cannot reach its refresh rate.
	#[test]
	fn dmabuf_feedback_is_built_for_the_render_node_device() {
		use smithay::backend::allocator::Format;
		use smithay::wayland::dmabuf::DmabufFeedbackBuilder;

		let Some(render_node) = ["/dev/dri/renderD128", "/dev/dri/renderD129", "/dev/dri/renderD130"]
			.into_iter()
			.find(|path| std::path::Path::new(path).exists())
		else {
			// No GPU in this environment; the same code path is exercised by the
			// compositor launch tests when a render node is present.
			return;
		};

		let dev = {
			use std::os::unix::fs::MetadataExt;
			std::fs::metadata(render_node).expect("stat render node").rdev()
		};
		assert_ne!(dev, 0, "a render node must have a nonzero device id");

		let formats: Vec<Format> = Vec::new();
		let feedback = DmabufFeedbackBuilder::new(dev, formats)
			.build()
			.expect("a render-node device must build a valid dmabuf feedback");

		// A feedback built for a different device must not compare equal: the
		// device is part of the feedback, which is what makes the client
		// allocate on the render node.
		let other = DmabufFeedbackBuilder::new(dev ^ 0xff, Vec::<Format>::new())
			.build()
			.expect("a valid device builds feedback");
		assert_ne!(feedback, other, "feedback must be keyed by its main device");
	}
}
