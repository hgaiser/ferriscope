//! GPU integration tests for the colour management protocols.
//!
//! `wp_color_management_v1` and gamescope's `frog_color_management_v1` are how
//! DXVK, vkd3d-proton (through Mesa's Wayland WSI) and mpv declare a surface's
//! colour space. These tests drive the protocol as such a client would and
//! check what the compositor advertises, accepts and rejects. What the
//! declaration does to the frames is covered by `hdr_output`.
//!
//! They are `#[ignore]`d: they need a render node. Run them alone:
//!
//! ```text
//! cargo test -p ferriscope-core --test colour_management -- --ignored --test-threads=1 --nocapture
//! ```

use std::{
	os::unix::net::UnixStream,
	path::{Path, PathBuf},
	time::Duration,
};

use ferriscope_core::{Compositor, CompositorConfig, CompositorHandles, Ready};
use wayland_client::{
	Connection, Dispatch, QueueHandle, WEnum, delegate_noop,
	globals::{GlobalList, GlobalListContents, registry_queue_init},
	protocol::{wl_compositor, wl_registry, wl_surface},
};
use wayland_protocols::wp::color_management::v1::client::{
	wp_color_management_output_v1, wp_color_management_surface_feedback_v1, wp_color_management_surface_v1,
	wp_color_manager_v1::{self, Feature, Primaries, RenderIntent, TransferFunction, WpColorManagerV1},
	wp_image_description_creator_params_v1::{self, WpImageDescriptionCreatorParamsV1},
	wp_image_description_info_v1::{self, WpImageDescriptionInfoV1},
	wp_image_description_v1::{self, WpImageDescriptionV1},
};

mod frog {
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

	use wayland_client::{self, protocol::*};

	pub mod __interfaces {
		use wayland_client::protocol::__interfaces::*;
		wayland_scanner::generate_interfaces!("protocol/frog-color-management-v1.xml");
	}
	use self::__interfaces::*;

	wayland_scanner::generate_client_code!("protocol/frog-color-management-v1.xml");
}

use frog::{frog_color_managed_surface, frog_color_management_factory_v1};

const READY_TIMEOUT: Duration = Duration::from_secs(30);

struct Session {
	compositor: Option<Compositor>,
	_handles: CompositorHandles,
	ready: Ready,
	record_dir: PathBuf,
}

impl Session {
	fn start(hdr: bool) -> Self {
		let nanos = std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.map(|d| d.as_nanos())
			.unwrap_or(0);
		let record_dir = std::env::temp_dir().join(format!("ferriscope-colour-{}-{nanos}", std::process::id()));
		let config = CompositorConfig {
			width: 320,
			height: 240,
			hdr,
			output: ferriscope_core::FrameOutput::Recorder(ferriscope_core::RecorderOutput {
				dir: Some(record_dir.clone()),
				max_frames: Some(1),
				..ferriscope_core::RecorderOutput::default()
			}),
			..CompositorConfig::default()
		};
		let (compositor, handles) = Compositor::launch(config).expect("launch compositor");
		let ready = handles.ready.recv_timeout(READY_TIMEOUT).expect("ready handshake");
		Self {
			compositor: Some(compositor),
			_handles: handles,
			ready,
			record_dir,
		}
	}

	fn client(&self) -> Client {
		let runtime_dir = std::env::var("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR is set");
		let stream = UnixStream::connect(Path::new(&runtime_dir).join(&self.ready.wayland_display))
			.expect("connect to the compositor socket");
		Client::new(Connection::from_socket(stream).expect("wayland connection"))
	}
}

impl Drop for Session {
	fn drop(&mut self) {
		if let Some(compositor) = self.compositor.take() {
			let _ = compositor.stop();
		}
		let _ = std::fs::remove_dir_all(&self.record_dir);
	}
}

/// What the client heard.
#[derive(Default)]
struct Heard {
	features: Vec<WEnum<Feature>>,
	transfer_functions: Vec<WEnum<TransferFunction>>,
	primaries: Vec<WEnum<Primaries>>,
	manager_done: bool,
	ready: Vec<u32>,
	failed: Vec<String>,
	info: Vec<String>,
	info_done: bool,
	frog_preferred: Option<(WEnum<frog_color_managed_surface::TransferFunction>, u32, u32)>,
}

struct State {
	heard: Heard,
}

struct Client {
	_conn: Connection,
	queue: wayland_client::EventQueue<State>,
	state: State,
	globals: GlobalList,
	compositor: wl_compositor::WlCompositor,
}

impl Client {
	fn new(conn: Connection) -> Self {
		let (globals, mut queue) = registry_queue_init::<State>(&conn).expect("registry");
		let qh = queue.handle();
		let compositor = globals.bind(&qh, 1..=4, ()).expect("wl_compositor");
		let mut state = State {
			heard: Heard::default(),
		};
		queue.roundtrip(&mut state).expect("roundtrip");
		Self {
			_conn: conn,
			queue,
			state,
			globals,
			compositor,
		}
	}

	fn qh(&self) -> QueueHandle<State> {
		self.queue.handle()
	}

	fn roundtrip(&mut self) -> Result<usize, wayland_client::DispatchError> {
		self.queue.roundtrip(&mut self.state)
	}

	fn manager(&mut self) -> WpColorManagerV1 {
		let qh = self.qh();
		let manager: WpColorManagerV1 = self.globals.bind(&qh, 1..=3, ()).expect("wp_color_manager_v1");
		self.roundtrip().expect("roundtrip");
		manager
	}

	fn surface(&self) -> wl_surface::WlSurface {
		self.compositor.create_surface(&self.qh(), ())
	}
}

/// The protocol error code the compositor killed the connection with.
fn protocol_error_code(result: Result<usize, wayland_client::DispatchError>) -> u32 {
	match result {
		Err(wayland_client::DispatchError::Backend(wayland_client::backend::WaylandError::Protocol(error))) => {
			error.code
		},
		other => panic!("expected a protocol error, got {other:?}"),
	}
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

impl Dispatch<WpColorManagerV1, ()> for State {
	fn event(
		state: &mut Self,
		_: &WpColorManagerV1,
		event: wp_color_manager_v1::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		match event {
			wp_color_manager_v1::Event::SupportedFeature { feature } => state.heard.features.push(feature),
			wp_color_manager_v1::Event::SupportedTfNamed { tf } => state.heard.transfer_functions.push(tf),
			wp_color_manager_v1::Event::SupportedPrimariesNamed { primaries } => state.heard.primaries.push(primaries),
			wp_color_manager_v1::Event::Done => state.heard.manager_done = true,
			_ => {},
		}
	}
}

impl Dispatch<WpImageDescriptionV1, ()> for State {
	fn event(
		state: &mut Self,
		_: &WpImageDescriptionV1,
		event: wp_image_description_v1::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		match event {
			wp_image_description_v1::Event::Ready { identity } => state.heard.ready.push(identity),
			wp_image_description_v1::Event::Ready2 {
				identity_hi,
				identity_lo,
			} => state.heard.ready.push(identity_lo.max(identity_hi)),
			wp_image_description_v1::Event::Failed { msg, .. } => state.heard.failed.push(msg),
			_ => {},
		}
	}
}

impl Dispatch<WpImageDescriptionInfoV1, ()> for State {
	fn event(
		state: &mut Self,
		_: &WpImageDescriptionInfoV1,
		event: wp_image_description_info_v1::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		match event {
			wp_image_description_info_v1::Event::TfNamed { tf } => state.heard.info.push(format!("tf {tf:?}")),
			wp_image_description_info_v1::Event::PrimariesNamed { primaries } => {
				state.heard.info.push(format!("primaries {primaries:?}"))
			},
			wp_image_description_info_v1::Event::Luminances {
				min_lum,
				max_lum,
				reference_lum,
			} => state
				.heard
				.info
				.push(format!("luminances {min_lum} {max_lum} {reference_lum}")),
			wp_image_description_info_v1::Event::Done => state.heard.info_done = true,
			_ => {},
		}
	}
}

impl Dispatch<frog_color_managed_surface::FrogColorManagedSurface, ()> for State {
	fn event(
		state: &mut Self,
		_: &frog_color_managed_surface::FrogColorManagedSurface,
		event: frog_color_managed_surface::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		let frog_color_managed_surface::Event::PreferredMetadata {
			transfer_function,
			max_luminance,
			min_luminance,
			..
		} = event;
		state.heard.frog_preferred = Some((transfer_function, max_luminance, min_luminance));
	}
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore wp_color_management_surface_v1::WpColorManagementSurfaceV1);
delegate_noop!(State: ignore wp_color_management_output_v1::WpColorManagementOutputV1);
delegate_noop!(State: ignore wp_color_management_surface_feedback_v1::WpColorManagementSurfaceFeedbackV1);
delegate_noop!(State: ignore WpImageDescriptionCreatorParamsV1);
delegate_noop!(State: ignore frog_color_management_factory_v1::FrogColorManagementFactoryV1);

/// A parametric creator with the given named transfer function and primaries.
fn creator(
	client: &mut Client,
	manager: &WpColorManagerV1,
	tf: TransferFunction,
	primaries: Primaries,
) -> WpImageDescriptionCreatorParamsV1 {
	let qh = client.qh();
	let creator = manager.create_parametric_creator(&qh, ());
	creator.set_tf_named(tf);
	creator.set_primaries_named(primaries);
	creator
}

#[test]
#[ignore = "requires GPU"]
fn hdr_encodings_are_advertised_only_when_hdr_is_enabled() {
	for hdr in [false, true] {
		let session = Session::start(hdr);
		let mut client = session.client();
		let _manager = client.manager();
		let heard = &client.state.heard;
		assert!(heard.manager_done, "the manager sent no done event");
		assert!(heard.features.contains(&WEnum::Value(Feature::Parametric)));
		assert!(
			heard
				.features
				.contains(&WEnum::Value(Feature::SetMasteringDisplayPrimaries))
		);
		assert!(
			heard
				.transfer_functions
				.contains(&WEnum::Value(TransferFunction::Gamma22))
		);
		assert!(heard.primaries.contains(&WEnum::Value(Primaries::Srgb)));
		assert_eq!(
			heard
				.transfer_functions
				.contains(&WEnum::Value(TransferFunction::St2084Pq)),
			hdr
		);
		assert_eq!(
			heard
				.transfer_functions
				.contains(&WEnum::Value(TransferFunction::ExtLinear)),
			hdr
		);
		assert_eq!(heard.primaries.contains(&WEnum::Value(Primaries::Bt2020)), hdr);
		assert_eq!(heard.features.contains(&WEnum::Value(Feature::WindowsScrgb)), hdr);
	}
}

#[test]
#[ignore = "requires GPU"]
fn parametric_descriptions_become_ready_or_fail_as_unsupported() {
	let session = Session::start(true);
	let mut client = session.client();
	let manager = client.manager();
	let qh = client.qh();

	// HDR10: PQ on BT.2020, with mastering metadata.
	let hdr10 = creator(&mut client, &manager, TransferFunction::St2084Pq, Primaries::Bt2020);
	hdr10.set_mastering_display_primaries(708_000, 292_000, 170_000, 797_000, 131_000, 46_000, 312_700, 329_000);
	hdr10.set_mastering_luminance(50, 1000);
	hdr10.set_max_cll(900);
	hdr10.set_max_fall(300);
	let _hdr10 = hdr10.create(&qh, ());
	// scRGB: extended linear on sRGB primaries.
	let scrgb = creator(&mut client, &manager, TransferFunction::ExtLinear, Primaries::Srgb);
	let _scrgb = scrgb.create(&qh, ());
	client.roundtrip().expect("roundtrip");
	assert_eq!(client.state.heard.ready.len(), 2);
	assert!(client.state.heard.failed.is_empty());

	// PQ on sRGB primaries cannot be carried.
	let odd = creator(&mut client, &manager, TransferFunction::St2084Pq, Primaries::Srgb);
	let _odd = odd.create(&qh, ());
	client.roundtrip().expect("roundtrip");
	assert_eq!(client.state.heard.ready.len(), 2);
	assert_eq!(client.state.heard.failed.len(), 1);

	// The predefined Windows descriptions are ready at once.
	let _scrgb = manager.create_windows_scrgb(&qh, ());
	let _bt2100 = manager.create_windows_bt2100(&qh, ());
	client.roundtrip().expect("roundtrip");
	assert_eq!(client.state.heard.ready.len(), 4);
}

#[test]
#[ignore = "requires GPU"]
fn an_incomplete_parameter_set_is_a_protocol_error() {
	let session = Session::start(true);
	let mut client = session.client();
	let manager = client.manager();
	let qh = client.qh();
	let creator = manager.create_parametric_creator(&qh, ());
	creator.set_tf_named(TransferFunction::St2084Pq);
	let _description = creator.create(&qh, ());
	let code = protocol_error_code(client.roundtrip());
	assert_eq!(
		code,
		wp_image_description_creator_params_v1::Error::IncompleteSet as u32
	);
}

#[test]
#[ignore = "requires GPU"]
fn setting_a_parameter_twice_is_a_protocol_error() {
	let session = Session::start(true);
	let mut client = session.client();
	let manager = client.manager();
	let creator = creator(&mut client, &manager, TransferFunction::St2084Pq, Primaries::Bt2020);
	creator.set_max_cll(1000);
	creator.set_max_cll(1000);
	let code = protocol_error_code(client.roundtrip());
	assert_eq!(code, wp_image_description_creator_params_v1::Error::AlreadySet as u32);
}

#[test]
#[ignore = "requires GPU"]
fn a_second_colour_management_object_for_a_surface_is_a_protocol_error() {
	let session = Session::start(true);
	let mut client = session.client();
	let manager = client.manager();
	let qh = client.qh();
	let surface = client.surface();
	let _first = manager.get_surface(&surface, &qh, ());
	client.roundtrip().expect("the first object is fine");
	let _second = manager.get_surface(&surface, &qh, ());
	let code = protocol_error_code(client.roundtrip());
	assert_eq!(code, wp_color_manager_v1::Error::SurfaceExists as u32);
}

#[test]
#[ignore = "requires GPU"]
fn an_unsupported_render_intent_is_a_protocol_error() {
	let session = Session::start(true);
	let mut client = session.client();
	let manager = client.manager();
	let qh = client.qh();
	let surface = client.surface();
	let colour_surface = manager.get_surface(&surface, &qh, ());
	let description = creator(&mut client, &manager, TransferFunction::St2084Pq, Primaries::Bt2020).create(&qh, ());
	colour_surface.set_image_description(&description, RenderIntent::Saturation);
	let code = protocol_error_code(client.roundtrip());
	assert_eq!(code, wp_color_management_surface_v1::Error::RenderIntent as u32);
}

#[test]
#[ignore = "requires GPU"]
fn the_preferred_description_follows_the_session() {
	for hdr in [false, true] {
		let session = Session::start(hdr);
		let mut client = session.client();
		let manager = client.manager();
		let qh = client.qh();
		let surface = client.surface();
		let feedback = manager.get_surface_feedback(&surface, &qh, ());
		let preferred = feedback.get_preferred(&qh, ());
		let _info = preferred.get_information(&qh, ());
		client.roundtrip().expect("roundtrip");
		assert_eq!(client.state.heard.ready.len(), 1);
		assert!(client.state.heard.info_done);
		let info = &client.state.heard.info;
		let (tf, primaries) = if hdr {
			("tf Value(St2084Pq)", "primaries Value(Bt2020)")
		} else {
			("tf Value(Gamma22)", "primaries Value(Srgb)")
		};
		assert!(info.iter().any(|line| line == tf), "{info:?}");
		assert!(info.iter().any(|line| line == primaries), "{info:?}");
	}
}

#[test]
#[ignore = "requires GPU"]
fn information_is_refused_for_a_client_created_description() {
	let session = Session::start(true);
	let mut client = session.client();
	let manager = client.manager();
	let qh = client.qh();
	let description = creator(&mut client, &manager, TransferFunction::St2084Pq, Primaries::Bt2020).create(&qh, ());
	let _info = description.get_information(&qh, ());
	let code = protocol_error_code(client.roundtrip());
	assert_eq!(code, wp_image_description_v1::Error::NoInformation as u32);
}

#[test]
#[ignore = "requires GPU"]
fn frog_surfaces_get_the_preferred_metadata() {
	for hdr in [false, true] {
		let session = Session::start(hdr);
		let mut client = session.client();
		let qh = client.qh();
		let factory: frog_color_management_factory_v1::FrogColorManagementFactoryV1 = client
			.globals
			.bind(&qh, 1..=1, ())
			.expect("frog_color_management_factory_v1");
		let surface = client.surface();
		let managed = factory.get_color_managed_surface(&surface, &qh, ());
		managed.set_known_transfer_function(frog_color_managed_surface::TransferFunction::St2084Pq);
		managed.set_known_container_color_volume(frog_color_managed_surface::Primaries::Rec2020);
		managed.set_hdr_metadata(
			34000, 16000, 13250, 34500, 7500, 3000, 15635, 16450, 1000, 50, 1000, 400,
		);
		surface.commit();
		client.roundtrip().expect("roundtrip");
		let (tf, max, min) = client.state.heard.frog_preferred.expect("preferred_metadata");
		if hdr {
			assert_eq!(tf, WEnum::Value(frog_color_managed_surface::TransferFunction::St2084Pq));
			assert_eq!((max, min), (1000, 50));
		} else {
			assert_eq!(tf, WEnum::Value(frog_color_managed_surface::TransferFunction::Gamma22));
		}
	}
}
