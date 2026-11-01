//! GPU integration tests for HDR output.
//!
//! A Wayland client declares what its surface holds through the WSI layer's
//! `gamescope_swapchain` (the only colour source, as in gamescope) and
//! presents a buffer of known values. The tests check what the compositor
//! delivers: the pool format, the `FrameInfo::colour` metadata and, by reading
//! the DMA-BUF back through a renderer of the test's own, the pixel values.
//!
//! They are `#[ignore]`d: they need a render node. Run them alone:
//!
//! ```text
//! cargo test -p ferriscope --test hdr_output -- --ignored --test-threads=1 --nocapture
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

use ferriscope::{
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
		drm::{DrmDeviceFd, DrmNode},
		renderer::{Bind, Color32F, ExportMem, Frame as _, ImportDma, Renderer, vulkan::VulkanRenderer},
		vulkan::{Instance, PhysicalDevice, version::Version},
	},
	reexports::rustix::{
		event::{PollFd, PollFlags, Timespec, poll},
		mm::{MapFlags, ProtFlags, mmap, munmap},
	},
	utils::{DeviceFd, Physical, Rectangle, Size, Transform},
};
use wayland_client::{
	Connection, Dispatch, QueueHandle, delegate_noop,
	globals::{GlobalList, GlobalListContents, registry_queue_init},
	protocol::{wl_buffer, wl_compositor, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface},
};
use wayland_protocols::{
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

use protocols::swapchain::{gamescope_swapchain, gamescope_swapchain_factory_v2};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;
const READY_TIMEOUT: Duration = Duration::from_secs(30);
const FRAME_TIMEOUT: Duration = Duration::from_secs(30);
const RENDER_NODE: &str = "/dev/dri/renderD128";

/// `VK_COLOR_SPACE_SRGB_NONLINEAR_KHR`.
const VK_SRGB: u32 = 0;
/// `VK_COLOR_SPACE_HDR10_ST2084_EXT`.
const VK_HDR10: u32 = 1_000_104_008;
/// `VK_COLOR_SPACE_EXTENDED_SRGB_LINEAR_EXT`.
const VK_SCRGB: u32 = 1_000_104_002;

/// What a client declares about its surface.
#[derive(Clone, Copy, PartialEq)]
enum Declare {
	/// Nothing: the surface is SDR (an sRGB swapchain, if it had one).
	Nothing,
	/// The WSI layer: an HDR10 swapchain with `set_hdr_metadata` (mastering
	/// 1000 cd/m2, CLL 700 and FALL 250).
	SwapchainPq,
	/// The WSI layer: an scRGB swapchain without metadata.
	SwapchainScRgb,
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
	renderer: VulkanRenderer,
}

impl Reader {
	fn new() -> Self {
		use std::os::unix::fs::MetadataExt;

		let file = std::fs::OpenOptions::new()
			.read(true)
			.write(true)
			.open(RENDER_NODE)
			.expect("open the render node");
		let node = DrmNode::from_dev_id(file.metadata().expect("stat the render node").rdev()).expect("drm node");
		let instance = Instance::new(Version::VERSION_1_3, None).expect("vulkan instance");
		let phd = PhysicalDevice::enumerate(&instance)
			.expect("enumerate vulkan devices")
			.find(|phd| phd.render_node().ok().flatten() == Some(node))
			.expect("a vulkan device for the render node");
		let drm = DrmDeviceFd::new(DeviceFd::from(std::os::fd::OwnedFd::from(file)));
		let renderer = VulkanRenderer::new(&phd, Some(drm)).expect("vulkan renderer");
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
		let fourcc = frame.dmabuf.format().code;
		let region = Rectangle::from_size((WIDTH as i32, HEIGHT as i32).into());
		let mapping = self
			.renderer
			.copy_texture(&texture, region, fourcc)
			.expect("read the delivered dmabuf back");
		let bytes = self.renderer.map_texture(&mapping).expect("map the readback");
		match fourcc {
			Fourcc::Abgr16161616f | Fourcc::Xbgr16161616f => bytes
				.as_chunks::<2>()
				.0
				.iter()
				.map(|half| f16_to_f32(u16::from_le_bytes(*half)))
				.collect(),
			Fourcc::Abgr2101010 | Fourcc::Xbgr2101010 => bytes
				.as_chunks::<4>()
				.0
				.iter()
				.flat_map(|word| {
					let word = u32::from_le_bytes(*word);
					[
						(word & 0x3ff) as f32 / 1023.0,
						((word >> 10) & 0x3ff) as f32 / 1023.0,
						((word >> 20) & 0x3ff) as f32 / 1023.0,
						(word >> 30) as f32 / 3.0,
					]
				})
				.collect(),
			// Abgr8888: R, G, B, A bytes in memory.
			_ => bytes.iter().map(|byte| f32::from(*byte) / 255.0).collect(),
		}
	}
}

/// An IEEE 754 half-precision float.
fn f16_to_f32(bits: u16) -> f32 {
	let sign = if bits & 0x8000 != 0 { -1.0 } else { 1.0 };
	let exponent = i32::from((bits >> 10) & 0x1f);
	let mantissa = f32::from(bits & 0x3ff);
	sign * match exponent {
		0 => mantissa * 2f32.powi(-24),
		31 => f32::INFINITY,
		_ => (1.0 + mantissa / 1024.0) * 2f32.powi(exponent - 15),
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

/// A packed `Argb2101010` word: 10 bits each of red, green and blue, 2 of alpha.
fn argb2101010(r: u32, g: u32, b: u32, a: u32) -> u32 {
	(a << 30) | (r << 20) | (g << 10) | b
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
		let (vk_format, vk_colorspace) = match declare {
			// No swapchain yet: nothing to declare.
			Declare::Nothing if self.swapchain.is_none() => return,
			// `VK_FORMAT_B8G8R8A8_UNORM`.
			Declare::Nothing => (44, VK_SRGB),
			// `VK_FORMAT_A2B10G10R10_UNORM_PACK32`.
			Declare::SwapchainPq => (64, VK_HDR10),
			// `VK_FORMAT_R16G16B16A16_SFLOAT`.
			Declare::SwapchainScRgb => (97, VK_SCRGB),
		};
		let factory: gamescope_swapchain_factory_v2::GamescopeSwapchainFactoryV2 = self
			.globals
			.bind(&qh, 1..=1, ())
			.expect("gamescope_swapchain_factory_v2");
		let swapchain = self
			.swapchain
			.get_or_insert_with(|| factory.create_swapchain(&self.surface, &qh, ()))
			.clone();
		swapchain.swapchain_feedback(3, vk_format, vk_colorspace, 1, 1, 1, "test-engine".to_string());
		if declare == Declare::SwapchainPq {
			swapchain.set_hdr_metadata(34000, 16000, 13250, 34500, 7500, 3000, 15635, 16450, 1000, 50, 700, 250);
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
		if let wl_seat::Event::Capabilities { capabilities } = event
			&& capabilities
				.into_result()
				.is_ok_and(|caps| caps.contains(wl_seat::Capability::Pointer))
		{
			seat.get_pointer(qh, ());
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
delegate_noop!(State: ignore gamescope_swapchain_factory_v2::GamescopeSwapchainFactoryV2);
delegate_noop!(State: ignore gamescope_swapchain::GamescopeSwapchain);

/// A `fourcc` buffer of the output's size, cleared to `colour` by
/// `renderer`, in a tiled layout the renderer can render to.
fn dmabuf_cleared_to(renderer: &mut VulkanRenderer, colour: [f32; 4]) -> Dmabuf {
	let file = std::fs::OpenOptions::new()
		.read(true)
		.write(true)
		.open(RENDER_NODE)
		.expect("open the render node");
	let gbm = GbmDevice::new(Arc::new(file)).expect("gbm device");
	let mut allocator = GbmAllocator::new(gbm, GbmBufferFlags::RENDERING);
	let modifiers: Vec<Modifier> = Bind::<Dmabuf>::supported_formats(renderer)
		.expect("render formats")
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
	client.declare(Declare::SwapchainPq);
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
	assert!((mastering.red.x - 0.68).abs() < 1e-3, "{}", mastering.red.x);
	assert!((mastering.white_point.y - 0.329).abs() < 1e-3);
	let light = colour.content_light.expect("content light level");
	assert_eq!((light.max_cll, light.max_fall), (700, 250));

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

	// Back to SDR: the swapchain turns sRGB and the 8-bit pool returns.
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

	client.declare(Declare::SwapchainScRgb);
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
fn nothing_changes_when_hdr_is_not_enabled() {
	let session = Session::start(false);
	assert!(!session.ready.hdr);
	assert!(
		!session.ready.env.iter().any(|(key, _)| key == "DXVK_HDR"),
		"DXVK_HDR is advertised with HDR off"
	);
	let mut client = session.client();
	// The swapchain declares HDR10 although HDR is off; the output stays SDR.
	client.declare(Declare::SwapchainPq);
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
fn an_hdr_game_dmabuf_is_passed_through_with_its_metadata() {
	let session = Session::start(true);
	let mut reader = Reader::new();
	let mut client = session.client();
	client.declare(Declare::SwapchainPq);
	let buffer = dmabuf_cleared_to(&mut reader.renderer, unorm10(512, 256, 768));
	client.present_dmabuf(&buffer);

	// The buffer is opaque, output-sized and alone, so the client's own buffer
	// is delivered (its format, not the pool's), with no composite to wait on.
	let (colour, frame) = session.frame_where(|dmabuf| dmabuf.dmabuf.format().code == Fourcc::Xbgr2101010);
	assert!(
		frame.acquire_fence.is_none(),
		"a passed-through frame has no composite to fence"
	);
	let centre = pixel(&reader.read(&frame), WIDTH as usize / 2, HEIGHT as usize / 2);
	assert!(
		close(centre, unorm10(512, 256, 768), 0.5 / 1023.0),
		"centre pixel is {centre:?}"
	);
	assert_eq!(colour.transfer, TransferFunction::Pq);
	assert_eq!(colour.primaries, ColourPrimaries::Bt2020);
	assert_eq!(colour.mastering.expect("mastering metadata").max_luminance, 1000.0);
	let light = colour.content_light.expect("content light level");
	assert_eq!((light.max_cll, light.max_fall), (700, 250));
}

#[test]
#[ignore = "requires GPU"]
fn an_argb2101010_hdr_buffer_is_composited_with_its_channel_order() {
	let session = Session::start(true);
	let mut reader = Reader::new();
	let mut client = session.client();
	client.declare(Declare::SwapchainPq);
	// The other HDR10 format the WSI layer offers (`A2R10G10B10_UNORM_PACK32`):
	// not passed through, so it is composited into the `Abgr2101010` pool.
	let word = argb2101010(512, 256, 768, 3);
	client.present(wl_shm::Format::Argb2101010, &word.to_le_bytes(), 160, 120);

	let (_colour, frame) = session.frame_where(|dmabuf| dmabuf.dmabuf.format().code == Fourcc::Abgr2101010);
	let centre = pixel(&reader.read(&frame), WIDTH as usize / 2, HEIGHT as usize / 2);
	assert!(
		close(centre, unorm10(512, 256, 768), 0.5 / 1023.0),
		"centre pixel is {centre:?}"
	);
}

#[test]
#[ignore = "requires GPU"]
fn sdr_cursor_over_hdr_content_is_drawn_at_the_sdr_white_level() {
	let session = Session::start(true);
	let mut reader = Reader::new();
	let mut client = session.client();

	client.declare(Declare::SwapchainScRgb);
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
