//! Shared raw-`wayland-client` / `wayland-protocols` harness for the native
//! Wayland integration tests (`tests/wayland_focus.rs`,
//! `tests/cursor_capture_frames.rs`).
//!
//! Test-only: the product never links a Wayland client (`[dev-dependencies]`).
//! The module binds the globals, maps one or two `xdg_toplevel`s backed by a
//! real `wl_shm` buffer, lets the test inject and query through a command
//! channel, and records the client-observable events into a shared log.
#![allow(dead_code)]

use std::{
	os::fd::{AsFd, OwnedFd},
	os::unix::net::{UnixListener, UnixStream},
	path::{Path, PathBuf},
	sync::{
		Arc, Mutex,
		atomic::{AtomicU32, Ordering},
		mpsc,
	},
	thread,
	time::{Duration, Instant},
};

use ferriscope::{Compositor, CompositorConfig, CompositorHandles, Ready};

mod tablet;
pub use tablet::{PenLogEvent, TabletInfo};

use wayland_client::{
	Connection, Dispatch, Proxy, QueueHandle, WEnum,
	backend::ObjectId,
	delegate_noop,
	globals::{GlobalListContents, registry_queue_init},
	protocol::{
		wl_buffer, wl_compositor, wl_keyboard, wl_pointer, wl_region, wl_registry, wl_seat, wl_shm, wl_shm_pool,
		wl_surface, wl_touch,
	},
};
use wayland_protocols::{
	wp::{
		pointer_constraints::zv1::client::{
			zwp_confined_pointer_v1, zwp_locked_pointer_v1, zwp_pointer_constraints_v1,
		},
		relative_pointer::zv1::client::{zwp_relative_pointer_manager_v1, zwp_relative_pointer_v1},
	},
	xdg::{
		activation::v1::client::{xdg_activation_token_v1, xdg_activation_v1},
		shell::client::{xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base},
	},
};

/// Budget for the `Ready` handshake.
const READY_TIMEOUT: Duration = Duration::from_secs(20);
/// Budget for the compositor to connect to the test's control socket.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(10);
/// Budget for one client command round trip.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
/// Budget for the activation token `done` event.
const TOKEN_TIMEOUT: Duration = Duration::from_secs(5);
/// Watchdog budget for `Compositor::stop` not to hang.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// A recorded client-side view of the events under test.
#[derive(Debug, Clone, Default)]
pub struct Log {
	/// Formats advertised by `wl_shm` (used to pick the buffer format).
	pub shm_formats: Vec<wl_shm::Format>,
	/// The size from the last `xdg_toplevel.configure`.
	pub configure_size: (i32, i32),
	/// Number of `xdg_surface.configure` events acknowledged.
	pub configure_acks: usize,
	/// Surface ids that received `wl_pointer.enter`.
	pub pointer_enter: Vec<u32>,
	/// Pointer positions from `wl_pointer.enter` / `motion`.
	pub pointer_motion: Vec<(f64, f64)>,
	/// Deltas from `zwp_relative_pointer.relative_motion`.
	pub relative_motion: Vec<(f64, f64)>,
	/// `wl_pointer.axis` events as `(axis, value)`.
	pub pointer_axis: Vec<(wl_pointer::Axis, f64)>,
	/// `wl_pointer.axis_discrete` steps, as `(axis, steps)`. Sent to v5–v7
	/// pointers; a v8+ pointer sees [`Self::pointer_axis_value120`] instead.
	pub pointer_axis_discrete: Vec<(wl_pointer::Axis, i32)>,
	/// `wl_pointer.axis_value120` steps, as `(axis, value120)`. Sent to v8+
	/// pointers.
	pub pointer_axis_value120: Vec<(wl_pointer::Axis, i32)>,
	/// `wl_pointer.axis_source` values reported for axis events.
	pub pointer_axis_source: Vec<wl_pointer::AxisSource>,
	/// Axes with a `wl_pointer.axis_stop`.
	pub pointer_axis_stop: Vec<wl_pointer::Axis>,
	/// Number of `zwp_locked_pointer.locked` events.
	pub locked: usize,
	/// Number of `zwp_confined_pointer_v1.confined` events.
	pub confined: usize,
	/// Number of `zwp_confined_pointer_v1.unconfined` events.
	pub unconfined: usize,
	/// Surface ids whose `xdg_toplevel.configure` carried `Activated`.
	pub activated: Vec<u32>,
	/// Surface ids whose `xdg_toplevel.configure` carried `Fullscreen`.
	pub fullscreen: Vec<u32>,
	/// Tokens from `xdg_activation_token_v1.done`.
	pub activation_tokens: Vec<String>,
	/// Surface ids that received `wl_keyboard.enter`.
	pub keyboard_enter: Vec<u32>,
	/// Surface ids that received `wl_keyboard.leave`.
	pub keyboard_leave: Vec<u32>,
	/// `wl_pointer.button` events as (evdev button, pressed).
	pub pointer_button: Vec<(u32, bool)>,
	/// `wl_touch` events in arrival order.
	pub touch: Vec<TouchEvent>,
	/// `zwp_tablet_tool_v2` events in arrival order.
	pub pen: Vec<PenLogEvent>,
	/// The tablets and tools advertised to the client.
	pub tablet: TabletInfo,
	/// Every `wl_keyboard.keymap` the client received, as keymap text.
	pub keymaps: Vec<String>,
	/// The popup's geometry from its last `xdg_popup.configure`, relative to
	/// its parent.
	pub popup_configure: Option<(i32, i32, i32, i32)>,
	/// Number of `xdg_popup.popup_done` events (the compositor dismissed it).
	pub popup_done: usize,
	/// `wl_keyboard.key` events as (evdev key, pressed), in arrival order.
	pub keys: Vec<(u32, bool)>,
	/// Events of the keyboard in arrival order: a keymap switch is
	/// `KeyboardEvent::Keymap(index into keymaps)`.
	pub keyboard_events: Vec<KeyboardEvent>,
}

/// A client-observed `wl_touch` event.
#[derive(Debug, Clone, PartialEq)]
pub enum TouchEvent {
	Down { id: i32, x: f64, y: f64 },
	Motion { id: i32, x: f64, y: f64 },
	Up { id: i32 },
	Frame,
	Cancel,
}

/// A client-observed keyboard event, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyboardEvent {
	Keymap(usize),
	Key { evdev: u32, pressed: bool },
}

/// Which toplevel a command or query refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
	/// The first normal toplevel: the single fullscreen window.
	Parent,
	/// A toplevel parented to [`Target::Parent`]: the transient.
	Child,
	/// A toplevel created after [`Target::Parent`] is destroyed.
	Second,
	/// An `xdg_popup` of [`Target::Parent`].
	Popup,
}

/// Shared by the client thread and the test: surface ids under each label.
#[derive(Default)]
struct Ids {
	parent: AtomicU32,
	child: AtomicU32,
	second: AtomicU32,
	popup: AtomicU32,
}

impl Ids {
	fn get(&self, target: Target) -> Option<u32> {
		let value = match target {
			Target::Parent => self.parent.load(Ordering::Acquire),
			Target::Child => self.child.load(Ordering::Acquire),
			Target::Second => self.second.load(Ordering::Acquire),
			Target::Popup => self.popup.load(Ordering::Acquire),
		};
		(value != 0).then_some(value)
	}

	fn set(&self, target: Target, id: u32) {
		match target {
			Target::Parent => self.parent.store(id, Ordering::Release),
			Target::Child => self.child.store(id, Ordering::Release),
			Target::Second => self.second.store(id, Ordering::Release),
			Target::Popup => self.popup.store(id, Ordering::Release),
		}
	}
}

/// What the client should set up before the test drives it.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClientOptions {
	/// Also map a toplevel parented to the window (a transient/dialog).
	pub with_child: bool,
	/// Commit a buffer of this size instead of the configured output size, so
	/// the output-fill scaling can be exercised with a smaller surface.
	pub buffer_size: Option<(i32, i32)>,
}

/// A command the test sends to the client thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandKind {
	/// Create a relative pointer and a persistent `lock` on the window.
	LockRelative,
	/// Create a `zwp_relative_pointer` on the window **without** a constraint,
	/// so a relative delta can be observed with no capture active.
	RelativePointer,
	/// Create a persistent `confine` on the window with a surface-local
	/// rectangle region.
	Confine {
		/// Region x, in surface-local coordinates.
		x: i32,
		/// Region y, in surface-local coordinates.
		y: i32,
		/// Region width.
		width: i32,
		/// Region height.
		height: i32,
	},
	/// Create a persistent `confine` on the window with **no** region, as
	/// Xwayland does for an X confine-to grab (`xwayland-input.c:3570`). The
	/// protocol default is the whole surface.
	ConfineSurface,
	/// Destroy the active confine constraint, releasing the pointer.
	Unconfine,
	/// Destroy the active lock (and its relative pointer), releasing the
	/// pointer and restoring the cursor.
	Unlock,
	/// Set a solid-colour client cursor image using the serial of the last
	/// `wl_pointer.enter`, so the compositor composites it.
	SetCursor,
	/// Request `xdg_activation_v1` activation for the window.
	Activate,
	/// Request `xdg_activation_v1` activation for the second toplevel (the
	/// peer), so a peer's focus-steal attempt can be asserted ignored.
	ActivateSecond,
	/// Destroy the window toplevel.
	DestroyParent,
	/// Map a fresh unparented toplevel.
	CreateSecondToplevel,
	/// Map a plain peer normal toplevel **after** the role holder, covering the
	/// whole output, without destroying the role holder. It is committed at the
	/// configured output size regardless of [`ClientOptions::buffer_size`], so
	/// it is z-above and larger than a scaled role holder.
	CreatePeerToplevel,
	/// Map a transient/dialog parented to the current role holder, after the
	/// peer, to assert a dialog still takes keyboard focus.
	CreateChildToplevel,
	/// Map an `xdg_popup` of the window, anchored at the window-local point
	/// `(x, y)` and opening down and to the right, flipped or slid to stay on
	/// the output. With `grab`, it takes an explicit grab with the serial of
	/// the last `wl_pointer.button`, like a menu opened by a click.
	CreatePopup {
		/// Anchor x, window-local.
		x: i32,
		/// Anchor y, window-local.
		y: i32,
		/// Popup width.
		width: i32,
		/// Popup height.
		height: i32,
		/// Take an explicit grab.
		grab: bool,
	},
}

/// A mapped popup and the objects that keep it alive.
struct MappedPopup {
	_surface: wl_surface::WlSurface,
	_xdg_surface: xdg_surface::XdgSurface,
	_popup: xdg_popup::XdgPopup,
	_shm: ShmBuffer,
}

struct Command {
	kind: CommandKind,
	ack: mpsc::SyncSender<()>,
}

/// One mapped toplevel and the objects that keep it alive.
struct MappedSurface {
	surface: wl_surface::WlSurface,
	xdg_surface: xdg_surface::XdgSurface,
	toplevel: xdg_toplevel::XdgToplevel,
	shm: ShmBuffer,
}

impl MappedSurface {
	fn destroy(self) {
		self.shm.buffer.destroy();
		self.toplevel.destroy();
		self.xdg_surface.destroy();
		self.surface.destroy();
	}
}

/// The `wl_shm` pool and buffer backing a surface, plus the fd they came from.
struct ShmBuffer {
	// Kept alive: the fd must outlive the `create_pool` request.
	_fd: OwnedFd,
	// Kept alive while the buffer exists; never read directly.
	_pool: wl_shm_pool::WlShmPool,
	buffer: wl_buffer::WlBuffer,
}

/// The per-connection state the `Dispatch` impls mutate.
struct App {
	log: Arc<Mutex<Log>>,
	ids: Arc<Ids>,
	compositor: wl_compositor::WlCompositor,
	shm: wl_shm::WlShm,
	wm_base: xdg_wm_base::XdgWmBase,
	// Bound to create the pointer/keyboard; the seat events themselves are
	// handled from the `Dispatch` parameter.
	_seat: wl_seat::WlSeat,
	pointer: Option<wl_pointer::WlPointer>,
	keyboard: Option<wl_keyboard::WlKeyboard>,
	touch: Option<wl_touch::WlTouch>,
	// Kept so the tablet seat's tablets and tools keep reporting.
	_tablet_seat: wayland_protocols::wp::tablet::zv2::client::zwp_tablet_seat_v2::ZwpTabletSeatV2,
	/// Serial from the last `wl_pointer.enter`, required by `set_cursor`.
	pointer_enter_serial: Option<u32>,
	/// Serial from the last `wl_pointer.button`, required by `xdg_popup.grab`.
	pointer_button_serial: Option<u32>,
	popup: Option<MappedPopup>,
	relative_manager: zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
	pointer_constraints: zwp_pointer_constraints_v1::ZwpPointerConstraintsV1,
	activation: xdg_activation_v1::XdgActivationV1,
	parent: Option<MappedSurface>,
	child: Option<MappedSurface>,
	second: Option<MappedSurface>,
	// Toplevel-to-surface ids, recorded at creation. The initial configure
	// arrives during the round trip that precedes the surface being stored,
	// so the id cannot be recovered from `parent`/`child`/`second` yet.
	toplevel_ids: Vec<(ObjectId, u32)>,
	// Kept alive for the lifetime of the lock/relative pointer objects.
	_relative_pointer: Option<zwp_relative_pointer_v1::ZwpRelativePointerV1>,
	_locked_pointer: Option<zwp_locked_pointer_v1::ZwpLockedPointerV1>,
	// The confine region and constraint, kept alive until `Unconfine`.
	_confine_region: Option<wl_region::WlRegion>,
	_confined_pointer: Option<zwp_confined_pointer_v1::ZwpConfinedPointerV1>,
	// The cursor surface and its buffer, kept alive while it is the image.
	_cursor_surface: Option<wl_surface::WlSurface>,
	_cursor_shm: Option<ShmBuffer>,
	// When set, toplevels commit this buffer size instead of the configured
	// output size (output-fill scaling tests).
	buffer_size: Option<(i32, i32)>,
}

impl App {
	fn surfaces(&self) -> impl Iterator<Item = &MappedSurface> {
		[self.parent.as_ref(), self.child.as_ref(), self.second.as_ref()]
			.into_iter()
			.flatten()
	}

	/// The `wl_surface` id of the toplevel whose `xdg_toplevel` is `id`.
	fn surface_id_for_toplevel(&self, id: &ObjectId) -> Option<u32> {
		self.surfaces()
			.find(|surface| surface.toplevel.id() == *id)
			.map(|surface| surface.surface.id().protocol_id())
			.or_else(|| {
				self.toplevel_ids
					.iter()
					.rev()
					.find(|(toplevel, _)| toplevel == id)
					.map(|(_, surface)| *surface)
			})
	}

	/// Create a `wl_shm` buffer of `width` x `height` in an advertised
	/// format, filled with the 32-bit little-endian word `fill` (AARRGGBB,
	/// which is also a valid XRGB pixel with alpha forced to 0xff).
	fn create_shm_buffer(&self, qh: &QueueHandle<Self>, width: i32, height: i32, fill: u32) -> ShmBuffer {
		let stride = width * 4;
		let size = (stride * height) as usize;
		let fd = shm_file(size, fill).expect("create shm backing file");
		let format = {
			let log = self.log.lock().expect("log lock");
			log.shm_formats
				.iter()
				.copied()
				.find(|format| matches!(format, wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888))
				.or_else(|| log.shm_formats.first().copied())
				.unwrap_or(wl_shm::Format::Argb8888)
		};
		let pool = self.shm.create_pool(fd.as_fd(), size as i32, qh, ());
		let buffer = pool.create_buffer(0, width, height, stride, format, qh, ());
		ShmBuffer {
			_fd: fd,
			_pool: pool,
			buffer,
		}
	}

	/// Create, map and (once mapped) focus a toplevel, keyed by `target`.
	///
	/// `buffer_size` overrides the committed buffer size; `None` means the
	/// configured output size. A scaled role holder passes
	/// [`ClientOptions::buffer_size`] here, a full-output peer passes `None`.
	fn create_toplevel(
		&mut self,
		queue: &mut wayland_client::EventQueue<Self>,
		qh: &QueueHandle<Self>,
		parent: Option<&xdg_toplevel::XdgToplevel>,
		target: Target,
		buffer_size: Option<(i32, i32)>,
	) {
		let surface = self.compositor.create_surface(qh, ());
		let xdg_surface = self.wm_base.get_xdg_surface(&surface, qh, ());
		let toplevel = xdg_surface.get_toplevel(qh, ());
		toplevel.set_title("ferriscope-test".to_string());
		if let Some(parent) = parent {
			toplevel.set_parent(Some(parent));
		}
		// Record the id before the configure arrives so `Activated` can be
		// attributed to this surface.
		self.toplevel_ids.push((toplevel.id(), surface.id().protocol_id()));
		// A bufferless commit starts the configure sequence; ack it before
		// attaching a buffer, as the protocol requires.
		let acks = self.log.lock().expect("log lock").configure_acks;
		surface.commit();
		let deadline = Instant::now() + Duration::from_secs(5);
		while {
			let acked = self.log.lock().expect("log lock").configure_acks > acks;
			!acked && Instant::now() < deadline
		} {
			if queue.roundtrip(self).is_err() {
				break;
			}
		}
		let (width, height) = match buffer_size {
			// Deliberately commit a buffer smaller than the configured output,
			// to exercise the output-fill scaling.
			Some(size) => size,
			None => {
				let log = self.log.lock().expect("log lock");
				if log.configure_size.0 > 0 && log.configure_size.1 > 0 {
					log.configure_size
				} else {
					(320, 240)
				}
			},
		};
		let shm = self.create_shm_buffer(qh, width, height, WINDOW_COLOR);
		surface.attach(Some(&shm.buffer), 0, 0);
		surface.damage(0, 0, width, height);
		surface.commit();
		// Let the server map the window and move focus; the test asserts on
		// the resulting events, never on this round trip itself.
		let _ = queue.roundtrip(self);

		let id = surface.id().protocol_id();
		self.ids.set(target, id);
		let mapped = MappedSurface {
			surface,
			xdg_surface,
			toplevel,
			shm,
		};
		match target {
			Target::Parent => self.parent = Some(mapped),
			Target::Child => self.child = Some(mapped),
			Target::Second => self.second = Some(mapped),
			Target::Popup => unreachable!("a popup is not a toplevel"),
		}
	}

	/// Map a popup of the window; see [`CommandKind::CreatePopup`].
	fn create_popup(
		&mut self,
		queue: &mut wayland_client::EventQueue<Self>,
		qh: &QueueHandle<Self>,
		(x, y, width, height): (i32, i32, i32, i32),
		grab: bool,
	) {
		let Some(parent) = self.parent.as_ref().map(|parent| parent.xdg_surface.clone()) else {
			return;
		};
		let positioner = self.wm_base.create_positioner(qh, ());
		positioner.set_size(width, height);
		positioner.set_anchor_rect(x, y, 1, 1);
		positioner.set_anchor(xdg_positioner::Anchor::TopLeft);
		positioner.set_gravity(xdg_positioner::Gravity::BottomRight);
		positioner.set_constraint_adjustment(
			xdg_positioner::ConstraintAdjustment::FlipX
				| xdg_positioner::ConstraintAdjustment::FlipY
				| xdg_positioner::ConstraintAdjustment::SlideX
				| xdg_positioner::ConstraintAdjustment::SlideY,
		);
		let surface = self.compositor.create_surface(qh, ());
		let xdg_surface = self.wm_base.get_xdg_surface(&surface, qh, ());
		let popup = xdg_surface.get_popup(Some(&parent), &positioner, qh, ());
		positioner.destroy();
		if grab {
			if let Some(serial) = self.pointer_button_serial {
				popup.grab(&self._seat, serial);
			}
		}
		// As for a toplevel: a bufferless commit, then ack the configure
		// before attaching a buffer. Without the configure it never maps.
		let acks = self.log.lock().expect("log lock").configure_acks;
		surface.commit();
		let deadline = Instant::now() + Duration::from_secs(5);
		while self.log.lock().expect("log lock").configure_acks == acks && Instant::now() < deadline {
			if queue.roundtrip(self).is_err() {
				break;
			}
		}
		let shm = self.create_shm_buffer(qh, width, height, WINDOW_COLOR);
		surface.attach(Some(&shm.buffer), 0, 0);
		surface.damage(0, 0, width, height);
		surface.commit();
		let _ = queue.roundtrip(self);
		self.ids.set(Target::Popup, surface.id().protocol_id());
		self.popup = Some(MappedPopup {
			_surface: surface,
			_xdg_surface: xdg_surface,
			_popup: popup,
			_shm: shm,
		});
	}

	/// Create a relative pointer and a persistent lock on the window.
	fn lock_relative(&mut self, qh: &QueueHandle<Self>) {
		let pointer = self.pointer.clone();
		let surface = self.parent.as_ref().map(|parent| parent.surface.clone());
		let (Some(pointer), Some(surface)) = (pointer, surface) else {
			return;
		};
		self._relative_pointer = Some(self.relative_manager.get_relative_pointer(&pointer, qh, ()));
		self._locked_pointer = Some(self.pointer_constraints.lock_pointer(
			&surface,
			&pointer,
			None,
			zwp_pointer_constraints_v1::Lifetime::Persistent,
			qh,
			(),
		));
	}

	/// Create a relative pointer with no constraint.
	fn relative_pointer(&mut self, qh: &QueueHandle<Self>) {
		let pointer = self.pointer.clone();
		let Some(pointer) = pointer else {
			return;
		};
		self._relative_pointer = Some(self.relative_manager.get_relative_pointer(&pointer, qh, ()));
	}

	/// Create a persistent confine on the window with a `wl_region`.
	fn confine(&mut self, qh: &QueueHandle<Self>, x: i32, y: i32, width: i32, height: i32) {
		let pointer = self.pointer.clone();
		let surface = self.parent.as_ref().map(|parent| parent.surface.clone());
		let (Some(pointer), Some(surface)) = (pointer, surface) else {
			return;
		};
		let region = self.compositor.create_region(qh, ());
		region.add(x, y, width, height);
		self._confined_pointer = Some(self.pointer_constraints.confine_pointer(
			&surface,
			&pointer,
			Some(&region),
			zwp_pointer_constraints_v1::Lifetime::Persistent,
			qh,
			(),
		));
		self._confine_region = Some(region);
	}

	/// Create a persistent confine with no region (the whole surface), the
	/// shape Xwayland uses for an X confine-to grab.
	fn confine_surface(&mut self, qh: &QueueHandle<Self>) {
		let pointer = self.pointer.clone();
		let surface = self.parent.as_ref().map(|parent| parent.surface.clone());
		let (Some(pointer), Some(surface)) = (pointer, surface) else {
			return;
		};
		self._confined_pointer = Some(self.pointer_constraints.confine_pointer(
			&surface,
			&pointer,
			None,
			zwp_pointer_constraints_v1::Lifetime::Persistent,
			qh,
			(),
		));
	}

	/// Destroy the confine constraint, releasing the pointer.
	fn unconfine(&mut self) {
		if let Some(confined) = self._confined_pointer.take() {
			confined.destroy();
		}
		if let Some(region) = self._confine_region.take() {
			region.destroy();
		}
	}

	/// Destroy the lock and its relative pointer, releasing the pointer and
	/// letting the compositor restore the cursor.
	fn unlock(&mut self) {
		if let Some(locked) = self._locked_pointer.take() {
			locked.destroy();
		}
		if let Some(relative) = self._relative_pointer.take() {
			relative.destroy();
		}
	}

	/// Set a solid-colour cursor image on the window's pointer.
	///
	/// `wl_pointer.set_cursor` only accepts the serial of the last
	/// `wl_pointer.enter` for the same client, so this is a no-op until the
	/// test has injected absolute motion and the enter arrived.
	fn set_cursor(&mut self, qh: &QueueHandle<Self>) {
		let (Some(pointer), Some(serial)) = (self.pointer.clone(), self.pointer_enter_serial) else {
			return;
		};
		let surface = self.compositor.create_surface(qh, ());
		let shm = self.create_shm_buffer(qh, CURSOR_SIZE, CURSOR_SIZE, CURSOR_COLOR);
		surface.attach(Some(&shm.buffer), 0, 0);
		surface.damage(0, 0, CURSOR_SIZE, CURSOR_SIZE);
		surface.commit();
		pointer.set_cursor(serial, Some(&surface), 0, 0);
		// A second commit after the role is assigned makes the buffer
		// current for the next composite.
		surface.commit();
		self._cursor_surface = Some(surface);
		self._cursor_shm = Some(shm);
	}

	/// Request activation, waiting (bounded) for the token first.
	fn activate(&mut self, queue: &mut wayland_client::EventQueue<Self>, qh: &QueueHandle<Self>, target: Target) {
		let surface = match target {
			Target::Parent => self.parent.as_ref().map(|parent| parent.surface.clone()),
			Target::Child => self.child.as_ref().map(|child| child.surface.clone()),
			Target::Second => self.second.as_ref().map(|second| second.surface.clone()),
			Target::Popup => None,
		};
		let Some(surface) = surface else {
			return;
		};
		let token = self.activation.get_activation_token(qh, ());
		// The server issues the token on `commit`, not on creation.
		token.set_surface(&surface);
		token.commit();
		let deadline = Instant::now() + TOKEN_TIMEOUT;
		while self.log.lock().expect("log lock").activation_tokens.is_empty() && Instant::now() < deadline {
			if queue.roundtrip(self).is_err() {
				break;
			}
		}
		if let Some(token) = self.log.lock().expect("log lock").activation_tokens.last().cloned() {
			self.activation.activate(token, &surface);
		}
		token.destroy();
	}

	fn handle(&mut self, kind: CommandKind, queue: &mut wayland_client::EventQueue<Self>, qh: &QueueHandle<Self>) {
		match kind {
			CommandKind::LockRelative => self.lock_relative(qh),
			CommandKind::RelativePointer => self.relative_pointer(qh),
			CommandKind::Confine { x, y, width, height } => self.confine(qh, x, y, width, height),
			CommandKind::ConfineSurface => self.confine_surface(qh),
			CommandKind::Unconfine => self.unconfine(),
			CommandKind::Unlock => self.unlock(),
			CommandKind::SetCursor => self.set_cursor(qh),
			CommandKind::Activate => self.activate(queue, qh, Target::Parent),
			CommandKind::ActivateSecond => self.activate(queue, qh, Target::Second),
			CommandKind::DestroyParent => {
				if let Some(parent) = self.parent.take() {
					parent.destroy();
					self.ids.set(Target::Parent, 0);
				}
			},
			CommandKind::CreateSecondToplevel => {
				self.create_toplevel(queue, qh, None, Target::Second, self.buffer_size)
			},
			// A peer must cover the output, so it ignores `buffer_size` and
			// commits the configured output size.
			CommandKind::CreatePeerToplevel => self.create_toplevel(queue, qh, None, Target::Second, None),
			CommandKind::CreateChildToplevel => {
				let parent = self.parent.as_ref().map(|parent| parent.toplevel.clone());
				self.create_toplevel(queue, qh, parent.as_ref(), Target::Child, self.buffer_size)
			},
			CommandKind::CreatePopup {
				x,
				y,
				width,
				height,
				grab,
			} => self.create_popup(queue, qh, (x, y, width, height), grab),
		}
		let _ = queue.roundtrip(self);
	}
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for App {
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

delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_surface::WlSurface);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
delegate_noop!(App: ignore wl_buffer::WlBuffer);
delegate_noop!(App: ignore wl_region::WlRegion);
delegate_noop!(App: ignore zwp_pointer_constraints_v1::ZwpPointerConstraintsV1);
delegate_noop!(App: ignore zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1);
delegate_noop!(App: ignore xdg_activation_v1::XdgActivationV1);
delegate_noop!(App: ignore xdg_positioner::XdgPositioner);

impl Dispatch<wl_shm::WlShm, ()> for App {
	fn event(state: &mut Self, _: &wl_shm::WlShm, event: wl_shm::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
		if let wl_shm::Event::Format {
			format: WEnum::Value(format),
		} = event
		{
			state.log.lock().expect("log lock").shm_formats.push(format);
		}
	}
}

impl Dispatch<wl_seat::WlSeat, ()> for App {
	fn event(
		state: &mut Self,
		seat: &wl_seat::WlSeat,
		event: wl_seat::Event,
		_: &(),
		_: &Connection,
		qh: &QueueHandle<Self>,
	) {
		if let wl_seat::Event::Capabilities {
			capabilities: WEnum::Value(capabilities),
		} = event
		{
			if capabilities.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
				state.pointer = Some(seat.get_pointer(qh, ()));
			}
			if capabilities.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
				state.keyboard = Some(seat.get_keyboard(qh, ()));
			}
			if capabilities.contains(wl_seat::Capability::Touch) && state.touch.is_none() {
				state.touch = Some(seat.get_touch(qh, ()));
			}
		}
	}
}

impl Dispatch<wl_pointer::WlPointer, ()> for App {
	fn event(
		state: &mut Self,
		_: &wl_pointer::WlPointer,
		event: wl_pointer::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		match event {
			wl_pointer::Event::Enter {
				surface,
				surface_x,
				surface_y,
				serial,
				..
			} => {
				// `set_cursor` requires the enter serial; remember it.
				state.pointer_enter_serial = Some(serial);
				let mut log = state.log.lock().expect("log lock");
				log.pointer_enter.push(surface.id().protocol_id());
				log.pointer_motion.push((surface_x, surface_y));
			},
			wl_pointer::Event::Motion {
				surface_x, surface_y, ..
			} => {
				state
					.log
					.lock()
					.expect("log lock")
					.pointer_motion
					.push((surface_x, surface_y));
			},
			wl_pointer::Event::Axis {
				axis: WEnum::Value(axis),
				value,
				..
			} => {
				state.log.lock().expect("log lock").pointer_axis.push((axis, value));
			},
			wl_pointer::Event::AxisDiscrete {
				axis: WEnum::Value(axis),
				discrete,
			} => {
				state
					.log
					.lock()
					.expect("log lock")
					.pointer_axis_discrete
					.push((axis, discrete));
			},
			wl_pointer::Event::AxisValue120 {
				axis: WEnum::Value(axis),
				value120,
			} => {
				state
					.log
					.lock()
					.expect("log lock")
					.pointer_axis_value120
					.push((axis, value120));
			},
			wl_pointer::Event::Button {
				button,
				state: WEnum::Value(button_state),
				serial,
				..
			} => {
				state.pointer_button_serial = Some(serial);
				state
					.log
					.lock()
					.expect("log lock")
					.pointer_button
					.push((button, button_state == wl_pointer::ButtonState::Pressed));
			},
			wl_pointer::Event::AxisSource {
				axis_source: WEnum::Value(source),
			} => {
				state.log.lock().expect("log lock").pointer_axis_source.push(source);
			},
			wl_pointer::Event::AxisStop {
				axis: WEnum::Value(axis),
				..
			} => {
				state.log.lock().expect("log lock").pointer_axis_stop.push(axis);
			},
			_ => {},
		}
	}
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for App {
	fn event(
		state: &mut Self,
		_: &wl_keyboard::WlKeyboard,
		event: wl_keyboard::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		match event {
			wl_keyboard::Event::Enter { surface, .. } => {
				state
					.log
					.lock()
					.expect("log lock")
					.keyboard_enter
					.push(surface.id().protocol_id());
			},
			wl_keyboard::Event::Leave { surface, .. } => {
				state
					.log
					.lock()
					.expect("log lock")
					.keyboard_leave
					.push(surface.id().protocol_id());
			},
			wl_keyboard::Event::Keymap { fd, size, .. } => {
				let text = read_keymap(fd, size as usize);
				let mut log = state.log.lock().expect("log lock");
				log.keymaps.push(text);
				let index = log.keymaps.len() - 1;
				log.keyboard_events.push(KeyboardEvent::Keymap(index));
			},
			wl_keyboard::Event::Key {
				key,
				state: WEnum::Value(key_state),
				..
			} => {
				let pressed = key_state == wl_keyboard::KeyState::Pressed;
				let mut log = state.log.lock().expect("log lock");
				log.keys.push((key, pressed));
				log.keyboard_events.push(KeyboardEvent::Key { evdev: key, pressed });
			},
			_ => {},
		}
	}
}

/// Read a `wl_keyboard.keymap` fd (a NUL-terminated XKB text keymap).
fn read_keymap(fd: OwnedFd, size: usize) -> String {
	use std::os::unix::fs::FileExt;
	// The descriptor shares its offset with the sender; read from the start.
	let file = std::fs::File::from(fd);
	let mut bytes = vec![0u8; size];
	let _ = file.read_exact_at(&mut bytes, 0);
	while bytes.last() == Some(&0) {
		bytes.pop();
	}
	String::from_utf8_lossy(&bytes).into_owned()
}

impl Dispatch<wl_touch::WlTouch, ()> for App {
	fn event(
		state: &mut Self,
		_: &wl_touch::WlTouch,
		event: wl_touch::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		let touch = match event {
			wl_touch::Event::Down { id, x, y, .. } => TouchEvent::Down { id, x, y },
			wl_touch::Event::Motion { id, x, y, .. } => TouchEvent::Motion { id, x, y },
			wl_touch::Event::Up { id, .. } => TouchEvent::Up { id },
			wl_touch::Event::Frame => TouchEvent::Frame,
			wl_touch::Event::Cancel => TouchEvent::Cancel,
			_ => return,
		};
		state.log.lock().expect("log lock").touch.push(touch);
	}
}

impl Dispatch<zwp_relative_pointer_v1::ZwpRelativePointerV1, ()> for App {
	fn event(
		state: &mut Self,
		_: &zwp_relative_pointer_v1::ZwpRelativePointerV1,
		event: zwp_relative_pointer_v1::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		if let zwp_relative_pointer_v1::Event::RelativeMotion { dx, dy, .. } = event {
			state.log.lock().expect("log lock").relative_motion.push((dx, dy));
		}
	}
}

impl Dispatch<zwp_locked_pointer_v1::ZwpLockedPointerV1, ()> for App {
	fn event(
		state: &mut Self,
		_: &zwp_locked_pointer_v1::ZwpLockedPointerV1,
		event: zwp_locked_pointer_v1::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		if let zwp_locked_pointer_v1::Event::Locked = event {
			state.log.lock().expect("log lock").locked += 1;
		}
	}
}

impl Dispatch<zwp_confined_pointer_v1::ZwpConfinedPointerV1, ()> for App {
	fn event(
		state: &mut Self,
		_: &zwp_confined_pointer_v1::ZwpConfinedPointerV1,
		event: zwp_confined_pointer_v1::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		match event {
			zwp_confined_pointer_v1::Event::Confined => {
				state.log.lock().expect("log lock").confined += 1;
			},
			zwp_confined_pointer_v1::Event::Unconfined => {
				state.log.lock().expect("log lock").unconfined += 1;
			},
			_ => {},
		}
	}
}

impl Dispatch<xdg_activation_token_v1::XdgActivationTokenV1, ()> for App {
	fn event(
		state: &mut Self,
		_: &xdg_activation_token_v1::XdgActivationTokenV1,
		event: xdg_activation_token_v1::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		if let xdg_activation_token_v1::Event::Done { token } = event {
			state.log.lock().expect("log lock").activation_tokens.push(token);
		}
	}
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for App {
	fn event(
		_: &mut Self,
		proxy: &xdg_wm_base::XdgWmBase,
		event: xdg_wm_base::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		if let xdg_wm_base::Event::Ping { serial } = event {
			proxy.pong(serial);
		}
	}
}

impl Dispatch<xdg_surface::XdgSurface, ()> for App {
	fn event(
		state: &mut Self,
		proxy: &xdg_surface::XdgSurface,
		event: xdg_surface::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		if let xdg_surface::Event::Configure { serial } = event {
			proxy.ack_configure(serial);
			state.log.lock().expect("log lock").configure_acks += 1;
		}
	}
}

impl Dispatch<xdg_popup::XdgPopup, ()> for App {
	fn event(
		state: &mut Self,
		_: &xdg_popup::XdgPopup,
		event: xdg_popup::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		let mut log = state.log.lock().expect("log lock");
		match event {
			xdg_popup::Event::Configure { x, y, width, height } => log.popup_configure = Some((x, y, width, height)),
			xdg_popup::Event::PopupDone => log.popup_done += 1,
			_ => {},
		}
	}
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for App {
	fn event(
		state: &mut Self,
		proxy: &xdg_toplevel::XdgToplevel,
		event: xdg_toplevel::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		let xdg_toplevel::Event::Configure { width, height, states } = event else {
			return;
		};
		let has_state = |want: xdg_toplevel::State| {
			states
				.chunks_exact(4)
				.any(|chunk| u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) == u32::from(want))
		};
		let activated = has_state(xdg_toplevel::State::Activated);
		let fullscreen = has_state(xdg_toplevel::State::Fullscreen);
		let surface_id = state.surface_id_for_toplevel(&proxy.id());
		let mut log = state.log.lock().expect("log lock");
		if width > 0 && height > 0 {
			log.configure_size = (width, height);
		}
		if let Some(surface_id) = surface_id {
			if activated {
				log.activated.push(surface_id);
			}
			if fullscreen {
				log.fullscreen.push(surface_id);
			}
		}
	}
}

/// A client handle: the log the tests inspect plus control over the thread.
pub struct WaylandClient {
	log: Arc<Mutex<Log>>,
	ids: Arc<Ids>,
	commands: Option<mpsc::Sender<Command>>,
	join: Option<thread::JoinHandle<()>>,
}

impl WaylandClient {
	/// Connect, bind the globals, and map the initial toplevel(s).
	pub fn spawn(runtime_dir: &Path, display: &str, options: ClientOptions) -> Self {
		let log = Arc::new(Mutex::new(Log::default()));
		let ids = Arc::new(Ids::default());
		let (commands_tx, commands_rx) = mpsc::channel::<Command>();
		let (init_tx, init_rx) = mpsc::channel::<std::result::Result<(), String>>();

		let thread_log = Arc::clone(&log);
		let thread_ids = Arc::clone(&ids);
		let path = runtime_dir.join(display);
		let join = thread::Builder::new()
			.name("ferriscope-wayland-test-client".to_string())
			.spawn(move || run(path, thread_log, thread_ids, options, commands_rx, &init_tx))
			.expect("spawn wayland test client thread");

		match init_rx.recv_timeout(COMMAND_TIMEOUT) {
			Ok(Ok(())) => Self {
				log,
				ids,
				commands: Some(commands_tx),
				join: Some(join),
			},
			Ok(Err(error)) => panic!("wayland test client setup failed: {error}"),
			Err(_) => panic!("wayland test client did not report ready within {COMMAND_TIMEOUT:?}"),
		}
	}

	/// The `wl_surface` id currently mapped for `target`.
	pub fn surface_id(&self, target: Target) -> Option<u32> {
		self.ids.get(target)
	}

	/// A consistent snapshot of the event log.
	pub fn snapshot(&self) -> Log {
		self.log.lock().expect("log lock").clone()
	}

	/// Send one command and wait, bounded, for the client to service it.
	pub fn command(&self, kind: CommandKind) -> bool {
		let Some(commands) = &self.commands else {
			return false;
		};
		let (ack_tx, ack_rx) = mpsc::sync_channel(0);
		if commands.send(Command { kind, ack: ack_tx }).is_err() {
			return false;
		}
		ack_rx.recv_timeout(COMMAND_TIMEOUT).is_ok()
	}

	/// Stop the client thread, bounded, without hanging the test.
	pub fn shutdown(&mut self) {
		self.commands.take();
		if let Some(join) = self.join.take() {
			let (done_tx, done_rx) = mpsc::channel();
			thread::spawn(move || {
				let _ = done_tx.send(join.join());
			});
			// If the client wedges, detach rather than hang the harness.
			let _ = done_rx.recv_timeout(STOP_TIMEOUT);
		}
	}
}

impl Drop for WaylandClient {
	fn drop(&mut self) {
		self.shutdown();
	}
}

/// The client thread: set up, then service commands and dispatch events.
fn run(
	path: PathBuf,
	log: Arc<Mutex<Log>>,
	ids: Arc<Ids>,
	options: ClientOptions,
	commands: mpsc::Receiver<Command>,
	init: &mpsc::Sender<std::result::Result<(), String>>,
) {
	let result = run_inner(path, log, ids, options, commands, init);
	if let Err(error) = result {
		// After a successful setup this is a post-ready exit; the command
		// channel closing is what the tests observe.
		let _ = init.send(Err(error));
	}
}

fn run_inner(
	path: PathBuf,
	log: Arc<Mutex<Log>>,
	ids: Arc<Ids>,
	options: ClientOptions,
	commands: mpsc::Receiver<Command>,
	init: &mpsc::Sender<std::result::Result<(), String>>,
) -> std::result::Result<(), String> {
	let stream = UnixStream::connect(&path).map_err(|e| format!("connect {}: {e}", path.display()))?;
	let connection = Connection::from_socket(stream).map_err(|e| format!("wayland connection: {e}"))?;
	let (globals, mut queue) = registry_queue_init::<App>(&connection).map_err(|e| format!("registry: {e}"))?;
	let qh = queue.handle();

	let bind = |qhandle: &wayland_client::QueueHandle<App>| -> std::result::Result<App, String> {
		let seat = bind_global::<wl_seat::WlSeat>(&globals, qhandle, 1..=5)?;
		let tablet_manager = bind_global::<
			wayland_protocols::wp::tablet::zv2::client::zwp_tablet_manager_v2::ZwpTabletManagerV2,
		>(&globals, qhandle, 1..=1)?;
		Ok(App {
			log: Arc::clone(&log),
			ids: Arc::clone(&ids),
			compositor: bind_global::<wl_compositor::WlCompositor>(&globals, qhandle, 1..=4)?,
			shm: bind_global::<wl_shm::WlShm>(&globals, qhandle, 1..=1)?,
			wm_base: bind_global::<xdg_wm_base::XdgWmBase>(&globals, qhandle, 1..=1)?,
			_tablet_seat: tablet::bind_tablet_seat(&tablet_manager, &seat, qhandle),
			_seat: seat,
			pointer: None,
			keyboard: None,
			touch: None,
			pointer_enter_serial: None,
			pointer_button_serial: None,
			popup: None,
			relative_manager: bind_global::<zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1>(
				&globals,
				qhandle,
				1..=1,
			)?,
			pointer_constraints: bind_global::<zwp_pointer_constraints_v1::ZwpPointerConstraintsV1>(
				&globals,
				qhandle,
				1..=1,
			)?,
			activation: bind_global::<xdg_activation_v1::XdgActivationV1>(&globals, qhandle, 1..=1)?,
			parent: None,
			child: None,
			second: None,
			toplevel_ids: Vec::new(),
			_relative_pointer: None,
			_locked_pointer: None,
			_confine_region: None,
			_confined_pointer: None,
			_cursor_surface: None,
			_cursor_shm: None,
			buffer_size: options.buffer_size,
		})
	};
	let mut app = bind(&qh).map_err(|e| format!("bind globals: {e}"))?;
	// Collect seat capabilities (to create wl_pointer/wl_keyboard) and the
	// shm formats before creating any surface.
	queue
		.roundtrip(&mut app)
		.map_err(|e| format!("initial roundtrip: {e}"))?;

	app.create_toplevel(&mut queue, &qh, None, Target::Parent, options.buffer_size);
	if options.with_child {
		let parent = app.parent.as_ref().map(|parent| parent.toplevel.clone());
		app.create_toplevel(&mut queue, &qh, parent.as_ref(), Target::Child, options.buffer_size);
	}

	init.send(Ok(())).ok();

	loop {
		match commands.try_recv() {
			Ok(command) => {
				app.handle(command.kind, &mut queue, &qh);
				let _ = command.ack.send(());
			},
			Err(mpsc::TryRecvError::Empty) => {},
			Err(mpsc::TryRecvError::Disconnected) => break,
		}
		if queue.roundtrip(&mut app).is_err() {
			break;
		}
	}
	Ok(())
}

/// Bind a global, naming it on failure.
fn bind_global<I>(
	globals: &wayland_client::globals::GlobalList,
	qh: &wayland_client::QueueHandle<App>,
	version: std::ops::RangeInclusive<u32>,
) -> std::result::Result<I, String>
where
	I: wayland_client::Proxy + 'static,
	App: wayland_client::Dispatch<I, ()>,
{
	globals
		.bind::<I, App, ()>(qh, version, ())
		.map_err(|e| format!("bind {}: {e}", I::interface().name))
}

/// Create an anonymous, size-`size`, shared-memory file filled with the
/// little-endian 32-bit word `fill` (a tail shorter than four bytes stays
/// zero). The cursor image must be opaque, or the composite draws nothing.
fn shm_file(size: usize, fill: u32) -> std::io::Result<OwnedFd> {
	let path = std::env::temp_dir().join(format!(
		"ferriscope-wayland-shm-{}-{}",
		std::process::id(),
		unique_nanos()
	));
	let mut file = std::fs::OpenOptions::new()
		.read(true)
		.write(true)
		.create_new(true)
		.open(&path)?;
	file.set_len(size as u64)?;
	let _ = std::fs::remove_file(&path);
	let mut pixels = Vec::with_capacity(size);
	while pixels.len() + 4 <= size {
		pixels.extend_from_slice(&fill.to_le_bytes());
	}
	pixels.resize(size, 0);
	use std::io::Write;
	file.write_all(&pixels)?;
	Ok(file.into())
}

/// Nanoseconds since the epoch, for unique temp names.
fn unique_nanos() -> u128 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|duration| duration.as_nanos())
		.unwrap_or(0)
}

/// A launched compositor plus its control socket, killed on drop.
pub struct FocusHarness {
	compositor: Option<Compositor>,
	socket_path: PathBuf,
}

impl FocusHarness {
	/// Stop the compositor under a watchdog. The client must be shut down
	/// first; a failed assertion must not leak a compositor thread.
	pub fn stop(&mut self) -> bool {
		let (status_tx, status_rx) = mpsc::channel();
		let compositor = self.compositor.take();
		thread::spawn(move || {
			let result = match compositor {
				Some(compositor) => compositor.stop(),
				None => Ok(()),
			};
			let _ = status_tx.send(result);
		});
		let stopped = status_rx
			.recv_timeout(STOP_TIMEOUT)
			.map(|result| result.is_ok())
			.unwrap_or(false);
		let _ = std::fs::remove_file(&self.socket_path);
		stopped
	}
}

impl Drop for FocusHarness {
	fn drop(&mut self) {
		let _ = self.stop();
	}
}

/// Everything a test needs: compositor, client and the control socket.
pub struct Started {
	pub guard: FocusHarness,
	pub client: WaylandClient,
	pub control: UnixStream,
	/// The compositor's Wayland socket path, for a test's own extra client.
	pub wayland_socket: PathBuf,
	// Keep the consumer endpoints alive: dropping them disconnects the sink
	// and the compositor would stop mid-test.
	pub _handles: CompositorHandles,
}

/// Launch the compositor (no Xwayland), connect the client, accept input.
pub fn start(options: ClientOptions) -> Started {
	start_with_record(options, None, None)
}

/// Launch as [`start`], but optionally record CPU PPM frames into
/// `record_dir` through the in-process `Compositor::launch` path.
///
/// With a recorder the frame mailbox never carries a frame (see
/// `FrameOutput::Recorder`); `input` and `ready` still work, so
/// tests can drive the client and read frames from disk. `record_max_frames`
/// bounds disk use.
pub fn start_with_record(
	options: ClientOptions,
	record_dir: Option<PathBuf>,
	record_max_frames: Option<u64>,
) -> Started {
	start_configured(options, |config| {
		config.output = ferriscope::FrameOutput::Recorder(ferriscope::RecorderOutput {
			dir: record_dir,
			max_frames: record_max_frames,
			..ferriscope::RecorderOutput::default()
		});
	})
}

/// The channel output of a config that has not been switched to a recorder.
pub fn channel(config: &mut CompositorConfig) -> &mut ferriscope::ChannelOutput {
	match &mut config.output {
		ferriscope::FrameOutput::Channel(channel) => channel,
		_ => panic!("the config does not use the channel output"),
	}
}

/// Launch as [`start`], letting the test adjust the compositor config.
pub fn start_configured(options: ClientOptions, configure: impl FnOnce(&mut CompositorConfig)) -> Started {
	let socket_path = unique_socket_path();
	let _ = std::fs::remove_file(&socket_path);
	let listener = UnixListener::bind(&socket_path).expect("bind control listener");

	let mut config = CompositorConfig {
		width: WIDTH,
		height: HEIGHT,
		xwayland: false,
		command: None,
		control_socket: Some(socket_path.clone()),
		..CompositorConfig::default()
	};
	configure(&mut config);
	let (compositor, handles) = Compositor::launch(config).expect("launch compositor");
	let guard = FocusHarness {
		compositor: Some(compositor),
		socket_path,
	};

	let ready: Ready = handles
		.ready
		.recv_timeout(READY_TIMEOUT)
		.expect("ready handshake within timeout");
	let control = accept_control(&listener);
	let runtime_dir = std::env::var("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR is set");
	let wayland_socket = Path::new(&runtime_dir).join(&ready.wayland_display);
	let client = WaylandClient::spawn(Path::new(&runtime_dir), &ready.wayland_display, options);

	Started {
		guard,
		client,
		control,
		wayland_socket,
		_handles: handles,
	}
}

/// Accept the compositor's control connection, bounded.
pub fn accept_control(listener: &UnixListener) -> UnixStream {
	listener.set_nonblocking(true).expect("listener nonblocking");
	let deadline = Instant::now() + ACCEPT_TIMEOUT;
	loop {
		match listener.accept() {
			Ok((stream, _)) => return stream,
			Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {},
			Err(e) => panic!("accept control connection: {e}"),
		}
		assert!(
			Instant::now() < deadline,
			"compositor did not connect to the control socket within {ACCEPT_TIMEOUT:?}"
		);
		thread::sleep(POLL_SLICE);
	}
}

/// Send one control line and give the compositor a beat to inject it.
pub fn send_line(stream: &mut UnixStream, line: &str) {
	use std::io::Write;
	stream
		.write_all(line.as_bytes())
		.unwrap_or_else(|e| panic!("write control line {line:?}: {e}"));
	stream
		.flush()
		.unwrap_or_else(|e| panic!("flush control line {line:?}: {e}"));
	thread::sleep(CONTROL_GAP);
}

/// Wait, bounded, for `predicate` to hold on a fresh log snapshot.
pub fn wait_log(client: &WaylandClient, timeout: Duration, mut predicate: impl FnMut(&Log) -> bool) -> bool {
	let deadline = Instant::now() + timeout;
	loop {
		if predicate(&client.snapshot()) {
			return true;
		}
		if Instant::now() >= deadline {
			return false;
		}
		thread::sleep(POLL_SLICE);
	}
}

/// A unique control-socket path under the temp dir.
pub fn unique_socket_path() -> PathBuf {
	std::env::temp_dir().join(format!(
		"ferriscope-wayland-focus-{}-{}.sock",
		std::process::id(),
		unique_nanos()
	))
}

/// Output width, small enough that the software render fallback keeps up.
const WIDTH: u32 = 320;
/// Output height.
const HEIGHT: u32 = 240;
/// The window buffer's opaque fill (AARRGGBB): a mid blue, deliberately not
/// black so a mapped-window check and a cursor diff are both easy to read.
const WINDOW_COLOR: u32 = 0xff20_4060;
/// The client cursor image side, in pixels.
const CURSOR_SIZE: i32 = 16;
/// The client cursor image's opaque fill (AARRGGBB): white, so it differs from
/// the window and from black by a large margin.
const CURSOR_COLOR: u32 = 0xffff_ffff;
/// Sleep between injected events, so each reaches the compositor alone.
const CONTROL_GAP: Duration = Duration::from_millis(120);
/// Poll slice while waiting on events.
const POLL_SLICE: Duration = Duration::from_millis(25);
