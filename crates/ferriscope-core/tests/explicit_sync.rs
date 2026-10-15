//! GPU integration test for explicit sync (`linux-drm-syncobj-v1`).
//!
//! Gates:
//! - the compositor advertises `wp_linux_drm_syncobj_manager_v1`;
//! - a commit with an unsignalled acquire point is not delivered until the
//!   point signals, and is delivered promptly afterwards;
//! - the replaced buffer's release point signals once the new buffer has been
//!   composited.
//!
//! `#[ignore]`d: needs a render node with syncobj eventfd support.
//!
//! ```text
//! cargo test -p ferriscope-core --test explicit_sync -- --ignored --test-threads=1
//! ```

use std::{
	os::{fd::OwnedFd, unix::net::UnixStream},
	path::PathBuf,
	sync::Arc,
	time::{Duration, Instant},
};

use ferriscope_core::{
	Compositor, CompositorConfig, CompositorHandles, DmaBufFrame, Frame, FrameData, RecvTimeoutError, Release,
};
use smithay::{
	backend::{
		allocator::{
			Allocator, Buffer, Fourcc, Modifier,
			dmabuf::{AsDmabuf, Dmabuf},
			gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
		},
		drm::DrmDeviceFd,
	},
	reexports::drm::control::{Device as ControlDevice, syncobj},
	utils::DeviceFd,
};
use wayland_client::{
	Connection, Dispatch, EventQueue, QueueHandle, delegate_noop,
	globals::{GlobalList, GlobalListContents, registry_queue_init},
	protocol::{wl_buffer, wl_compositor, wl_registry, wl_surface},
};
use wayland_protocols::{
	wp::{
		linux_dmabuf::zv1::client::{zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1},
		linux_drm_syncobj::v1::client::{
			wp_linux_drm_syncobj_manager_v1, wp_linux_drm_syncobj_surface_v1, wp_linux_drm_syncobj_timeline_v1,
		},
	},
	xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base},
};

const SIZE: i32 = 64;
const READY_TIMEOUT: Duration = Duration::from_secs(20);
const FRAME_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Default)]
struct State {
	configured: bool,
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore xdg_toplevel::XdgToplevel);
delegate_noop!(State: ignore zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1);
delegate_noop!(State: ignore zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1);
delegate_noop!(State: ignore wp_linux_drm_syncobj_manager_v1::WpLinuxDrmSyncobjManagerV1);
delegate_noop!(State: ignore wp_linux_drm_syncobj_surface_v1::WpLinuxDrmSyncobjSurfaceV1);
delegate_noop!(State: ignore wp_linux_drm_syncobj_timeline_v1::WpLinuxDrmSyncobjTimelineV1);

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
		base: &xdg_wm_base::XdgWmBase,
		event: xdg_wm_base::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		if let xdg_wm_base::Event::Ping { serial } = event {
			base.pong(serial);
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

/// A client-side timeline syncobj, imported into the compositor.
struct Timeline {
	handle: syncobj::Handle,
	proxy: wp_linux_drm_syncobj_timeline_v1::WpLinuxDrmSyncobjTimelineV1,
}

struct Harness {
	compositor: Option<Compositor>,
	handles: CompositorHandles,
	queue: EventQueue<State>,
	state: State,
	globals: GlobalList,
	drm: DrmDeviceFd,
	allocator: GbmAllocator<Arc<std::fs::File>>,
	surface: wl_surface::WlSurface,
	manager: Option<wp_linux_drm_syncobj_manager_v1::WpLinuxDrmSyncobjManagerV1>,
	syncobj_surface: Option<wp_linux_drm_syncobj_surface_v1::WpLinuxDrmSyncobjSurfaceV1>,
	dmabuf: zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
}

impl Drop for Harness {
	fn drop(&mut self) {
		if let Some(compositor) = self.compositor.take() {
			let _ = compositor.stop();
		}
	}
}

fn render_node() -> PathBuf {
	PathBuf::from("/dev/dri/renderD128")
}

impl Harness {
	fn start() -> Self {
		let config = CompositorConfig {
			width: SIZE as u32,
			height: SIZE as u32,
			xwayland: false,
			command: None,
			render_node: render_node(),
			..CompositorConfig::default()
		};
		let (compositor, handles) = Compositor::launch(config).expect("launch compositor");
		let ready = handles.ready.recv_timeout(READY_TIMEOUT).expect("ready handshake");
		let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR");
		let socket = UnixStream::connect(PathBuf::from(runtime_dir).join(&ready.wayland_display)).expect("connect");
		let conn = Connection::from_socket(socket).expect("wayland connection");
		let (globals, mut queue) = registry_queue_init::<State>(&conn).expect("registry");
		let qh = queue.handle();

		let compositor_global: wl_compositor::WlCompositor = globals.bind(&qh, 4..=4, ()).expect("wl_compositor");
		let wm_base: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=1, ()).expect("xdg_wm_base");
		let dmabuf: zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1 = globals.bind(&qh, 3..=4, ()).expect("linux-dmabuf");
		let manager = globals
			.bind::<wp_linux_drm_syncobj_manager_v1::WpLinuxDrmSyncobjManagerV1, _, _>(&qh, 1..=1, ())
			.ok();

		let surface = compositor_global.create_surface(&qh, ());
		let xdg = wm_base.get_xdg_surface(&surface, &qh, ());
		let toplevel = xdg.get_toplevel(&qh, ());
		toplevel.set_fullscreen(None);
		surface.commit();
		let mut state = State::default();
		while !state.configured {
			queue.blocking_dispatch(&mut state).expect("dispatch");
		}

		let file = std::fs::OpenOptions::new()
			.read(true)
			.write(true)
			.open(render_node())
			.expect("open render node");
		let drm = DrmDeviceFd::new(DeviceFd::from(OwnedFd::from(file.try_clone().expect("dup"))));
		let gbm = GbmDevice::new(Arc::new(file)).expect("gbm");
		let allocator = GbmAllocator::new(gbm, GbmBufferFlags::RENDERING);
		let syncobj_surface = manager.as_ref().map(|m| m.get_surface(&surface, &qh, ()));

		Self {
			compositor: Some(compositor),
			handles,
			queue,
			state,
			globals,
			drm,
			allocator,
			surface,
			manager,
			syncobj_surface,
			dmabuf,
		}
	}

	fn new_buffer(&mut self) -> wl_buffer::WlBuffer {
		let qh = self.queue.handle();
		let bo = self
			.allocator
			.create_buffer(SIZE as u32, SIZE as u32, Fourcc::Argb8888, &[Modifier::Linear])
			.expect("allocate");
		let dmabuf: Dmabuf = bo.export().expect("export");
		let params = self.dmabuf.create_params(&qh, ());
		let modifier: u64 = dmabuf.format().modifier.into();
		for ((fd, offset), stride) in dmabuf.handles().zip(dmabuf.offsets()).zip(dmabuf.strides()) {
			params.add(fd, 0, offset, stride, (modifier >> 32) as u32, modifier as u32);
		}
		let buffer = params.create_immed(
			SIZE,
			SIZE,
			Fourcc::Argb8888 as u32,
			zwp_linux_buffer_params_v1::Flags::empty(),
			&qh,
			(),
		);
		params.destroy();
		// Keep the allocation alive for the buffer's lifetime.
		std::mem::forget(dmabuf);
		buffer
	}

	fn new_timeline(&mut self) -> Timeline {
		let qh = self.queue.handle();
		let handle = self.drm.create_syncobj(false).expect("create syncobj");
		let fd = self.drm.syncobj_to_fd(handle, false).expect("export syncobj");
		let proxy = self
			.manager
			.as_ref()
			.expect("syncobj manager")
			.import_timeline(fd.as_fd_borrowed(), &qh, ());
		Timeline { handle, proxy }
	}

	fn signal(&self, timeline: &Timeline, point: u64) {
		self.drm
			.syncobj_timeline_signal(&[timeline.handle], &[point])
			.expect("signal point");
	}

	fn signalled(&self, timeline: &Timeline) -> u64 {
		let mut points = [0u64];
		self.drm
			.syncobj_timeline_query(&[timeline.handle], &mut points, false)
			.expect("query");
		points[0]
	}

	/// Attach `buffer` with acquire point 1 and release point 2 on `timeline`.
	fn commit_explicit(&mut self, buffer: &wl_buffer::WlBuffer, timeline: &Timeline) {
		let sync = self.syncobj_surface.as_ref().expect("syncobj surface");
		sync.set_acquire_point(&timeline.proxy, 0, 1);
		sync.set_release_point(&timeline.proxy, 0, 2);
		self.surface.attach(Some(buffer), 0, 0);
		self.surface.damage_buffer(0, 0, SIZE, SIZE);
		self.surface.commit();
		self.queue.roundtrip(&mut self.state).expect("roundtrip");
	}

	fn recv(&self, timeout: Duration) -> Option<DmaBufFrame> {
		let deadline = Instant::now() + timeout;
		loop {
			let remaining = deadline.saturating_duration_since(Instant::now());
			if remaining.is_zero() {
				return None;
			}
			match self.handles.frames.recv_timeout(remaining) {
				Ok(Frame {
					data: FrameData::DmaBuf(frame),
					..
				}) => {
					self.release(&frame);
					return Some(frame);
				},
				Ok(_) => {},
				Err(RecvTimeoutError::Timeout) => return None,
				Err(RecvTimeoutError::Disconnected) => panic!("frame mailbox disconnected"),
			}
		}
	}

	fn release(&self, frame: &DmaBufFrame) {
		let _ = self.handles.releases.send(Release {
			buffer_id: frame.buffer_id,
			fence: None,
		});
	}
}

trait AsFdBorrowed {
	fn as_fd_borrowed(&self) -> std::os::fd::BorrowedFd<'_>;
}

impl AsFdBorrowed for OwnedFd {
	fn as_fd_borrowed(&self) -> std::os::fd::BorrowedFd<'_> {
		std::os::fd::AsFd::as_fd(self)
	}
}

#[test]
#[ignore = "requires GPU with syncobj eventfd support"]
fn syncobj_global_is_advertised() {
	let harness = Harness::start();
	assert!(
		harness
			.globals
			.contents()
			.with_list(|list| list.iter().any(|g| g.interface == "wp_linux_drm_syncobj_manager_v1")),
		"wp_linux_drm_syncobj_manager_v1 not advertised"
	);
}

#[test]
#[ignore = "requires GPU with syncobj eventfd support"]
fn commit_waits_for_acquire_point_and_release_follows() {
	let mut h = Harness::start();

	// Buffer A: acquire signalled up front, so it becomes current at once.
	let a = h.new_buffer();
	let ta = h.new_timeline();
	h.signal(&ta, 1);
	h.commit_explicit(&a, &ta);
	assert!(h.recv(FRAME_TIMEOUT).is_some(), "no frame for the first buffer");
	assert!(h.signalled(&ta) < 2, "A's release point signalled while A is current");

	// Buffer B: acquire point still pending. Align to just after a delivered
	// frame so the 1s static keepalive cannot land in the observation window.
	let tb = h.new_timeline();
	let b = h.new_buffer();
	assert!(h.recv(FRAME_TIMEOUT).is_some(), "no keepalive frame");
	h.commit_explicit(&b, &tb);
	assert!(
		h.recv(Duration::from_millis(500)).is_none(),
		"a frame was delivered although B's acquire point was unsignalled"
	);
	assert!(h.signalled(&ta) < 2, "A was released before B became current");

	// The signal must wake the loop and deliver without waiting for a timer.
	let signalled_at = Instant::now();
	h.signal(&tb, 1);
	assert!(
		h.recv(Duration::from_millis(400)).is_some(),
		"no frame after the signal"
	);
	assert!(signalled_at.elapsed() < Duration::from_millis(400));

	// A's release point fires once B has been composited.
	let deadline = Instant::now() + Duration::from_secs(2);
	while h.signalled(&ta) < 2 && Instant::now() < deadline {
		std::thread::sleep(Duration::from_millis(10));
	}
	assert!(h.signalled(&ta) >= 2, "A's release point never signalled");
}
