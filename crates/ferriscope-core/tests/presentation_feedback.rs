//! GPU integration test for `wp_presentation`.
//!
//! Gates:
//! - the global is advertised with `CLOCK_MONOTONIC`;
//! - a commit's feedback is answered with `presented`, timestamped between the
//!   commit and the moment the client sees the event, with the session
//!   interval as refresh and no `vsync` flag;
//! - a commit superseded before a frame is delivered is answered `discarded`.
//!
//! `#[ignore]`d: needs a render node.
//!
//! ```text
//! cargo test -p ferriscope-core --test presentation_feedback -- --ignored --test-threads=1
//! ```

use std::{
	io::Write,
	os::{fd::AsFd, unix::net::UnixStream},
	path::PathBuf,
	time::{Duration, Instant},
};

use ferriscope_core::{Compositor, CompositorConfig, CompositorHandles};
use smithay::reexports::rustix::time::{ClockId, clock_gettime};
use wayland_client::{
	Connection, Dispatch, EventQueue, QueueHandle, WEnum, delegate_noop,
	globals::{GlobalListContents, registry_queue_init},
	protocol::{wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface},
};
use wayland_protocols::{
	wp::{
		fifo::v1::client::{wp_fifo_manager_v1, wp_fifo_v1},
		presentation_time::client::{wp_presentation, wp_presentation_feedback},
	},
	xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base},
};

const SIZE: i32 = 64;
const READY_TIMEOUT: Duration = Duration::from_secs(20);
const EVENT_TIMEOUT: Duration = Duration::from_secs(10);
const RATE: f64 = 60.0;

#[derive(Debug, PartialEq)]
enum Outcome {
	Presented {
		time: Duration,
		refresh: Duration,
		sequence: u64,
		flags: u32,
		received: Duration,
	},
	Discarded,
}

#[derive(Default)]
struct State {
	configured: bool,
	clock_id: Option<u32>,
	/// Outcome per feedback, keyed by the id passed at creation.
	outcomes: Vec<(u32, Outcome)>,
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore xdg_toplevel::XdgToplevel);
delegate_noop!(State: ignore wp_fifo_manager_v1::WpFifoManagerV1);
delegate_noop!(State: ignore wp_fifo_v1::WpFifoV1);

fn now() -> Duration {
	let ts = clock_gettime(ClockId::Monotonic);
	Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
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

impl Dispatch<wp_presentation::WpPresentation, ()> for State {
	fn event(
		state: &mut Self,
		_: &wp_presentation::WpPresentation,
		event: wp_presentation::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		if let wp_presentation::Event::ClockId { clk_id } = event {
			state.clock_id = Some(clk_id);
		}
	}
}

impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, u32> for State {
	fn event(
		state: &mut Self,
		_: &wp_presentation_feedback::WpPresentationFeedback,
		event: wp_presentation_feedback::Event,
		id: &u32,
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		let outcome = match event {
			wp_presentation_feedback::Event::Presented {
				tv_sec_hi,
				tv_sec_lo,
				tv_nsec,
				refresh,
				seq_hi,
				seq_lo,
				flags,
			} => Outcome::Presented {
				time: Duration::new((u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo), tv_nsec),
				refresh: Duration::from_nanos(u64::from(refresh)),
				sequence: (u64::from(seq_hi) << 32) | u64::from(seq_lo),
				flags: match flags {
					WEnum::Value(flags) => flags.bits(),
					WEnum::Unknown(raw) => raw,
				},
				received: now(),
			},
			wp_presentation_feedback::Event::Discarded => Outcome::Discarded,
			_ => return,
		};
		state.outcomes.push((*id, outcome));
	}
}

struct Client {
	queue: EventQueue<State>,
	state: State,
	surface: wl_surface::WlSurface,
	presentation: wp_presentation::WpPresentation,
	buffer: wl_buffer::WlBuffer,
	fifo: wp_fifo_v1::WpFifoV1,
}

struct Harness {
	compositor: Option<Compositor>,
	_handles: CompositorHandles,
	runtime_dir: std::ffi::OsString,
	display: std::ffi::OsString,
	client: Client,
}

impl std::ops::Deref for Harness {
	type Target = Client;
	fn deref(&self) -> &Client {
		&self.client
	}
}

impl std::ops::DerefMut for Harness {
	fn deref_mut(&mut self) -> &mut Client {
		&mut self.client
	}
}

impl Drop for Harness {
	fn drop(&mut self) {
		if let Some(compositor) = self.compositor.take() {
			let _ = compositor.stop();
		}
	}
}

impl Harness {
	/// The slots of the DMA-BUF pool are never released here, so the consumer
	/// stalls once the pool is full.
	fn start() -> Self {
		let config = CompositorConfig {
			width: SIZE as u32,
			height: SIZE as u32,
			rate: RATE,
			xwayland: false,
			command: None,
			render_node: PathBuf::from("/dev/dri/renderD128"),
			..CompositorConfig::default()
		};
		let (compositor, handles) = Compositor::launch(config).expect("launch compositor");
		let ready = handles.ready.recv_timeout(READY_TIMEOUT).expect("ready handshake");
		let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR");
		let display = std::ffi::OsString::from(ready.wayland_display.clone());
		let client = Client::connect(&runtime_dir, &display);
		Self {
			compositor: Some(compositor),
			_handles: handles,
			runtime_dir,
			display,
			client,
		}
	}

	/// Another fullscreen client; once mapped it takes focus from the first.
	fn second_client(&self) -> Client {
		Client::connect(&self.runtime_dir, &self.display)
	}
}

impl Client {
	fn connect(runtime_dir: &std::ffi::OsStr, display: &std::ffi::OsStr) -> Self {
		let socket = UnixStream::connect(PathBuf::from(runtime_dir).join(display)).expect("connect");
		let conn = Connection::from_socket(socket).expect("wayland connection");
		let (globals, mut queue) = registry_queue_init::<State>(&conn).expect("registry");
		let qh = queue.handle();

		let compositor_global: wl_compositor::WlCompositor = globals.bind(&qh, 4..=4, ()).expect("wl_compositor");
		let wm_base: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=1, ()).expect("xdg_wm_base");
		let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).expect("wl_shm");
		let presentation: wp_presentation::WpPresentation = globals.bind(&qh, 1..=2, ()).expect("wp_presentation");
		let fifo_manager: wp_fifo_manager_v1::WpFifoManagerV1 =
			globals.bind(&qh, 1..=1, ()).expect("wp_fifo_manager_v1");

		let surface = compositor_global.create_surface(&qh, ());
		let fifo = fifo_manager.get_fifo(&surface, &qh, ());
		let xdg = wm_base.get_xdg_surface(&surface, &qh, ());
		let toplevel = xdg.get_toplevel(&qh, ());
		toplevel.set_fullscreen(None);
		surface.commit();
		let mut state = State::default();
		while !state.configured {
			queue.blocking_dispatch(&mut state).expect("dispatch");
		}

		let bytes = (SIZE * SIZE * 4) as usize;
		let mut file = tempfile_in(runtime_dir);
		file.write_all(&vec![0x80; bytes]).expect("fill pool");
		let pool = shm.create_pool(file.as_fd(), bytes as i32, &qh, ());
		let buffer = pool.create_buffer(0, SIZE, SIZE, SIZE * 4, wl_shm::Format::Argb8888, &qh, ());
		pool.destroy();

		Self {
			queue,
			state,
			surface,
			presentation,
			buffer,
			fifo,
		}
	}

	/// Commit the buffer with a feedback request tagged `id`; returns the
	/// commit time. Does not wait for the compositor.
	fn commit(&mut self, id: u32) -> Duration {
		let qh = self.queue.handle();
		self.presentation.feedback(&self.surface, &qh, id);
		self.surface.attach(Some(&self.buffer), 0, 0);
		self.surface.damage_buffer(0, 0, SIZE, SIZE);
		let at = now();
		self.surface.commit();
		at
	}

	fn flush(&self) {
		self.queue.flush().expect("flush");
	}

	/// Dispatch until feedback `id` has an outcome.
	fn wait_outcome(&mut self, id: u32) -> Outcome {
		let deadline = Instant::now() + EVENT_TIMEOUT;
		loop {
			if let Some(pos) = self.state.outcomes.iter().position(|(got, _)| *got == id) {
				return self.state.outcomes.remove(pos).1;
			}
			assert!(Instant::now() < deadline, "no outcome for feedback {id}");
			self.queue.flush().expect("flush");
			if let Some(guard) = self.queue.prepare_read() {
				let fd = guard.connection_fd().as_fd().try_clone_to_owned().expect("dup fd");
				let mut poll = [smithay::reexports::rustix::event::PollFd::new(
					&fd,
					smithay::reexports::rustix::event::PollFlags::IN,
				)];
				let _ = smithay::reexports::rustix::event::poll(
					&mut poll,
					Some(&smithay::reexports::rustix::time::Timespec {
						tv_sec: 0,
						tv_nsec: 50_000_000,
					}),
				);
				let _ = guard.read();
			}
			self.queue.dispatch_pending(&mut self.state).expect("dispatch");
		}
	}
}

fn tempfile_in(dir: &std::ffi::OsStr) -> std::fs::File {
	let path = PathBuf::from(dir).join(format!(
		"ferriscope-presentation-{}-{}",
		std::process::id(),
		now().as_nanos()
	));
	let file = std::fs::OpenOptions::new()
		.read(true)
		.write(true)
		.create(true)
		.truncate(true)
		.open(&path)
		.expect("create pool file");
	std::fs::remove_file(&path).expect("unlink pool file");
	file
}

#[test]
#[ignore = "requires a GPU render node"]
fn presented_is_timestamped_between_commit_and_delivery() {
	let mut h = Harness::start();
	h.client.queue.roundtrip(&mut h.client.state).expect("roundtrip");
	assert_eq!(h.state.clock_id, Some(1), "CLOCK_MONOTONIC");

	let committed = h.commit(1);
	h.flush();
	match h.wait_outcome(1) {
		Outcome::Presented {
			time,
			refresh,
			flags,
			sequence: _,
			received,
		} => {
			assert!(time >= committed, "presented {time:?} before commit {committed:?}");
			assert!(time <= received, "presented {time:?} after receipt {received:?}");
			assert_eq!(refresh, Duration::from_secs_f64(1.0 / RATE));
			assert_eq!(
				flags & wp_presentation_feedback::Kind::Vsync.bits(),
				0,
				"no vblank, no VSYNC"
			);
		},
		Outcome::Discarded => panic!("a delivered commit was discarded"),
	}
}

#[test]
#[ignore = "requires a GPU render node"]
fn a_superseded_commit_is_discarded() {
	let mut h = Harness::start();
	// Both commits reach the compositor in one read, before a frame can be
	// delivered between them.
	h.commit(1);
	h.commit(2);
	h.flush();
	assert_eq!(h.wait_outcome(1), Outcome::Discarded);
	assert!(matches!(h.wait_outcome(2), Outcome::Presented { .. }));
}

#[test]
#[ignore = "requires a GPU render node"]
fn fifo_waiting_commit_lands_in_a_later_frame_than_the_barrier() {
	let mut h = Harness::start();
	// Without fifo the second commit would supersede the first (see above).
	// The first sets a barrier and the second waits on it.
	h.fifo.set_barrier();
	h.commit(1);
	h.fifo.wait_barrier();
	h.commit(2);
	h.flush();
	let first = h.wait_outcome(1);
	let second = h.wait_outcome(2);
	let (Outcome::Presented { sequence: a, .. }, Outcome::Presented { sequence: b, .. }) = (first, second) else {
		panic!("both fifo commits must be presented");
	};
	assert!(
		b > a,
		"the waiting commit ({b}) must land in a later frame than the barrier ({a})"
	);
}

/// Run `COMMITS` fifo-paced commits (each waits on the previous barrier and sets
/// the next) and return how long the last one took to be superseded by its
/// successor or presented, i.e. to be applied by the compositor.
fn fifo_run(client: &mut Client) -> Duration {
	const COMMITS: u32 = 13;
	let start = Instant::now();
	for id in 0..COMMITS {
		client.fifo.wait_barrier();
		client.fifo.set_barrier();
		client.commit(id);
		client.flush();
	}
	// Commit `id` is superseded (discarded) once commit `id + 1` is applied, and
	// applying happens only when the previous barrier is released.
	for id in 0..COMMITS - 1 {
		client.wait_outcome(id);
	}
	start.elapsed()
}

/// One cadence tick per commit at the session rate, with generous slack; the
/// static keepalive (one second per frame) would take far longer.
fn assert_session_cadence(elapsed: Duration) {
	assert!(
		elapsed < Duration::from_millis(1500),
		"fifo barriers were not released on the frame-callback cadence: {elapsed:?}"
	);
}

#[test]
#[ignore = "requires a GPU render node"]
fn fifo_barriers_follow_the_callback_cadence_when_the_consumer_stalls() {
	// Every pool slot ends up held by an unreleasing consumer, so no further
	// frame is delivered. The client must keep advancing at the session rate.
	let mut h = Harness::start();
	let elapsed = fifo_run(&mut h.client);
	assert_session_cadence(elapsed);
}

#[test]
#[ignore = "requires a GPU render node"]
fn fifo_barriers_follow_the_callback_cadence_when_the_window_is_hidden() {
	let mut h = Harness::start();
	let mut other = h.second_client();
	// Both windows commit a buffer, then the scene is static. Only one window
	// is shown at a time, so whichever is hidden must still advance at the
	// session rate: run both.
	h.commit(1000);
	h.flush();
	other.commit(1000);
	other.flush();
	h.wait_outcome(1000);
	other.wait_outcome(1000);
	assert_session_cadence(fifo_run(&mut h.client));
	assert_session_cadence(fifo_run(&mut other));
}
