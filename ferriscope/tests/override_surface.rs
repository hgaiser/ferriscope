//! GPU integration tests for the WSI protocols: `ferriscope_swapchain_v1`
//! (`ferriscope-wsi`) and gamescope's, spoken by a stock gamescope layer.
//!
//! A Vulkan WSI layer presents a game on a `wl_surface` of its own
//! and names the X11 window it replaces (`gamescope_xwayland.override_window_content`
//! or the swapchain's `override_window_content`). From then on that surface
//! *is* the window's content (`steamcompmgr.cpp:8281`, `wlserver.cpp:795`).
//!
//! A mapped X11 window is filled red. A Wayland client then overrides it with a
//! green `wl_surface`. The recorded frame must show green, and red again once
//! the override surface goes away.
//!
//! They are `#[ignore]`d: they need a render node and Xwayland. Run them alone:
//!
//! ```text
//! cargo test -p ferriscope --test override_surface -- --ignored --test-threads=1 --nocapture
//! ```

use std::{
	io::Write,
	os::{
		fd::{AsFd, OwnedFd},
		unix::net::{UnixListener, UnixStream},
	},
	path::{Path, PathBuf},
	process::{Command, Stdio},
	thread,
	time::{Duration, Instant},
};

use ferriscope::{Compositor, CompositorConfig, CompositorHandles, Ready};
use smithay::reexports::x11rb::{
	self, COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, connection::Connection, protocol::xproto::*,
	rust_connection::RustConnection,
};
use wayland_client::{
	Connection as WlConnection, Dispatch, QueueHandle, delegate_noop,
	globals::{GlobalListContents, registry_queue_init},
	protocol::{wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface},
};

/// Protocol bindings for the client side of the WSI protocols.
mod protocols {
	#![allow(
		dead_code,
		non_camel_case_types,
		unused_unsafe,
		unused_variables,
		non_upper_case_globals,
		non_snake_case,
		unused_imports,
		missing_docs
	)]
	#![allow(clippy::all)]

	pub mod xwayland {
		use wayland_client::{self, protocol::*};

		pub mod __interfaces {
			use wayland_client::protocol::__interfaces::*;
			wayland_scanner::generate_interfaces!("protocol/gamescope-xwayland.xml");
		}
		use self::__interfaces::*;

		wayland_scanner::generate_client_code!("protocol/gamescope-xwayland.xml");
	}

	pub mod swapchain {
		use wayland_client::{self, protocol::*};

		pub mod __interfaces {
			use wayland_client::protocol::__interfaces::*;
			wayland_scanner::generate_interfaces!("protocol/gamescope-swapchain.xml");
		}
		use self::__interfaces::*;

		wayland_scanner::generate_client_code!("protocol/gamescope-swapchain.xml");
	}

	pub mod ferriscope {
		use wayland_client::{self, protocol::*};

		pub mod __interfaces {
			use wayland_client::protocol::__interfaces::*;
			wayland_scanner::generate_interfaces!("protocol/ferriscope-swapchain-v1.xml");
		}
		use self::__interfaces::*;

		wayland_scanner::generate_client_code!("protocol/ferriscope-swapchain-v1.xml");
	}
}

use protocols::{
	ferriscope::{ferriscope_swapchain_factory_v1, ferriscope_swapchain_v1},
	swapchain::{gamescope_swapchain, gamescope_swapchain_factory_v2},
	xwayland::gamescope_xwayland,
};

/// Output size.
const WIDTH: usize = 320;
const HEIGHT: usize = 240;
/// The X window and the override surface are both this size, so each is
/// fill-scaled to the whole output.
const CONTENT: (i32, i32) = (160, 120);
/// The X window's background (0xRRGGBB) and the override surface's pixel
/// (AARRGGBB).
const X11_COLOR: u32 = 0x00ff_0000;
const OVERRIDE_COLOR: u32 = 0xff00_ff00;
/// Budget for the compositor handshake.
const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// Budget for a frame with the expected content to appear.
const FRAME_TIMEOUT: Duration = Duration::from_secs(30);
/// Poll slice.
const POLL_SLICE: Duration = Duration::from_millis(50);
/// Stop writing after this many frames; bounds disk use on tmpfs.
const RECORD_LIMIT: u64 = 1500;

/// A decoded PPM frame.
struct Ppm {
	width: usize,
	height: usize,
	rgb: Vec<u8>,
}

fn parse_ppm(bytes: &[u8]) -> Option<Ppm> {
	if !bytes.starts_with(b"P6\n") {
		return None;
	}
	let dims_end = bytes[3..].iter().position(|&b| b == b'\n')? + 3;
	let dims = std::str::from_utf8(&bytes[3..dims_end]).ok()?;
	let mut tokens = dims.split_whitespace();
	let width: usize = tokens.next()?.parse().ok()?;
	let height: usize = tokens.next()?.parse().ok()?;
	let maxval_end = bytes[dims_end + 1..].iter().position(|&b| b == b'\n')? + dims_end + 1;
	let rgb = &bytes[maxval_end + 1..];
	(rgb.len() >= width * height * 3).then(|| Ppm {
		width,
		height,
		rgb: rgb.to_vec(),
	})
}

/// The newest recorded frame, or `None` while there is none.
fn latest_frame(dir: &Path) -> Option<Ppm> {
	let mut latest: Option<u64> = None;
	for entry in std::fs::read_dir(dir).ok()?.flatten() {
		let name = entry.file_name();
		let digits = name
			.to_string_lossy()
			.strip_prefix("frame_")
			.and_then(|rest| rest.strip_suffix(".ppm").map(str::to_owned));
		if let Some(sequence) = digits.and_then(|digits| digits.parse::<u64>().ok()) {
			latest = Some(latest.map_or(sequence, |current| current.max(sequence)));
		}
	}
	parse_ppm(&std::fs::read(dir.join(format!("frame_{:06}.ppm", latest?))).ok()?)
}

/// Whether every pixel of the frame is within a small tolerance of `rgb`: the
/// content fills the whole output.
fn is_solid(frame: &Ppm, rgb: [u8; 3]) -> bool {
	frame.width == WIDTH
		&& frame.height == HEIGHT
		&& frame
			.rgb
			.chunks_exact(3)
			.all(|pixel| pixel.iter().zip(rgb).all(|(got, want)| got.abs_diff(want) <= 8))
}

/// Wait, bounded, for a newest frame that is solid `rgb`.
fn wait_for_solid(dir: &Path, rgb: [u8; 3], timeout: Duration) -> bool {
	let deadline = Instant::now() + timeout;
	loop {
		if latest_frame(dir).is_some_and(|frame| is_solid(&frame, rgb)) {
			return true;
		}
		if Instant::now() >= deadline {
			return false;
		}
		thread::sleep(POLL_SLICE);
	}
}

fn unique_dir(tag: &str) -> PathBuf {
	let nanos = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_nanos())
		.unwrap_or(0);
	std::env::temp_dir().join(format!("ferriscope-wsi-{tag}-{}-{nanos}", std::process::id()))
}

/// The compositor with Xwayland, a recorder, and the X window to override.
struct Session {
	compositor: Option<Compositor>,
	_handles: CompositorHandles,
	ready: Ready,
	record_dir: PathBuf,
	x11: RustConnection,
	window: Window,
	/// The input control stream, for [`Session::start_for_input`].
	control: Option<(UnixStream, PathBuf)>,
}

impl Session {
	fn start(tag: &str) -> Self {
		Self::start_with(tag, false)
	}

	/// A session whose X window draws nothing, so Xwayland never commits a
	/// buffer for it (a Vulkan game presenting through the WSI layer), with an
	/// input control stream and the window listening for button presses.
	fn start_for_input(tag: &str) -> Self {
		Self::start_with(tag, true)
	}

	fn start_with(tag: &str, input: bool) -> Self {
		let record_dir = unique_dir(tag);
		let control_path = input.then(|| unique_dir(&format!("{tag}-control")));
		let listener = control_path
			.as_ref()
			.map(|path| UnixListener::bind(path).expect("bind control listener"));
		let config = CompositorConfig {
			width: WIDTH as u32,
			height: HEIGHT as u32,
			xwayland: true,
			output: ferriscope::FrameOutput::Recorder(ferriscope::RecorderOutput {
				dir: Some(record_dir.clone()),
				max_frames: Some(RECORD_LIMIT),
				..ferriscope::RecorderOutput::default()
			}),
			control_socket: control_path.clone(),
			..CompositorConfig::default()
		};
		let (compositor, handles) = Compositor::launch(config).expect("launch compositor");
		let ready = handles.ready.recv_timeout(READY_TIMEOUT).expect("ready handshake");
		let control = listener.zip(control_path).map(|(listener, path)| {
			let (stream, _) = listener.accept().expect("accept the control connection");
			(stream, path)
		});
		let display = ready.xdisplay.expect("Xwayland display");
		let (x11, _) = x11rb::connect(Some(&format!(":{display}"))).expect("connect to Xwayland");
		let window = if input {
			create_input_window(&x11)
		} else {
			create_window(&x11)
		};
		Self {
			compositor: Some(compositor),
			_handles: handles,
			ready,
			record_dir,
			x11,
			window,
			control,
		}
	}

	/// Send one control-stream line, e.g. `motion-abs 10 10`.
	fn input(&mut self, line: &str) {
		let (stream, _) = self.control.as_mut().expect("a session started for input");
		stream
			.write_all(format!("{line}\n").as_bytes())
			.expect("write control line");
	}

	/// The Xwayland server id a layer reads from the X11 root window
	/// (`steam::display_id_to_server_id`).
	fn server_id(&self) -> u32 {
		let display = self.ready.xdisplay.expect("Xwayland display");
		((display + 100) << 8) | display
	}

	/// Connect a Wayland client to the compositor.
	fn wayland_client(&self) -> Client {
		let runtime_dir = std::env::var("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR is set");
		let stream = UnixStream::connect(Path::new(&runtime_dir).join(&self.ready.wayland_display))
			.expect("connect to the compositor socket");
		Client::new(WlConnection::from_socket(stream).expect("wayland connection"))
	}
}

impl Drop for Session {
	fn drop(&mut self) {
		if let Some(compositor) = self.compositor.take() {
			let _ = compositor.stop();
		}
		// Only this process's own Xwayland.
		let _ = Command::new("pkill")
			.args(["-9", "-x", "-P", &std::process::id().to_string(), "Xwayland"])
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.status();
		let _ = std::fs::remove_dir_all(&self.record_dir);
		if let Some((_, path)) = &self.control {
			let _ = std::fs::remove_file(path);
		}
	}
}

/// A mapped, managed X11 window with a red background.
fn create_window(conn: &RustConnection) -> Window {
	let root = conn.setup().roots[0].root;
	let window = conn.generate_id().expect("window id");
	conn.create_window(
		COPY_DEPTH_FROM_PARENT,
		window,
		root,
		0,
		0,
		CONTENT.0 as u16,
		CONTENT.1 as u16,
		0,
		WindowClass::INPUT_OUTPUT,
		COPY_FROM_PARENT,
		&CreateWindowAux::new()
			.background_pixel(X11_COLOR)
			.event_mask(EventMask::STRUCTURE_NOTIFY),
	)
	.expect("create window");
	conn.map_window(window).expect("map window");
	conn.flush().expect("flush");
	window
}

/// A mapped, managed X11 window with no background that listens for button
/// presses: nothing ever draws into it.
fn create_input_window(conn: &RustConnection) -> Window {
	let root = conn.setup().roots[0].root;
	let window = conn.generate_id().expect("window id");
	conn.create_window(
		COPY_DEPTH_FROM_PARENT,
		window,
		root,
		0,
		0,
		CONTENT.0 as u16,
		CONTENT.1 as u16,
		0,
		WindowClass::INPUT_OUTPUT,
		COPY_FROM_PARENT,
		&CreateWindowAux::new().event_mask(EventMask::STRUCTURE_NOTIFY | EventMask::BUTTON_PRESS),
	)
	.expect("create window");
	conn.map_window(window).expect("map window");
	conn.flush().expect("flush");
	window
}

/// What the client observed on its swapchain and factory objects.
#[derive(Default)]
struct Observed {
	refresh_cycle_ns: Option<u64>,
	retired: bool,
	hdr_available: Option<bool>,
	xwayland_server: Option<u32>,
	frame_ready: u32,
}

struct Client {
	conn: WlConnection,
	queue: wayland_client::EventQueue<ClientState>,
	state: ClientState,
}

struct ClientState {
	compositor: wl_compositor::WlCompositor,
	shm: wl_shm::WlShm,
	xwayland: gamescope_xwayland::GamescopeXwayland,
	factory: gamescope_swapchain_factory_v2::GamescopeSwapchainFactoryV2,
	ferriscope_factory: ferriscope_swapchain_factory_v1::FerriscopeSwapchainFactoryV1,
	observed: Observed,
}

impl Client {
	fn new(conn: WlConnection) -> Self {
		let (globals, mut queue) = registry_queue_init::<ClientState>(&conn).expect("registry");
		let qh = queue.handle();
		let mut state = ClientState {
			compositor: globals.bind(&qh, 1..=4, ()).expect("wl_compositor"),
			shm: globals.bind(&qh, 1..=1, ()).expect("wl_shm"),
			xwayland: globals.bind(&qh, 1..=1, ()).expect("gamescope_xwayland"),
			factory: globals.bind(&qh, 1..=1, ()).expect("gamescope_swapchain_factory_v2"),
			ferriscope_factory: globals.bind(&qh, 1..=1, ()).expect("ferriscope_swapchain_factory_v1"),
			observed: Observed::default(),
		};
		queue.roundtrip(&mut state).expect("roundtrip");
		Self { conn, queue, state }
	}

	fn qh(&self) -> QueueHandle<ClientState> {
		self.queue.handle()
	}

	fn roundtrip(&mut self) {
		self.queue.roundtrip(&mut self.state).expect("roundtrip");
	}

	/// A `wl_surface` with a solid `color` buffer attached and committed.
	fn solid_surface(&mut self, color: u32) -> wl_surface::WlSurface {
		let qh = self.qh();
		let (width, height) = CONTENT;
		let stride = width * 4;
		let size = (stride * height) as usize;
		let fd: OwnedFd = rustix_memfd(size);
		{
			let mut map = unsafe { memmap(&fd, size) };
			for pixel in map.chunks_exact_mut(4) {
				pixel.copy_from_slice(&color.to_le_bytes());
			}
		}
		let pool = self.state.shm.create_pool(fd.as_fd(), size as i32, &qh, ());
		let buffer = pool.create_buffer(0, width, height, stride, wl_shm::Format::Argb8888, &qh, ());
		let surface = self.state.compositor.create_surface(&qh, ());
		surface.attach(Some(&buffer), 0, 0);
		surface.damage_buffer(0, 0, width, height);
		surface.commit();
		// The pool and buffer live as long as the connection; the compositor has
		// the fd by the time the roundtrip returns.
		std::mem::forget((pool, buffer, fd));
		surface
	}
}

/// An anonymous shared-memory file of `size` bytes.
fn rustix_memfd(size: usize) -> OwnedFd {
	use smithay::reexports::rustix::fs::{MemfdFlags, ftruncate, memfd_create};
	let fd = memfd_create("ferriscope-wsi-test", MemfdFlags::CLOEXEC).expect("memfd");
	ftruncate(&fd, size as u64).expect("ftruncate");
	fd
}

/// Map `fd` read-write and return it as a mutable byte slice guard.
///
/// # Safety
///
/// `fd` must be a file of at least `size` bytes.
unsafe fn memmap(fd: &OwnedFd, size: usize) -> MemMap {
	use smithay::reexports::rustix::mm::{MapFlags, ProtFlags, mmap};
	let ptr = unsafe {
		mmap(
			std::ptr::null_mut(),
			size,
			ProtFlags::READ | ProtFlags::WRITE,
			MapFlags::SHARED,
			fd,
			0,
		)
	}
	.expect("mmap");
	MemMap { ptr: ptr.cast(), size }
}

struct MemMap {
	ptr: *mut u8,
	size: usize,
}

impl std::ops::Deref for MemMap {
	type Target = [u8];
	fn deref(&self) -> &[u8] {
		unsafe { std::slice::from_raw_parts(self.ptr, self.size) }
	}
}

impl std::ops::DerefMut for MemMap {
	fn deref_mut(&mut self) -> &mut [u8] {
		unsafe { std::slice::from_raw_parts_mut(self.ptr, self.size) }
	}
}

impl Drop for MemMap {
	fn drop(&mut self) {
		unsafe {
			let _ = smithay::reexports::rustix::mm::munmap(self.ptr.cast(), self.size);
		}
	}
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ClientState {
	fn event(
		_: &mut Self,
		_: &wl_registry::WlRegistry,
		_: wl_registry::Event,
		_: &GlobalListContents,
		_: &WlConnection,
		_: &QueueHandle<Self>,
	) {
	}
}

impl Dispatch<gamescope_swapchain::GamescopeSwapchain, ()> for ClientState {
	fn event(
		state: &mut Self,
		_: &gamescope_swapchain::GamescopeSwapchain,
		event: gamescope_swapchain::Event,
		_: &(),
		_: &WlConnection,
		_: &QueueHandle<Self>,
	) {
		match event {
			gamescope_swapchain::Event::RefreshCycle {
				refresh_cycle_hi,
				refresh_cycle_lo,
			} => {
				state.observed.refresh_cycle_ns =
					Some((u64::from(refresh_cycle_hi) << 32) | u64::from(refresh_cycle_lo));
			},
			gamescope_swapchain::Event::Retired => state.observed.retired = true,
			_ => {},
		}
	}
}

impl Dispatch<ferriscope_swapchain_v1::FerriscopeSwapchainV1, ()> for ClientState {
	fn event(
		state: &mut Self,
		_: &ferriscope_swapchain_v1::FerriscopeSwapchainV1,
		event: ferriscope_swapchain_v1::Event,
		_: &(),
		_: &WlConnection,
		_: &QueueHandle<Self>,
	) {
		match event {
			ferriscope_swapchain_v1::Event::RefreshCycle {
				refresh_cycle_hi,
				refresh_cycle_lo,
			} => {
				state.observed.refresh_cycle_ns =
					Some((u64::from(refresh_cycle_hi) << 32) | u64::from(refresh_cycle_lo));
			},
			ferriscope_swapchain_v1::Event::Retired => state.observed.retired = true,
			ferriscope_swapchain_v1::Event::FrameReady => state.observed.frame_ready += 1,
			_ => {},
		}
	}
}

impl Dispatch<ferriscope_swapchain_factory_v1::FerriscopeSwapchainFactoryV1, ()> for ClientState {
	fn event(
		state: &mut Self,
		_: &ferriscope_swapchain_factory_v1::FerriscopeSwapchainFactoryV1,
		event: ferriscope_swapchain_factory_v1::Event,
		_: &(),
		_: &WlConnection,
		_: &QueueHandle<Self>,
	) {
		match event {
			ferriscope_swapchain_factory_v1::Event::HdrAvailable { available } => {
				state.observed.hdr_available = Some(available != 0);
			},
			ferriscope_swapchain_factory_v1::Event::XwaylandServer { server_id } => {
				state.observed.xwayland_server = Some(server_id);
			},
		}
	}
}

delegate_noop!(ClientState: ignore wl_compositor::WlCompositor);
delegate_noop!(ClientState: ignore wl_shm::WlShm);
delegate_noop!(ClientState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(ClientState: ignore wl_buffer::WlBuffer);
delegate_noop!(ClientState: ignore wl_surface::WlSurface);
delegate_noop!(ClientState: ignore gamescope_xwayland::GamescopeXwayland);
delegate_noop!(ClientState: ignore gamescope_swapchain_factory_v2::GamescopeSwapchainFactoryV2);

/// The red X11 window is composited before any override exists.
fn assert_x11_content(session: &Session) {
	assert!(
		wait_for_solid(&session.record_dir, [0xff, 0, 0], FRAME_TIMEOUT),
		"the X11 window's own content never filled the output; frames in {}",
		session.record_dir.display()
	);
}

#[test]
#[ignore = "requires GPU and Xwayland"]
fn gamescope_xwayland_override_surface_replaces_the_window_content() {
	let session = Session::start("xwayland");
	assert_x11_content(&session);

	let mut client = session.wayland_client();
	let surface = client.solid_surface(OVERRIDE_COLOR);
	client.state.xwayland.override_window_content(&surface, session.window);
	client.roundtrip();

	assert!(
		wait_for_solid(&session.record_dir, [0, 0xff, 0], FRAME_TIMEOUT),
		"the override surface did not become the window's content; frames in {}",
		session.record_dir.display()
	);

	// The override surface going away hands the window back to its own surface.
	surface.destroy();
	client.roundtrip();
	assert!(
		wait_for_solid(&session.record_dir, [0xff, 0, 0], FRAME_TIMEOUT),
		"the window did not return to its own content after the override surface was destroyed"
	);
	drop(client.conn);
}

#[test]
#[ignore = "requires GPU and Xwayland"]
fn swapchain_override_replaces_the_content_and_reports_back() {
	let session = Session::start("swapchain");
	assert_x11_content(&session);

	let mut client = session.wayland_client();
	let qh = client.qh();
	let surface = client.solid_surface(OVERRIDE_COLOR);
	let swapchain = client.state.factory.create_swapchain(&surface, &qh, ());
	swapchain.swapchain_feedback(3, 37, 0, 1, 1, 1, "test-engine".to_string());
	swapchain.set_present_mode(2);
	swapchain.override_window_content(session.server_id(), session.window);
	client.roundtrip();

	assert!(
		wait_for_solid(&session.record_dir, [0, 0xff, 0], FRAME_TIMEOUT),
		"the swapchain's surface did not become the window's content; frames in {}",
		session.record_dir.display()
	);
	assert_eq!(
		client.state.observed.refresh_cycle_ns,
		Some(16_666_666),
		"swapchain_feedback was not answered with the 60 Hz refresh cycle"
	);

	// A second swapchain taking the window over retires the first.
	let second_surface = client.solid_surface(OVERRIDE_COLOR);
	let second = client.state.factory.create_swapchain(&second_surface, &qh, ());
	second.override_window_content(session.server_id(), session.window);
	client.roundtrip();
	client.roundtrip();
	assert!(
		client.state.observed.retired,
		"replacing the override did not retire the first swapchain"
	);
	drop(client.conn);
}

#[test]
#[ignore = "requires GPU and Xwayland"]
fn ferriscope_swapchain_override_replaces_the_content_and_reports_back() {
	let session = Session::start("ferriscope");
	assert_x11_content(&session);

	let mut client = session.wayland_client();
	let qh = client.qh();
	// The factory announces its capabilities as soon as it is bound.
	assert_eq!(client.state.observed.hdr_available, Some(false));
	assert_eq!(client.state.observed.xwayland_server, Some(session.server_id()));

	let surface = client.solid_surface(OVERRIDE_COLOR);
	let swapchain = client.state.ferriscope_factory.create_swapchain(&surface, &qh, ());
	swapchain.swapchain_feedback(3, 37, 0, 1, 1, 1, "test-engine".to_string());
	swapchain.set_present_mode(2);
	// Another compositor's Xwayland is refused.
	swapchain.override_window_content(session.server_id() + 1, session.window);
	client.roundtrip();
	assert!(
		!wait_for_solid(&session.record_dir, [0, 0xff, 0], Duration::from_millis(500)),
		"an override naming another Xwayland server took effect"
	);
	swapchain.override_window_content(session.server_id(), session.window);
	client.roundtrip();

	assert!(
		wait_for_solid(&session.record_dir, [0, 0xff, 0], FRAME_TIMEOUT),
		"the swapchain's surface did not become the window's content; frames in {}",
		session.record_dir.display()
	);
	assert_eq!(
		client.state.observed.refresh_cycle_ns,
		Some(16_666_666),
		"swapchain_feedback was not answered with the 60 Hz refresh cycle"
	);

	// A second swapchain taking the window over retires the first.
	let second_surface = client.solid_surface(OVERRIDE_COLOR);
	let second = client
		.state
		.ferriscope_factory
		.create_swapchain(&second_surface, &qh, ());
	second.override_window_content(session.server_id(), session.window);
	client.roundtrip();
	client.roundtrip();
	assert!(
		client.state.observed.retired,
		"replacing the override did not retire the first swapchain"
	);
	drop(client.conn);
}

/// Regression: a Vulkan game presenting through the WSI layer draws nothing
/// into its X window, so Xwayland's surface for it never gets a buffer and has
/// no input region. Hit-testing that surface found nothing under the pointer,
/// and clicks never reached the game. Input belongs to the X window, as in
/// gamescope, whatever surface shows its content.
#[test]
#[ignore = "requires GPU and Xwayland"]
fn a_click_on_an_overridden_window_reaches_the_x_client() {
	let mut session = Session::start_for_input("input");
	let mut client = session.wayland_client();
	let surface = client.solid_surface(OVERRIDE_COLOR);
	client.state.xwayland.override_window_content(&surface, session.window);
	// A swapchain keeps presenting: the window has no other content, so this
	// commit is what makes it presentable.
	surface.damage_buffer(0, 0, CONTENT.0, CONTENT.1);
	surface.commit();
	client.roundtrip();
	assert!(
		wait_for_solid(&session.record_dir, [0, 0xff, 0], FRAME_TIMEOUT),
		"the override surface did not become the window's content; frames in {}",
		session.record_dir.display()
	);

	session.input(&format!("motion-abs {} {}", WIDTH / 2, HEIGHT / 2));
	session.input("button 272 1");
	session.input("button 272 0");

	let deadline = Instant::now() + FRAME_TIMEOUT;
	let mut pressed = false;
	while !pressed && Instant::now() < deadline {
		match session.x11.poll_for_event().expect("poll X events") {
			Some(x11rb::protocol::Event::ButtonPress(event)) => pressed = event.event == session.window,
			Some(_) => {},
			None => thread::sleep(POLL_SLICE),
		}
	}
	assert!(pressed, "the click did not reach the overridden X window");
	drop(client.conn);
}
