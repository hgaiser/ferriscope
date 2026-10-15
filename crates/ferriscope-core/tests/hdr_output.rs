//! GPU integration tests for HDR output.
//!
//! A Wayland client declares what its surface holds (`wp_color_management_v1`,
//! `frog_color_management_v1` or the WSI layer's `gamescope_swapchain`) and
//! presents a buffer of known values. The tests check what the compositor
//! delivers: the pool format, the `FrameInfo::colour` metadata and, by reading
//! the DMA-BUF back through a renderer of the test's own, the pixel values.
//!
//! They are `#[ignore]`d: they need a render node. Run them alone:
//!
//! ```text
//! cargo test -p ferriscope-core --test hdr_output -- --ignored --test-threads=1 --nocapture
//! ```

use std::{
	os::{
		fd::{AsFd, OwnedFd},
		unix::net::UnixStream,
	},
	path::Path,
	sync::Arc,
	time::{Duration, Instant},
};

use ferriscope_core::{
	ColourMetadata, ColourPrimaries, Compositor, CompositorConfig, CompositorHandles, DmaBufFrame, Fourcc, Frame,
	FrameData, InputEvent, Ready, RecvTimeoutError, TransferFunction,
};
use smithay::{
	backend::{
		allocator::{
			Allocator, Buffer, Fourcc as SmithayFourcc, Modifier,
			dmabuf::{AsDmabuf, Dmabuf},
			gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
		},
		egl::{EGLContext, EGLDisplay},
		renderer::{
			Bind, Color32F, Frame as _, ImportDma, Renderer,
			gles::{GlesRenderer, ffi},
		},
	},
	reexports::rustix::{
		event::{PollFd, PollFlags, Timespec, poll},
		mm::{MapFlags, ProtFlags, mmap, munmap},
	},
	utils::{Physical, Rectangle, Size, Transform},
};
use wayland_client::{
	Connection, Dispatch, QueueHandle, delegate_noop,
	globals::{GlobalList, GlobalListContents, registry_queue_init},
	protocol::{wl_buffer, wl_compositor, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface},
};
use wayland_protocols::{
	wp::color_management::v1::client::{
		wp_color_management_surface_v1::WpColorManagementSurfaceV1,
		wp_color_manager_v1::{Primaries, RenderIntent, TransferFunction as WpTransferFunction, WpColorManagerV1},
		wp_image_description_creator_params_v1::WpImageDescriptionCreatorParamsV1,
		wp_image_description_v1::WpImageDescriptionV1,
	},
	wp::linux_dmabuf::zv1::client::{zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1},
	xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base},
};

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

	pub mod frog {
		use wayland_client::{self, protocol::*};

		pub mod __interfaces {
			use wayland_client::protocol::__interfaces::*;
			wayland_scanner::generate_interfaces!("protocol/frog-color-management-v1.xml");
		}
		use self::__interfaces::*;

		wayland_scanner::generate_client_code!("protocol/frog-color-management-v1.xml");
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
}

use protocols::{
	frog::{frog_color_managed_surface, frog_color_management_factory_v1},
	swapchain::{gamescope_swapchain, gamescope_swapchain_factory_v2},
};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;
const READY_TIMEOUT: Duration = Duration::from_secs(30);
const FRAME_TIMEOUT: Duration = Duration::from_secs(30);
const RENDER_NODE: &str = "/dev/dri/renderD128";

/// `VK_COLOR_SPACE_HDR10_ST2084_EXT`.
const VK_HDR10: u32 = 1_000_104_008;

/// What a client declares about its surface.
#[derive(Clone, Copy, PartialEq)]
enum Declare {
	/// Nothing: the surface is SDR.
	Nothing,
	/// `wp_color_management_v1`: HDR10 with mastering 1000 cd/m2, CLL 900 and
	/// FALL 300.
	WpPq,
	/// `wp_color_management_v1`: scRGB.
	WpScRgb,
	/// `frog_color_management_v1`: HDR10.
	FrogPq,
	/// The WSI layer: an HDR10 swapchain with `set_hdr_metadata` (CLL 700), on
	/// top of a `wp` description saying CLL 900.
	SwapchainPq,
}

struct Session {
	compositor: Option<Compositor>,
	handles: CompositorHandles,
	ready: Ready,
}

impl Session {
	fn start(hdr: bool) -> Self {
		let _ = tracing_subscriber::fmt()
			.with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
			.with_test_writer()
			.try_init();
		let config = CompositorConfig {
			width: WIDTH,
			height: HEIGHT,
			hdr,
			..CompositorConfig::default()
		};
		let (compositor, handles) = Compositor::launch(config).expect("launch compositor");
		let ready = handles.ready.recv_timeout(READY_TIMEOUT).expect("ready handshake");
		Self {
			compositor: Some(compositor),
			handles,
			ready,
		}
	}

	fn client(&self) -> Client {
		let runtime_dir = std::env::var("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR is set");
		let stream = UnixStream::connect(Path::new(&runtime_dir).join(&self.ready.wayland_display))
			.expect("connect to the compositor socket");
		Client::new(Connection::from_socket(stream).expect("wayland connection"))
	}

	/// The first DMA-BUF frame satisfying `accept`.
	fn frame_where(&self, accept: impl Fn(&DmaBufFrame) -> bool) -> (ColourMetadata, DmaBufFrame) {
		let deadline = Instant::now() + FRAME_TIMEOUT;
		loop {
			let remaining = deadline.saturating_duration_since(Instant::now());
			assert!(!remaining.is_zero(), "no matching frame within {FRAME_TIMEOUT:?}");
			match self.handles.frames.recv_timeout(remaining) {
				Ok(Frame {
					info,
					data: FrameData::DmaBuf(dmabuf),
				}) if accept(&dmabuf) => return (info.colour, dmabuf),
				Ok(_) => {},
				Err(RecvTimeoutError::Timeout) => panic!("no matching frame within {FRAME_TIMEOUT:?}"),
				Err(RecvTimeoutError::Disconnected) => panic!("frame mailbox disconnected"),
			}
		}
	}
}

impl Drop for Session {
	fn drop(&mut self) {
		if let Some(compositor) = self.compositor.take() {
			let _ = compositor.stop();
		}
	}
}

/// Reads delivered DMA-BUFs back through a renderer of the test's own.
struct Reader {
	renderer: GlesRenderer,
}

impl Reader {
	fn new() -> Self {
		let file = std::fs::OpenOptions::new()
			.read(true)
			.write(true)
			.open(RENDER_NODE)
			.expect("open the render node");
		let gbm = GbmDevice::new(Arc::new(file)).expect("gbm device");
		let display = unsafe { EGLDisplay::new(gbm) }.expect("egl display");
		let context = EGLContext::new(&display).expect("egl context");
		let renderer = unsafe { GlesRenderer::new(context) }.expect("gles");
		Self { renderer }
	}

	/// The RGBA floats of `frame`, once its render has finished.
	fn read(&mut self, frame: &DmaBufFrame) -> Vec<f32> {
		if let Some(fence) = &frame.acquire_fence {
			let deadline = Instant::now() + Duration::from_secs(5);
			loop {
				let mut fds = [PollFd::new(fence, PollFlags::IN)];
				let slice = Timespec {
					tv_sec: 0,
					tv_nsec: 20_000_000,
				};
				poll(&mut fds, Some(&slice)).expect("poll acquire fence");
				if fds[0].revents().contains(PollFlags::IN) {
					break;
				}
				assert!(Instant::now() < deadline, "the acquire fence never signalled");
			}
		}
		let texture = self
			.renderer
			.import_dmabuf(&frame.dmabuf, None)
			.expect("import the delivered dmabuf");
		let id = texture.tex_id();
		let fourcc = frame.dmabuf.format().code;
		let mut floats = vec![0f32; (WIDTH * HEIGHT * 4) as usize];
		self.renderer
			.with_context(|gl| unsafe {
				let mut fbo = 0;
				gl.GenFramebuffers(1, &mut fbo);
				gl.BindFramebuffer(ffi::FRAMEBUFFER, fbo);
				gl.FramebufferTexture2D(ffi::FRAMEBUFFER, ffi::COLOR_ATTACHMENT0, ffi::TEXTURE_2D, id, 0);
				assert_eq!(
					gl.CheckFramebufferStatus(ffi::FRAMEBUFFER),
					ffi::FRAMEBUFFER_COMPLETE,
					"the delivered dmabuf cannot be a framebuffer"
				);
				match fourcc {
					Fourcc::Abgr16161616f => gl.ReadPixels(
						0,
						0,
						WIDTH as i32,
						HEIGHT as i32,
						ffi::RGBA,
						ffi::FLOAT,
						floats.as_mut_ptr().cast(),
					),
					Fourcc::Abgr2101010 => {
						let mut words = vec![0u32; (WIDTH * HEIGHT) as usize];
						gl.ReadPixels(
							0,
							0,
							WIDTH as i32,
							HEIGHT as i32,
							ffi::RGBA,
							ffi::UNSIGNED_INT_2_10_10_10_REV,
							words.as_mut_ptr().cast(),
						);
						for (word, out) in words.iter().zip(floats.chunks_exact_mut(4)) {
							out.copy_from_slice(&[
								(word & 0x3ff) as f32 / 1023.0,
								((word >> 10) & 0x3ff) as f32 / 1023.0,
								((word >> 20) & 0x3ff) as f32 / 1023.0,
								(word >> 30) as f32 / 3.0,
							]);
						}
					},
					_ => {
						let mut bytes = vec![0u8; (WIDTH * HEIGHT * 4) as usize];
						gl.ReadPixels(
							0,
							0,
							WIDTH as i32,
							HEIGHT as i32,
							ffi::RGBA,
							ffi::UNSIGNED_BYTE,
							bytes.as_mut_ptr().cast(),
						);
						for (byte, out) in bytes.iter().zip(floats.iter_mut()) {
							*out = f32::from(*byte) / 255.0;
						}
					},
				}
				assert_eq!(gl.GetError(), ffi::NO_ERROR, "glReadPixels failed");
				gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
				gl.DeleteFramebuffers(1, &fbo);
			})
			.expect("gl context");
		floats
	}
}

/// The RGBA floats at `(x, y)` of a readback.
fn pixel(floats: &[f32], x: usize, y: usize) -> [f32; 4] {
	let start = (y * WIDTH as usize + x) * 4;
	floats[start..start + 4].try_into().unwrap()
}

/// Whether `got` is within `tolerance` of `want` in every channel.
fn close(got: [f32; 4], want: [f32; 4], tolerance: f32) -> bool {
	got.iter().zip(want).all(|(got, want)| (got - want).abs() <= tolerance)
}

/// The 10-bit PQ code values `(r, g, b)` as the floats a readback returns.
fn unorm10(r: u32, g: u32, b: u32) -> [f32; 4] {
	[r as f32 / 1023.0, g as f32 / 1023.0, b as f32 / 1023.0, 1.0]
}

/// A packed `Abgr2101010` word: 10 bits each of red, green and blue, 2 of alpha.
fn abgr2101010(r: u32, g: u32, b: u32, a: u32) -> u32 {
	(a << 30) | (b << 20) | (g << 10) | r
}

/// IEEE half-precision encoding of values that are exactly representable.
fn half(value: f32) -> u16 {
	let bits = value.to_bits();
	let sign = ((bits >> 16) & 0x8000) as u16;
	if value == 0.0 {
		return sign;
	}
	let exponent = ((bits >> 23) & 0xff) as i32 - 127 + 15;
	assert!((1..31).contains(&exponent), "{value} is not a normal half");
	assert_eq!(bits & 0x1fff, 0, "{value} is not exactly representable as a half");
	sign | ((exponent as u16) << 10) | ((bits >> 13) & 0x3ff) as u16
}

struct Client {
	_conn: Connection,
	queue: wayland_client::EventQueue<State>,
	state: State,
	globals: GlobalList,
	surface: wl_surface::WlSurface,
	colour_surface: Option<WpColorManagementSurfaceV1>,
	frog: Option<frog_color_managed_surface::FrogColorManagedSurface>,
	swapchain: Option<gamescope_swapchain::GamescopeSwapchain>,
	keep: Vec<(wl_shm_pool::WlShmPool, wl_buffer::WlBuffer)>,
	dmabufs: Vec<wl_buffer::WlBuffer>,
	cursor_surfaces: Vec<wl_surface::WlSurface>,
}

#[derive(Default)]
struct State {
	configured: bool,
	/// Serial of the last `wl_pointer.enter`, which `set_cursor` needs.
	enter_serial: Option<u32>,
}

impl Client {
	fn new(conn: Connection) -> Self {
		let (globals, mut queue) = registry_queue_init::<State>(&conn).expect("registry");
		let qh = queue.handle();
		let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 1..=4, ()).expect("wl_compositor");
		let wm_base: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=1, ()).expect("xdg_wm_base");
		let _seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ()).expect("wl_seat");
		let surface = compositor.create_surface(&qh, ());
		let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
		let toplevel = xdg_surface.get_toplevel(&qh, ());
		toplevel.set_fullscreen(None);
		surface.commit();
		let mut state = State::default();
		queue.roundtrip(&mut state).expect("roundtrip");
		assert!(state.configured, "the toplevel was never configured");
		Self {
			_conn: conn,
			queue,
			state,
			globals,
			surface,
			colour_surface: None,
			frog: None,
			swapchain: None,
			keep: Vec::new(),
			dmabufs: Vec::new(),
			cursor_surfaces: Vec::new(),
		}
	}

	fn qh(&self) -> QueueHandle<State> {
		self.queue.handle()
	}

	fn roundtrip(&mut self) {
		self.queue.roundtrip(&mut self.state).expect("roundtrip");
	}

	/// Declare `declare` for the surface; takes effect with the next commit.
	fn declare(&mut self, declare: Declare) {
		let qh = self.qh();
		if matches!(declare, Declare::WpPq | Declare::WpScRgb | Declare::SwapchainPq) {
			let manager: WpColorManagerV1 = self.globals.bind(&qh, 1..=3, ()).expect("wp_color_manager_v1");
			let colour_surface = self
				.colour_surface
				.get_or_insert_with(|| manager.get_surface(&self.surface, &qh, ()))
				.clone();
			let creator = manager.create_parametric_creator(&qh, ());
			match declare {
				Declare::WpScRgb => {
					creator.set_tf_named(WpTransferFunction::ExtLinear);
					creator.set_primaries_named(Primaries::Srgb);
				},
				_ => {
					creator.set_tf_named(WpTransferFunction::St2084Pq);
					creator.set_primaries_named(Primaries::Bt2020);
					creator.set_mastering_display_primaries(
						708_000, 292_000, 170_000, 797_000, 131_000, 46_000, 312_700, 329_000,
					);
					creator.set_mastering_luminance(50, 1000);
					creator.set_max_cll(900);
					creator.set_max_fall(300);
				},
			}
			let description = creator.create(&qh, ());
			colour_surface.set_image_description(&description, RenderIntent::Perceptual);
		}
		if declare == Declare::FrogPq {
			let factory: frog_color_management_factory_v1::FrogColorManagementFactoryV1 = self
				.globals
				.bind(&qh, 1..=1, ())
				.expect("frog_color_management_factory_v1");
			let frog = self
				.frog
				.get_or_insert_with(|| factory.get_color_managed_surface(&self.surface, &qh, ()))
				.clone();
			frog.set_known_transfer_function(frog_color_managed_surface::TransferFunction::St2084Pq);
			frog.set_known_container_color_volume(frog_color_managed_surface::Primaries::Rec2020);
			frog.set_hdr_metadata(34000, 16000, 13250, 34500, 7500, 3000, 15635, 16450, 1000, 50, 800, 200);
		}
		if declare == Declare::SwapchainPq {
			let factory: gamescope_swapchain_factory_v2::GamescopeSwapchainFactoryV2 = self
				.globals
				.bind(&qh, 1..=1, ())
				.expect("gamescope_swapchain_factory_v2");
			let swapchain = self
				.swapchain
				.get_or_insert_with(|| factory.create_swapchain(&self.surface, &qh, ()))
				.clone();
			swapchain.swapchain_feedback(3, 64, VK_HDR10, 1, 1, 1, "test-engine".to_string());
			swapchain.set_hdr_metadata(34000, 16000, 13250, 34500, 7500, 3000, 15635, 16450, 1000, 50, 700, 250);
		}
		if declare == Declare::Nothing {
			if let Some(colour_surface) = &self.colour_surface {
				colour_surface.unset_image_description();
			}
			if let Some(frog) = self.frog.take() {
				frog.destroy();
			}
		}
	}

	/// Attach a uniform `width` x `height` buffer of `format` filled with
	/// `pixel` and commit.
	///
	/// A surface that changes its shm format must also change its size: the
	/// renderer caches one texture per surface and reuses it for any buffer of
	/// the same size, whatever its format.
	fn present(&mut self, format: wl_shm::Format, pixel: &[u8], width: u32, height: u32) {
		let qh = self.qh();
		let stride = width as usize * pixel.len();
		let size = stride * height as usize;
		let fd = memfd(size);
		fill(&fd, size, pixel);
		let pool = self.shm().create_pool(fd.as_fd(), size as i32, &qh, ());
		let buffer = pool.create_buffer(0, width as i32, height as i32, stride as i32, format, &qh, ());
		self.surface.attach(Some(&buffer), 0, 0);
		self.surface.damage_buffer(0, 0, width as i32, height as i32);
		self.surface.commit();
		self.keep.push((pool, buffer));
		self.roundtrip();
	}

	/// Attach `dmabuf` as the surface's buffer and commit.
	fn present_dmabuf(&mut self, dmabuf: &Dmabuf) {
		let qh = self.qh();
		let linux_dmabuf: zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1 =
			self.globals.bind(&qh, 3..=3, ()).expect("zwp_linux_dmabuf_v1");
		let params = linux_dmabuf.create_params(&qh, ());
		let modifier = u64::from(dmabuf.format().modifier);
		for ((fd, offset), stride) in dmabuf.handles().zip(dmabuf.offsets()).zip(dmabuf.strides()) {
			params.add(fd, 0, offset, stride, (modifier >> 32) as u32, modifier as u32);
		}
		let buffer = params.create_immed(
			dmabuf.width() as i32,
			dmabuf.height() as i32,
			dmabuf.format().code as u32,
			zwp_linux_buffer_params_v1::Flags::empty(),
			&qh,
			(),
		);
		self.surface.attach(Some(&buffer), 0, 0);
		self.surface
			.damage_buffer(0, 0, dmabuf.width() as i32, dmabuf.height() as i32);
		self.surface.commit();
		self.dmabufs.push(buffer);
		self.roundtrip();
	}

	/// Give the pointer a `size` x `size` cursor of `argb`. Needs the pointer to
	/// have entered the surface.
	fn set_cursor(&mut self, argb: u32, size: u32) {
		let qh = self.qh();
		let serial = self
			.state
			.enter_serial
			.expect("the pointer has not entered the surface");
		let pointer = {
			// The seat's pointer is created from its capabilities event.
			let seat: wl_seat::WlSeat = self.globals.bind(&qh, 1..=1, ()).expect("wl_seat");
			seat.get_pointer(&qh, ())
		};
		self.roundtrip();
		let cursor = self.compositor_global().create_surface(&qh, ());
		let stride = size as usize * 4;
		let fd = memfd(stride * size as usize);
		fill(&fd, stride * size as usize, &argb.to_le_bytes());
		let pool = self
			.shm()
			.create_pool(fd.as_fd(), (stride * size as usize) as i32, &qh, ());
		let buffer = pool.create_buffer(
			0,
			size as i32,
			size as i32,
			stride as i32,
			wl_shm::Format::Argb8888,
			&qh,
			(),
		);
		cursor.attach(Some(&buffer), 0, 0);
		cursor.damage_buffer(0, 0, size as i32, size as i32);
		cursor.commit();
		pointer.set_cursor(serial, Some(&cursor), 0, 0);
		cursor.commit();
		self.keep.push((pool, buffer));
		self.cursor_surfaces.push(cursor);
		self.roundtrip();
	}

	fn compositor_global(&self) -> wl_compositor::WlCompositor {
		self.globals.bind(&self.qh(), 1..=4, ()).expect("wl_compositor")
	}

	fn shm(&self) -> wl_shm::WlShm {
		self.globals.bind(&self.qh(), 1..=1, ()).expect("wl_shm")
	}
}

fn memfd(size: usize) -> OwnedFd {
	use smithay::reexports::rustix::fs::{MemfdFlags, ftruncate, memfd_create};
	let fd = memfd_create("ferriscope-hdr-test", MemfdFlags::CLOEXEC).expect("memfd");
	ftruncate(&fd, size as u64).expect("ftruncate");
	fd
}

/// Fill the first `size` bytes of `fd` with repetitions of `pixel`.
fn fill(fd: &OwnedFd, size: usize, pixel: &[u8]) {
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
	let bytes = unsafe { std::slice::from_raw_parts_mut(ptr.cast::<u8>(), size) };
	for chunk in bytes.chunks_exact_mut(pixel.len()) {
		chunk.copy_from_slice(pixel);
	}
	unsafe { munmap(ptr, size) }.expect("munmap");
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
	fn event(
		_: &mut Self,
		_: &wl_registry::WlRegistry,
		_: wl_registry::Event,
		_: &GlobalListContents,
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
	}
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
	fn event(
		_: &mut Self,
		wm_base: &xdg_wm_base::XdgWmBase,
		event: xdg_wm_base::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		if let xdg_wm_base::Event::Ping { serial } = event {
			wm_base.pong(serial);
		}
	}
}

impl Dispatch<xdg_surface::XdgSurface, ()> for State {
	fn event(
		state: &mut Self,
		surface: &xdg_surface::XdgSurface,
		event: xdg_surface::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		if let xdg_surface::Event::Configure { serial } = event {
			surface.ack_configure(serial);
			state.configured = true;
		}
	}
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
impl Dispatch<wl_seat::WlSeat, ()> for State {
	fn event(
		_: &mut Self,
		seat: &wl_seat::WlSeat,
		event: wl_seat::Event,
		_: &(),
		_: &Connection,
		qh: &QueueHandle<Self>,
	) {
		if let wl_seat::Event::Capabilities { capabilities } = event {
			if capabilities
				.into_result()
				.is_ok_and(|caps| caps.contains(wl_seat::Capability::Pointer))
			{
				seat.get_pointer(qh, ());
			}
		}
	}
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
	fn event(
		state: &mut Self,
		_: &wl_pointer::WlPointer,
		event: wl_pointer::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		if let wl_pointer::Event::Enter { serial, .. } = event {
			state.enter_serial = Some(serial);
		}
	}
}

delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1);
delegate_noop!(State: ignore zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore WpColorManagerV1);
delegate_noop!(State: ignore WpColorManagementSurfaceV1);
delegate_noop!(State: ignore WpImageDescriptionCreatorParamsV1);
delegate_noop!(State: ignore WpImageDescriptionV1);
delegate_noop!(State: ignore frog_color_management_factory_v1::FrogColorManagementFactoryV1);
delegate_noop!(State: ignore frog_color_managed_surface::FrogColorManagedSurface);
delegate_noop!(State: ignore gamescope_swapchain_factory_v2::GamescopeSwapchainFactoryV2);
delegate_noop!(State: ignore gamescope_swapchain::GamescopeSwapchain);

/// An `Xbgr2101010` buffer of the output's size, cleared to `colour` by
/// `renderer`, in a layout the renderer can render to (NVIDIA's EGL takes a
/// linear 10-bit buffer only as an external image, which the composite cannot
/// sample).
fn dmabuf_cleared_to(renderer: &mut GlesRenderer, colour: [f32; 4]) -> Dmabuf {
	let file = std::fs::OpenOptions::new()
		.read(true)
		.write(true)
		.open(RENDER_NODE)
		.expect("open the render node");
	let gbm = GbmDevice::new(Arc::new(file)).expect("gbm device");
	let mut allocator = GbmAllocator::new(gbm, GbmBufferFlags::RENDERING);
	let modifiers: Vec<Modifier> = renderer
		.egl_context()
		.dmabuf_render_formats()
		.iter()
		.filter(|format| format.code == SmithayFourcc::Xbgr2101010 && format.modifier != Modifier::Linear)
		.map(|format| format.modifier)
		.collect();
	let buffer = allocator
		.create_buffer(WIDTH, HEIGHT, SmithayFourcc::Xbgr2101010, &modifiers)
		.expect("allocate a 10-bit buffer");
	let mut dmabuf = buffer.export().expect("export the buffer");
	let size = Size::<i32, Physical>::from((WIDTH as i32, HEIGHT as i32));
	let sync = {
		let mut target = renderer.bind(&mut dmabuf).expect("bind the buffer");
		let mut frame = renderer
			.render(&mut target, size, Transform::Normal)
			.expect("render to the buffer");
		frame
			.clear(Color32F::from(colour), &[Rectangle::from_size(size)])
			.expect("clear the buffer");
		frame.finish().expect("finish the clear")
	};
	sync.wait().expect("wait for the clear");
	dmabuf
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
	fn event(
		_: &mut Self,
		_: &xdg_toplevel::XdgToplevel,
		_: xdg_toplevel::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
	}
}

/// The 8-bit SDR pixel `Argb8888` (B, G, R, A in memory).
const SDR_PIXEL: [u8; 4] = [0x40, 0x80, 0xc0, 0xff];

fn is_sdr(colour: &ColourMetadata) -> bool {
	*colour == ColourMetadata::sdr()
}

#[test]
#[ignore = "requires GPU"]
fn hdr10_content_switches_the_pool_to_10_bit_pq_and_back() {
	let session = Session::start(true);
	assert!(session.ready.hdr, "the handshake does not say HDR is enabled");
	assert!(
		session.ready.env.contains(&("DXVK_HDR".to_string(), "1".to_string())),
		"DXVK_HDR is not advertised with HDR on"
	);
	let mut reader = Reader::new();
	let mut client = session.client();

	// SDR first: the 8-bit pool and no HDR description.
	client.present(wl_shm::Format::Argb8888, &SDR_PIXEL, WIDTH, HEIGHT);
	let (colour, frame) = session.frame_where(|dmabuf| dmabuf.dmabuf.format().code == Fourcc::Abgr8888);
	assert!(is_sdr(&colour), "SDR content is described as {colour:?}");
	drop(frame);

	// HDR10: red 512, green 256, blue 768 of 1023.
	client.declare(Declare::WpPq);
	let word = abgr2101010(512, 256, 768, 3);
	client.present(wl_shm::Format::Abgr2101010, &word.to_le_bytes(), 160, 120);
	let (colour, frame) = session.frame_where(|dmabuf| dmabuf.dmabuf.format().code == Fourcc::Abgr2101010);
	assert_eq!(colour.transfer, TransferFunction::Pq);
	assert_eq!(colour.primaries, ColourPrimaries::Bt2020);
	let mastering = colour.mastering.expect("mastering display metadata");
	assert_eq!(mastering.max_luminance, 1000.0);
	assert!(
		(mastering.min_luminance - 0.005).abs() < 1e-6,
		"{}",
		mastering.min_luminance
	);
	assert!((mastering.red.x - 0.708).abs() < 1e-3, "{}", mastering.red.x);
	assert!((mastering.white_point.y - 0.329).abs() < 1e-3);
	let light = colour.content_light.expect("content light level");
	assert_eq!((light.max_cll, light.max_fall), (900, 300));

	let floats = reader.read(&frame);
	for (x, y) in [
		(0, 0),
		(WIDTH as usize / 2, HEIGHT as usize / 2),
		(WIDTH as usize - 1, HEIGHT as usize - 1),
	] {
		let got = pixel(&floats, x, y);
		assert!(close(got, unorm10(512, 256, 768), 0.001), "pixel ({x}, {y}) is {got:?}");
	}
	drop(frame);

	// Back to SDR: the description goes away and the 8-bit pool returns.
	client.declare(Declare::Nothing);
	client.present(wl_shm::Format::Argb8888, &SDR_PIXEL, 200, 150);
	let (colour, frame) = session.frame_where(|dmabuf| dmabuf.dmabuf.format().code != Fourcc::Abgr2101010);
	assert!(is_sdr(&colour), "SDR content is described as {colour:?}");
	let floats = reader.read(&frame);
	let centre = pixel(&floats, WIDTH as usize / 2, HEIGHT as usize / 2);
	// The buffer was B=0x40, G=0x80, R=0xc0.
	assert!(
		close(centre, [192.0 / 255.0, 128.0 / 255.0, 64.0 / 255.0, 1.0], 0.005),
		"SDR pixel after returning from HDR: {centre:?}"
	);
}

#[test]
#[ignore = "requires GPU"]
fn scrgb_content_switches_the_pool_to_fp16_and_keeps_values_above_one() {
	let session = Session::start(true);
	let mut reader = Reader::new();
	let mut client = session.client();

	client.declare(Declare::WpScRgb);
	let values = [1.0f32, 2.5, 0.25, 1.0];
	let pixel_bytes: Vec<u8> = values.iter().flat_map(|value| half(*value).to_le_bytes()).collect();
	client.present(wl_shm::Format::Abgr16161616f, &pixel_bytes, 160, 120);
	let (colour, frame) = session.frame_where(|dmabuf| dmabuf.dmabuf.format().code == Fourcc::Abgr16161616f);
	assert_eq!(colour.transfer, TransferFunction::Linear);
	assert_eq!(colour.primaries, ColourPrimaries::Bt709);
	assert!(colour.is_hdr());
	assert_eq!(colour.mastering, None);

	let floats = reader.read(&frame);
	let centre = pixel(&floats, WIDTH as usize / 2, HEIGHT as usize / 2);
	assert!(
		close(centre, values, 0.0),
		"scRGB pixel above 1.0 was altered: {centre:?}"
	);
}

#[test]
#[ignore = "requires GPU"]
fn frog_declared_hdr10_switches_the_pool() {
	let session = Session::start(true);
	let mut client = session.client();
	client.declare(Declare::FrogPq);
	let word = abgr2101010(100, 200, 300, 3);
	client.present(wl_shm::Format::Abgr2101010, &word.to_le_bytes(), 160, 120);
	client.surface.commit();
	let (colour, _frame) = session.frame_where(|dmabuf| dmabuf.dmabuf.format().code == Fourcc::Abgr2101010);
	assert_eq!(colour.transfer, TransferFunction::Pq);
	let light = colour.content_light.expect("content light level");
	assert_eq!((light.max_cll, light.max_fall), (800, 200));
}

#[test]
#[ignore = "requires GPU"]
fn the_wsi_swapchain_metadata_wins_over_the_protocol_description() {
	let session = Session::start(true);
	let mut client = session.client();
	// The protocol says CLL 900; the WSI layer's `set_hdr_metadata` says 700.
	client.declare(Declare::WpPq);
	client.declare(Declare::SwapchainPq);
	let word = abgr2101010(100, 200, 300, 3);
	client.present(wl_shm::Format::Abgr2101010, &word.to_le_bytes(), 160, 120);
	let (colour, _frame) = session.frame_where(|dmabuf| dmabuf.dmabuf.format().code == Fourcc::Abgr2101010);
	assert_eq!(colour.transfer, TransferFunction::Pq);
	let light = colour.content_light.expect("content light level");
	assert_eq!((light.max_cll, light.max_fall), (700, 250));
}

#[test]
#[ignore = "requires GPU"]
fn nothing_changes_when_hdr_is_not_enabled() {
	let session = Session::start(false);
	assert!(!session.ready.hdr);
	assert!(
		!session.ready.env.iter().any(|(key, _)| key == "DXVK_HDR"),
		"DXVK_HDR is advertised with HDR off"
	);
	let mut client = session.client();
	// Frog accepts the declaration even when HDR is off; the output stays SDR.
	client.declare(Declare::FrogPq);
	let word = abgr2101010(100, 200, 300, 3);
	client.present(wl_shm::Format::Abgr2101010, &word.to_le_bytes(), 160, 120);
	let (colour, frame) = session.frame_where(|_| true);
	assert!(is_sdr(&colour), "{colour:?}");
	assert!(
		matches!(
			frame.dmabuf.format().code,
			Fourcc::Abgr8888 | Fourcc::Argb8888 | Fourcc::Xrgb8888
		),
		"the pool is {:?}",
		frame.dmabuf.format().code
	);
}

#[test]
#[ignore = "requires GPU"]
fn an_hdr_game_dmabuf_is_composited_with_its_metadata() {
	let session = Session::start(true);
	let mut reader = Reader::new();
	let mut client = session.client();
	client.declare(Declare::WpPq);
	let buffer = dmabuf_cleared_to(&mut reader.renderer, unorm10(512, 256, 768));
	client.present_dmabuf(&buffer);

	// The client's buffer is composited into the 10-bit PQ pool unchanged.
	let (colour, frame) = session.frame_where(|dmabuf| dmabuf.dmabuf.format().code == Fourcc::Abgr2101010);
	let centre = pixel(&reader.read(&frame), WIDTH as usize / 2, HEIGHT as usize / 2);
	assert!(
		close(centre, unorm10(512, 256, 768), 0.001),
		"centre pixel is {centre:?}"
	);
	assert_eq!(colour.transfer, TransferFunction::Pq);
	assert_eq!(colour.primaries, ColourPrimaries::Bt2020);
	assert_eq!(colour.mastering.expect("mastering metadata").max_luminance, 1000.0);
	let light = colour.content_light.expect("content light level");
	assert_eq!((light.max_cll, light.max_fall), (900, 300));
}

#[test]
#[ignore = "requires GPU"]
fn sdr_cursor_over_hdr_content_is_drawn_at_the_sdr_white_level() {
	let session = Session::start(true);
	let mut reader = Reader::new();
	let mut client = session.client();

	client.declare(Declare::WpScRgb);
	let values = [1.0f32, 2.5, 0.25, 1.0];
	let content: Vec<u8> = values.iter().flat_map(|value| half(*value).to_le_bytes()).collect();
	client.present(wl_shm::Format::Abgr16161616f, &content, 160, 120);

	// The pointer enters the window, then the client gives it a white cursor.
	let (x, y) = (100usize, 60usize);
	session
		.handles
		.input
		.send(InputEvent::PointerMotion {
			x: x as f64,
			y: y as f64,
		})
		.expect("inject pointer motion");
	let deadline = Instant::now() + FRAME_TIMEOUT;
	while client.state.enter_serial.is_none() {
		assert!(Instant::now() < deadline, "the pointer never entered the window");
		client.roundtrip();
		std::thread::sleep(Duration::from_millis(20));
	}
	client.set_cursor(0xffff_ffff, 16);

	// SDR white at the default 203 nits is 2.5375 in scRGB (80 nits is 1.0).
	let expected = 203.0f32 / 80.0;
	let deadline = Instant::now() + FRAME_TIMEOUT;
	loop {
		let (_, frame) = session.frame_where(|dmabuf| dmabuf.dmabuf.format().code == Fourcc::Abgr16161616f);
		let floats = reader.read(&frame);
		let inside = pixel(&floats, x + 4, y + 4);
		if close(inside, [expected, expected, expected, 1.0], 0.01) {
			// Away from the cursor the HDR content is untouched.
			let away = pixel(&floats, x + 100, y + 100);
			assert!(close(away, values, 0.0), "content beside the cursor changed: {away:?}");
			return;
		}
		assert!(
			Instant::now() < deadline,
			"the cursor never reached the SDR white level; last: {inside:?}"
		);
	}
}
