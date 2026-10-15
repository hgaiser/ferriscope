//! wl_compositor, wl_shm, dmabuf and output handlers.

use smithay::{
	backend::{
		allocator::dmabuf::Dmabuf,
		renderer::{ImportDma, utils::on_commit_buffer_handler},
	},
	reexports::wayland_server::protocol::{wl_buffer, wl_surface::WlSurface},
	wayland::{
		buffer::BufferHandler,
		compositor::{
			CompositorClientState, CompositorHandler, CompositorState, add_pre_commit_hook, is_sync_subsurface,
		},
		dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
		output::OutputHandler,
		shm::{ShmHandler, ShmState},
	},
};

use crate::shell::ClientState;
use crate::shell::FerriscopeState;
use crate::shell::content::CommitRoute;
use crate::shell::geometry::root_surface;

impl CompositorHandler for FerriscopeState {
	fn compositor_state(&mut self) -> &mut CompositorState {
		&mut self.compositor_state
	}

	fn client_compositor_state<'a>(
		&self,
		client: &'a smithay::reexports::wayland_server::Client,
	) -> &'a CompositorClientState {
		if let Some(state) = client.get_data::<smithay::xwayland::XWaylandClientData>() {
			return &state.compositor_state;
		}
		if let Some(state) = client.get_data::<ClientState>() {
			return &state.compositor_state;
		}
		panic!("unknown client data type")
	}

	fn new_surface(&mut self, surface: &WlSurface) {
		crate::shell::sync::install_commit_gate(surface);
		add_pre_commit_hook::<Self, _>(surface, |_, _, surface| crate::shell::fifo::gate_wsi_fifo(surface));
	}

	fn commit(&mut self, surface: &WlSurface) {
		on_commit_buffer_handler::<Self>(surface);
		self.commit_popup(surface);
		let now = std::time::Instant::now();
		if is_sync_subsurface(surface) {
			self.scheduler.commit(now);
			return;
		}
		let root = root_surface(surface);
		match self.route_commit(&root) {
			// The window has an override surface and this is not content it takes
			// (`update_wayland_res`, `steamcompmgr.cpp:8288`): nothing changed.
			CommitRoute::Ignored => {},
			CommitRoute::Unrouted => self.scheduler.commit(now),
			CommitRoute::Window(id) => {
				// Latency tooling: a window's content commit was applied.
				tracing::trace!(target: "ferriscope::latency", window = id, "commit");
				self.scheduler.commit(now);
				if let Some(window) = self.windows.window(id) {
					window.on_commit();
				}
				// A buffer committed to a tracked window: it can now be presented, and
				// its damage sequence may change who the best window is.
				self.note_commit(id, &root);
				self.trace_content_colour(&root);
			},
		}
	}
}

impl BufferHandler for FerriscopeState {
	fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for FerriscopeState {
	fn shm_state(&self) -> &ShmState {
		&self.shm_state
	}
}

impl DmabufHandler for FerriscopeState {
	fn dmabuf_state(&mut self) -> &mut DmabufState {
		&mut self.dmabuf_state
	}

	fn dmabuf_imported(&mut self, _global: &DmabufGlobal, dmabuf: Dmabuf, notifier: ImportNotifier) {
		if self.renderer.import_dmabuf(&dmabuf, None).is_ok() {
			let _ = notifier.successful::<Self>();
		} else {
			notifier.failed();
		}
	}
}

impl OutputHandler for FerriscopeState {}
