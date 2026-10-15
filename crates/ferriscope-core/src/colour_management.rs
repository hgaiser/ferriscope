//! Colour management protocols: `wp_color_management_v1` and gamescope's
//! `frog_color_management_v1`.
//!
//! Clients (DXVK and vkd3d-proton through Mesa's Wayland WSI, mpv, older
//! DXVK builds through the frog protocol) declare the encoding of a surface's
//! content: SDR, HDR10 PQ or scRGB, plus HDR mastering metadata. The
//! declaration is double-buffered surface state applied on `wl_surface.commit`
//! and read back with [`surface_colour`]; `hdr` combines it with the WSI
//! swapchain's report. ferriscope does not colour-manage: a surface is only
//! described, and the shell decides what the output carries.
//!
//! The compositor reports one preferred description for every surface and
//! output: HDR10 when HDR is enabled, sRGB otherwise. HDR features and
//! encodings are advertised only when HDR is enabled, so a client never
//! believes it can present HDR to an SDR session.
//!
//! The Wayland protocol is `wayland-protocols`' staging
//! `color-management-v1`; the frog protocol is vendored from gamescope
//! (`protocol/frog-color-management-v1.xml`).

use std::sync::{
	Mutex,
	atomic::{AtomicBool, AtomicU64, Ordering},
};

use smithay::{
	reexports::wayland_protocols::wp::color_management::v1::server::{
		wp_color_management_output_v1::{self, WpColorManagementOutputV1},
		wp_color_management_surface_feedback_v1::{self, WpColorManagementSurfaceFeedbackV1},
		wp_color_management_surface_v1::{self, WpColorManagementSurfaceV1},
		wp_color_manager_v1::{self, WpColorManagerV1},
		wp_image_description_creator_icc_v1::{self, WpImageDescriptionCreatorIccV1},
		wp_image_description_creator_params_v1::{self, WpImageDescriptionCreatorParamsV1},
		wp_image_description_info_v1::WpImageDescriptionInfoV1,
		wp_image_description_v1::{self, WpImageDescriptionV1},
	},
	reexports::wayland_server::protocol::wl_surface::WlSurface,
	wayland::{
		Dispatch2, GlobalDispatch2,
		compositor::{Cacheable, with_states},
	},
};
use wayland_server::{
	Client, DataInit, DisplayHandle, New, Resource, WEnum,
	backend::{ClientId, GlobalId},
};

use crate::{
	hdr::{ContentColour, ContentKind, ContentMetadata},
	shell::FerriscopeState,
	swapchain::{HdrMetadata, protocol_bindings},
};

protocol_bindings!(frog_protocol, "protocol/frog-color-management-v1.xml");

use frog_protocol::server::{
	frog_color_managed_surface::{self, FrogColorManagedSurface},
	frog_color_management_factory_v1::{self, FrogColorManagementFactoryV1},
};

/// Hands out a distinct identity per image description object.
static NEXT_IDENTITY: AtomicU64 = AtomicU64::new(1);

/// The transfer functions the compositor accepts for a parametric description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NamedTransfer {
	/// BT.1886, gamma 2.2 or the sRGB curve: all SDR.
	Sdr,
	/// Extended linear.
	ExtLinear,
	/// SMPTE ST 2084.
	Pq,
}

/// The named primaries the compositor accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NamedPrimaries {
	/// sRGB / BT.709.
	Srgb,
	/// BT.2020.
	Bt2020,
}

/// Why a parametric description cannot be created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BuildError {
	/// The transfer function or primaries were never set (`incomplete_set`).
	Incomplete,
	/// The light levels are inconsistent (`invalid_luminance`).
	InvalidLuminance,
}

/// The parameters collected by a `wp_image_description_creator_params_v1`.
#[derive(Debug, Default)]
pub(crate) struct ParamsBuilder {
	transfer: Option<NamedTransfer>,
	primaries: Option<NamedPrimaries>,
	luminances: Option<(u32, u32, u32)>,
	/// Red, green, blue and white `(x, y)` in 1e-6.
	mastering_primaries: Option<[(i32, i32); 4]>,
	/// `(min * 10000, max)` in cd/m2.
	mastering_luminance: Option<(u32, u32)>,
	max_cll: Option<u32>,
	max_fall: Option<u32>,
}

impl ParamsBuilder {
	/// Convert a protocol 1e-6 chromaticity to CTA-861's 0.00002 units.
	fn cta_chromaticity(value: i32) -> u32 {
		(value.max(0) / 20) as u32
	}

	/// Create the description, or `Ok(None)` for a combination the compositor
	/// cannot carry (reported as `failed` / `unsupported`).
	pub(crate) fn build(&self) -> Result<Option<ContentColour>, BuildError> {
		let (Some(transfer), Some(primaries)) = (self.transfer, self.primaries) else {
			return Err(BuildError::Incomplete);
		};
		if let (Some(max_cll), Some(max_fall)) = (self.max_cll, self.max_fall) {
			if max_fall > max_cll {
				return Err(BuildError::InvalidLuminance);
			}
		}
		let kind = match (transfer, primaries) {
			(NamedTransfer::Pq, NamedPrimaries::Bt2020) => ContentKind::Pq,
			(NamedTransfer::ExtLinear, NamedPrimaries::Srgb) => ContentKind::ScRgb,
			(NamedTransfer::Sdr, NamedPrimaries::Srgb) => ContentKind::Sdr,
			_ => return Ok(None),
		};
		let mut metadata = ContentMetadata {
			mastering_luminance: self.mastering_luminance,
			max_cll: self.max_cll,
			max_fall: self.max_fall,
			..ContentMetadata::default()
		};
		if let Some([red, green, blue, white]) = self.mastering_primaries {
			let convert = |(x, y): (i32, i32)| (Self::cta_chromaticity(x), Self::cta_chromaticity(y));
			metadata.mastering_primaries = Some([convert(red), convert(green), convert(blue)]);
			metadata.white_point = Some(convert(white));
		}
		if kind == ContentKind::Sdr {
			metadata = ContentMetadata::default();
		}
		Ok(Some(ContentColour { kind, metadata }))
	}
}

/// What a frog client declared; each part is optional and `undefined` until
/// set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct FrogParams {
	kind: Option<ContentKind>,
	metadata: Option<HdrMetadata>,
}

impl FrogParams {
	/// The declared colour, or `None` while the transfer function is
	/// undefined. Metadata gamescope considers garbage is dropped
	/// (`wlserver.cpp:1036`).
	fn colour(&self) -> Option<ContentColour> {
		let kind = self.kind?;
		let metadata = match self.metadata.filter(HdrMetadata::is_valid) {
			Some(metadata) if kind.is_hdr() => metadata.into(),
			_ => ContentMetadata::default(),
		};
		Some(ContentColour { kind, metadata })
	}
}

/// The double-buffered colour state of a surface.
#[derive(Debug, Clone, Default)]
struct SurfaceColour {
	/// `wp_color_management_surface_v1.set_image_description`.
	described: Option<ContentColour>,
	frog: FrogParams,
}

impl Cacheable for SurfaceColour {
	fn commit(&mut self, _dh: &DisplayHandle) -> Self {
		self.clone()
	}

	fn merge_into(self, into: &mut Self, _dh: &DisplayHandle) {
		*into = self;
	}
}

/// What the client declared for `surface` as of its last commit; `None` when
/// it declared nothing. An explicit sRGB description is `Some` and SDR.
pub(crate) fn surface_colour(surface: &WlSurface) -> Option<ContentColour> {
	with_states(surface, |states| {
		let mut cached = states.cached_state.get::<SurfaceColour>();
		let current = cached.current();
		current.described.or_else(|| current.frog.colour())
	})
}

/// Edit the pending colour state of `surface`.
fn edit_pending(surface: &WlSurface, edit: impl FnOnce(&mut SurfaceColour)) {
	with_states(surface, |states| {
		let mut cached = states.cached_state.get::<SurfaceColour>();
		edit(cached.pending());
	});
}

/// Marks a surface that already has a `wp_color_management_surface_v1`.
#[derive(Default)]
struct SurfaceExtension(AtomicBool);

/// The colour management globals, kept so they live as long as the compositor.
pub(crate) struct ColourManagementGlobals {
	_manager: GlobalId,
	_frog: GlobalId,
}

impl ColourManagementGlobals {
	/// Advertise `wp_color_manager_v1` and `frog_color_management_factory_v1`.
	/// `hdr` says whether the session carries HDR.
	pub(crate) fn new(display: &DisplayHandle, hdr: bool) -> Self {
		Self {
			_manager: display.create_global::<FerriscopeState, WpColorManagerV1, ManagerData>(3, ManagerData { hdr }),
			_frog: display.create_global::<FerriscopeState, FrogColorManagementFactoryV1, FrogFactoryData>(
				1,
				FrogFactoryData { hdr },
			),
		}
	}
}

/// User data of the manager global and object.
#[derive(Clone, Copy)]
pub(crate) struct ManagerData {
	hdr: bool,
}

/// User data of a `wp_color_management_surface_v1`.
pub(crate) struct SurfaceData {
	surface: WlSurface,
}

/// User data of a `wp_color_management_output_v1`.
pub(crate) struct OutputData {
	hdr: bool,
}

/// User data of a `wp_color_management_surface_feedback_v1`.
pub(crate) struct FeedbackData {
	hdr: bool,
}

/// User data of a `wp_image_description_creator_params_v1`.
pub(crate) struct ParamsData {
	builder: Mutex<ParamsBuilder>,
}

/// User data of a `wp_image_description_creator_icc_v1`, which is never
/// usable: ICC profiles are not a supported feature.
pub(crate) struct IccData;

/// User data of a `wp_image_description_v1`.
pub(crate) struct ImageDescriptionData {
	/// The described colour; `None` when creation failed.
	colour: Option<ContentColour>,
	/// Whether `get_information` is allowed: only for the compositor's
	/// preferred description, which says whether it is HDR.
	preferred: Option<bool>,
}

/// User data of a `wp_image_description_info_v1`.
pub(crate) struct InfoData;

/// User data of the frog factory global and object.
#[derive(Clone, Copy)]
pub(crate) struct FrogFactoryData {
	hdr: bool,
}

/// User data of a `frog_color_managed_surface`.
pub(crate) struct FrogSurfaceData {
	surface: WlSurface,
}

/// The colour the compositor prefers for every surface and output.
fn preferred_colour(hdr: bool) -> ContentColour {
	ContentColour {
		kind: if hdr { ContentKind::Pq } else { ContentKind::Sdr },
		metadata: ContentMetadata::default(),
	}
}

/// Send `ready` for a new image description.
fn send_ready(description: &WpImageDescriptionV1) {
	let identity = NEXT_IDENTITY.fetch_add(1, Ordering::Relaxed);
	if description.version() >= 2 {
		description.ready2((identity >> 32) as u32, identity as u32);
	} else {
		description.ready(identity as u32);
	}
}

/// Initialise an image description object for `colour` and report it ready,
/// or failed as `unsupported` when there is none.
fn init_description(
	data_init: &mut DataInit<'_, FerriscopeState>,
	new: New<WpImageDescriptionV1>,
	colour: Option<ContentColour>,
	preferred: Option<bool>,
) {
	let description = data_init.init(new, ImageDescriptionData { colour, preferred });
	match colour {
		Some(_) => send_ready(&description),
		None => description.failed(
			wp_image_description_v1::Cause::Unsupported,
			"unsupported combination of transfer function and primaries".to_string(),
		),
	}
}

/// CIE xy chromaticities of the named primaries and the D65 white point, x1e6.
const SRGB_PRIMARIES: [(i32, i32); 4] = [
	(640_000, 330_000),
	(300_000, 600_000),
	(150_000, 60_000),
	(312_700, 329_000),
];
const BT2020_PRIMARIES: [(i32, i32); 4] = [
	(708_000, 292_000),
	(170_000, 797_000),
	(131_000, 46_000),
	(312_700, 329_000),
];

/// Answer `get_information` for the preferred description. The caller ends it
/// with `done`, a destructor event that cannot be sent before the request
/// handler returns and the new object is initialised.
fn send_information(info: &WpImageDescriptionInfoV1, hdr: bool) {
	use wp_color_manager_v1::{Primaries, TransferFunction};
	let (named, primaries, transfer, luminances, target) = if hdr {
		(
			Primaries::Bt2020,
			BT2020_PRIMARIES,
			TransferFunction::St2084Pq,
			// Minimum 0.005 cd/m2, PQ's 10000 cd/m2 range, 203 cd/m2 reference white.
			(50, 10_000, 203),
			(50, 1000),
		)
	} else {
		(
			Primaries::Srgb,
			SRGB_PRIMARIES,
			TransferFunction::Gamma22,
			(2000, 80, 80),
			(2000, 80),
		)
	};
	let [red, green, blue, white] = primaries;
	info.primaries_named(named);
	info.primaries(red.0, red.1, green.0, green.1, blue.0, blue.1, white.0, white.1);
	info.tf_named(transfer);
	info.luminances(luminances.0, luminances.1, luminances.2);
	info.target_primaries(red.0, red.1, green.0, green.1, blue.0, blue.1, white.0, white.1);
	info.target_luminance(target.0, target.1);
}

impl GlobalDispatch2<WpColorManagerV1, FerriscopeState> for ManagerData {
	fn bind(
		&self,
		_state: &mut FerriscopeState,
		_handle: &DisplayHandle,
		_client: &Client,
		resource: New<WpColorManagerV1>,
		data_init: &mut DataInit<'_, FerriscopeState>,
	) {
		use wp_color_manager_v1::{Feature, Primaries, RenderIntent, TransferFunction};
		let manager = data_init.init(resource, *self);
		manager.supported_intent(RenderIntent::Perceptual);
		manager.supported_feature(Feature::Parametric);
		manager.supported_feature(Feature::SetLuminances);
		manager.supported_feature(Feature::SetMasteringDisplayPrimaries);
		manager.supported_feature(Feature::ExtendedTargetVolume);
		manager.supported_tf_named(TransferFunction::Gamma22);
		manager.supported_tf_named(TransferFunction::Bt1886);
		manager.supported_tf_named(TransferFunction::Srgb);
		manager.supported_primaries_named(Primaries::Srgb);
		if self.hdr {
			manager.supported_feature(Feature::WindowsScrgb);
			if manager.version() >= 3 {
				manager.supported_feature(Feature::WindowsBt2100);
			}
			manager.supported_tf_named(TransferFunction::St2084Pq);
			manager.supported_tf_named(TransferFunction::ExtLinear);
			manager.supported_primaries_named(Primaries::Bt2020);
		}
		manager.done();
	}
}

impl Dispatch2<WpColorManagerV1, FerriscopeState> for ManagerData {
	fn request(
		&self,
		_state: &mut FerriscopeState,
		_client: &Client,
		resource: &WpColorManagerV1,
		request: wp_color_manager_v1::Request,
		_handle: &DisplayHandle,
		data_init: &mut DataInit<'_, FerriscopeState>,
	) {
		use wp_color_manager_v1::{Error, Request};
		match request {
			Request::Destroy => {},
			Request::GetOutput { id, .. } => {
				data_init.init(id, OutputData { hdr: self.hdr });
			},
			Request::GetSurface { id, surface } => {
				let first = with_states(&surface, |states| {
					states.data_map.insert_if_missing_threadsafe(SurfaceExtension::default);
					let marker = states.data_map.get::<SurfaceExtension>().expect("inserted just above");
					!marker.0.swap(true, Ordering::AcqRel)
				});
				if !first {
					resource.post_error(
						Error::SurfaceExists,
						"the surface already has a colour management object",
					);
				}
				data_init.init(id, SurfaceData { surface });
			},
			Request::GetSurfaceFeedback { id, .. } => {
				data_init.init(id, FeedbackData { hdr: self.hdr });
			},
			Request::CreateIccCreator { obj } => {
				data_init.init(obj, IccData);
				resource.post_error(Error::UnsupportedFeature, "ICC profiles are not supported");
			},
			Request::CreateParametricCreator { obj } => {
				data_init.init(
					obj,
					ParamsData {
						builder: Mutex::new(ParamsBuilder::default()),
					},
				);
			},
			Request::CreateWindowsScrgb { image_description } => {
				if self.hdr {
					let colour = ContentColour {
						kind: ContentKind::ScRgb,
						metadata: ContentMetadata::default(),
					};
					init_description(data_init, image_description, Some(colour), None);
				} else {
					data_init.init(
						image_description,
						ImageDescriptionData {
							colour: None,
							preferred: None,
						},
					);
					resource.post_error(Error::UnsupportedFeature, "windows_scrgb is not supported");
				}
			},
			Request::CreateWindowsBt2100 { image_description } => {
				if self.hdr {
					let colour = ContentColour {
						kind: ContentKind::Pq,
						metadata: ContentMetadata::default(),
					};
					init_description(data_init, image_description, Some(colour), None);
				} else {
					data_init.init(
						image_description,
						ImageDescriptionData {
							colour: None,
							preferred: None,
						},
					);
					resource.post_error(Error::UnsupportedFeature, "windows_bt2100 is not supported");
				}
			},
			// `get_image_description` from a reference: references are never
			// handed out.
			Request::GetImageDescription { image_description, .. } => {
				init_description(data_init, image_description, None, None);
			},
			_ => {},
		}
	}
}

impl Dispatch2<WpColorManagementOutputV1, FerriscopeState> for OutputData {
	fn request(
		&self,
		_state: &mut FerriscopeState,
		_client: &Client,
		_resource: &WpColorManagementOutputV1,
		request: wp_color_management_output_v1::Request,
		_handle: &DisplayHandle,
		data_init: &mut DataInit<'_, FerriscopeState>,
	) {
		match request {
			wp_color_management_output_v1::Request::Destroy => {},
			wp_color_management_output_v1::Request::GetImageDescription { image_description } => {
				init_description(
					data_init,
					image_description,
					Some(preferred_colour(self.hdr)),
					Some(self.hdr),
				);
			},
			_ => {},
		}
	}
}

impl Dispatch2<WpColorManagementSurfaceV1, FerriscopeState> for SurfaceData {
	fn request(
		&self,
		_state: &mut FerriscopeState,
		_client: &Client,
		resource: &WpColorManagementSurfaceV1,
		request: wp_color_management_surface_v1::Request,
		_handle: &DisplayHandle,
		_data_init: &mut DataInit<'_, FerriscopeState>,
	) {
		use wp_color_management_surface_v1::{Error, Request};
		match request {
			Request::Destroy => {},
			Request::SetImageDescription {
				image_description,
				render_intent,
			} => {
				if !matches!(
					render_intent,
					WEnum::Value(wp_color_manager_v1::RenderIntent::Perceptual)
				) {
					resource.post_error(Error::RenderIntent, "unsupported render intent");
					return;
				}
				let colour = image_description
					.data::<ImageDescriptionData>()
					.and_then(|data| data.colour);
				match colour {
					Some(colour) => edit_pending(&self.surface, |state| state.described = Some(colour)),
					None => resource.post_error(Error::ImageDescription, "the image description is not ready"),
				}
			},
			Request::UnsetImageDescription => edit_pending(&self.surface, |state| state.described = None),
			_ => {},
		}
	}

	fn destroyed(&self, _state: &mut FerriscopeState, _client: ClientId, _resource: &WpColorManagementSurfaceV1) {
		// Destroying the extension unsets the description on the next commit
		// and lets the surface have another one.
		edit_pending(&self.surface, |state| state.described = None);
		with_states(&self.surface, |states| {
			if let Some(marker) = states.data_map.get::<SurfaceExtension>() {
				marker.0.store(false, Ordering::Release);
			}
		});
	}
}

impl Dispatch2<WpColorManagementSurfaceFeedbackV1, FerriscopeState> for FeedbackData {
	fn request(
		&self,
		_state: &mut FerriscopeState,
		_client: &Client,
		_resource: &WpColorManagementSurfaceFeedbackV1,
		request: wp_color_management_surface_feedback_v1::Request,
		_handle: &DisplayHandle,
		data_init: &mut DataInit<'_, FerriscopeState>,
	) {
		use wp_color_management_surface_feedback_v1::Request;
		match request {
			Request::Destroy => {},
			Request::GetPreferred { image_description } | Request::GetPreferredParametric { image_description } => {
				init_description(
					data_init,
					image_description,
					Some(preferred_colour(self.hdr)),
					Some(self.hdr),
				);
			},
			_ => {},
		}
	}
}

impl Dispatch2<WpImageDescriptionCreatorIccV1, FerriscopeState> for IccData {
	fn request(
		&self,
		_state: &mut FerriscopeState,
		_client: &Client,
		_resource: &WpImageDescriptionCreatorIccV1,
		_request: wp_image_description_creator_icc_v1::Request,
		_handle: &DisplayHandle,
		_data_init: &mut DataInit<'_, FerriscopeState>,
	) {
	}
}

impl Dispatch2<WpImageDescriptionCreatorParamsV1, FerriscopeState> for ParamsData {
	fn request(
		&self,
		_state: &mut FerriscopeState,
		_client: &Client,
		resource: &WpImageDescriptionCreatorParamsV1,
		request: wp_image_description_creator_params_v1::Request,
		_handle: &DisplayHandle,
		data_init: &mut DataInit<'_, FerriscopeState>,
	) {
		use wp_image_description_creator_params_v1::{Error, Request};
		let mut builder = self.builder.lock().unwrap();
		match request {
			Request::Create { image_description } => match builder.build() {
				Ok(colour) => init_description(data_init, image_description, colour, None),
				Err(error) => {
					data_init.init(
						image_description,
						ImageDescriptionData {
							colour: None,
							preferred: None,
						},
					);
					match error {
						BuildError::Incomplete => {
							resource.post_error(Error::IncompleteSet, "transfer function or primaries not set")
						},
						BuildError::InvalidLuminance => {
							resource.post_error(Error::InvalidLuminance, "max_fall exceeds max_cll")
						},
					}
				},
			},
			Request::SetTfNamed { tf } => {
				use wp_color_manager_v1::TransferFunction;
				if builder.transfer.is_some() {
					resource.post_error(Error::AlreadySet, "transfer function already set");
					return;
				}
				builder.transfer = match tf {
					WEnum::Value(TransferFunction::Gamma22 | TransferFunction::Bt1886 | TransferFunction::Srgb) => {
						Some(NamedTransfer::Sdr)
					},
					WEnum::Value(TransferFunction::St2084Pq) => Some(NamedTransfer::Pq),
					WEnum::Value(TransferFunction::ExtLinear) => Some(NamedTransfer::ExtLinear),
					_ => {
						resource.post_error(Error::InvalidTf, "unsupported transfer function");
						None
					},
				};
			},
			Request::SetTfPower { .. } | Request::SetPrimaries { .. } => {
				resource.post_error(Error::UnsupportedFeature, "feature not supported");
			},
			Request::SetPrimariesNamed { primaries } => {
				use wp_color_manager_v1::Primaries;
				if builder.primaries.is_some() {
					resource.post_error(Error::AlreadySet, "primaries already set");
					return;
				}
				builder.primaries = match primaries {
					WEnum::Value(Primaries::Srgb) => Some(NamedPrimaries::Srgb),
					WEnum::Value(Primaries::Bt2020) => Some(NamedPrimaries::Bt2020),
					_ => {
						resource.post_error(Error::InvalidPrimariesNamed, "unsupported primaries");
						None
					},
				};
			},
			Request::SetLuminances {
				min_lum,
				max_lum,
				reference_lum,
			} => {
				if builder.luminances.is_some() {
					resource.post_error(Error::AlreadySet, "luminances already set");
				} else if u64::from(max_lum) * 10_000 <= u64::from(min_lum)
					|| u64::from(reference_lum) * 10_000 <= u64::from(min_lum)
				{
					resource.post_error(Error::InvalidLuminance, "luminance range is empty");
				} else {
					builder.luminances = Some((min_lum, max_lum, reference_lum));
				}
			},
			Request::SetMasteringDisplayPrimaries {
				r_x,
				r_y,
				g_x,
				g_y,
				b_x,
				b_y,
				w_x,
				w_y,
			} => {
				if builder.mastering_primaries.is_some() {
					resource.post_error(Error::AlreadySet, "mastering display primaries already set");
				} else {
					builder.mastering_primaries = Some([(r_x, r_y), (g_x, g_y), (b_x, b_y), (w_x, w_y)]);
				}
			},
			Request::SetMasteringLuminance { min_lum, max_lum } => {
				if builder.mastering_luminance.is_some() {
					resource.post_error(Error::AlreadySet, "mastering luminance already set");
				} else if u64::from(max_lum) * 10_000 <= u64::from(min_lum) {
					resource.post_error(Error::InvalidLuminance, "mastering luminance range is empty");
				} else {
					builder.mastering_luminance = Some((min_lum, max_lum));
				}
			},
			Request::SetMaxCll { max_cll } => {
				if builder.max_cll.is_some() {
					resource.post_error(Error::AlreadySet, "max_cll already set");
				} else {
					builder.max_cll = Some(max_cll);
				}
			},
			Request::SetMaxFall { max_fall } => {
				if builder.max_fall.is_some() {
					resource.post_error(Error::AlreadySet, "max_fall already set");
				} else {
					builder.max_fall = Some(max_fall);
				}
			},
			_ => {},
		}
	}
}

impl Dispatch2<WpImageDescriptionV1, FerriscopeState> for ImageDescriptionData {
	fn request(
		&self,
		state: &mut FerriscopeState,
		_client: &Client,
		resource: &WpImageDescriptionV1,
		request: wp_image_description_v1::Request,
		_handle: &DisplayHandle,
		data_init: &mut DataInit<'_, FerriscopeState>,
	) {
		use wp_image_description_v1::{Error, Request};
		match request {
			Request::Destroy => {},
			Request::GetInformation { information } => {
				let info = data_init.init(information, InfoData);
				match (self.colour, self.preferred) {
					(Some(_), Some(hdr)) => {
						send_information(&info, hdr);
						state.defer(move || info.done());
					},
					(Some(_), None) => {
						resource.post_error(Error::NoInformation, "this image description has no information");
					},
					(None, _) => resource.post_error(Error::NotReady, "the image description is not ready"),
				}
			},
			_ => {},
		}
	}
}

impl Dispatch2<WpImageDescriptionInfoV1, FerriscopeState> for InfoData {
	fn request(
		&self,
		_state: &mut FerriscopeState,
		_client: &Client,
		_resource: &WpImageDescriptionInfoV1,
		_request: <WpImageDescriptionInfoV1 as Resource>::Request,
		_handle: &DisplayHandle,
		_data_init: &mut DataInit<'_, FerriscopeState>,
	) {
	}
}

impl GlobalDispatch2<FrogColorManagementFactoryV1, FerriscopeState> for FrogFactoryData {
	fn bind(
		&self,
		_state: &mut FerriscopeState,
		_handle: &DisplayHandle,
		_client: &Client,
		resource: New<FrogColorManagementFactoryV1>,
		data_init: &mut DataInit<'_, FerriscopeState>,
	) {
		data_init.init(resource, *self);
	}
}

impl Dispatch2<FrogColorManagementFactoryV1, FerriscopeState> for FrogFactoryData {
	fn request(
		&self,
		_state: &mut FerriscopeState,
		_client: &Client,
		_resource: &FrogColorManagementFactoryV1,
		request: frog_color_management_factory_v1::Request,
		_handle: &DisplayHandle,
		data_init: &mut DataInit<'_, FerriscopeState>,
	) {
		match request {
			frog_color_management_factory_v1::Request::Destroy => {},
			frog_color_management_factory_v1::Request::GetColorManagedSurface { surface, callback } => {
				let managed = data_init.init(callback, FrogSurfaceData { surface });
				send_frog_preferred(&managed, self.hdr);
			},
		}
	}
}

/// `preferred_metadata`: what the compositor would like the surface tone-mapped
/// to. Chromaticities in 0.00002, luminances in cd/m2 (minimum in 0.0001).
fn send_frog_preferred(managed: &FrogColorManagedSurface, hdr: bool) {
	use frog_color_managed_surface::TransferFunction;
	let cta = |(x, y): (i32, i32)| ((x / 20) as u32, (y / 20) as u32);
	let [red, green, blue, white] = if hdr { BT2020_PRIMARIES } else { SRGB_PRIMARIES }.map(cta);
	let (transfer, max, min, full_frame) = if hdr {
		(TransferFunction::St2084Pq, 1000, 50, 400)
	} else {
		(TransferFunction::Gamma22, 80, 2000, 80)
	};
	managed.preferred_metadata(
		transfer, red.0, red.1, green.0, green.1, blue.0, blue.1, white.0, white.1, max, min, full_frame,
	);
}

impl Dispatch2<FrogColorManagedSurface, FerriscopeState> for FrogSurfaceData {
	fn request(
		&self,
		_state: &mut FerriscopeState,
		_client: &Client,
		_resource: &FrogColorManagedSurface,
		request: frog_color_managed_surface::Request,
		_handle: &DisplayHandle,
		_data_init: &mut DataInit<'_, FerriscopeState>,
	) {
		use frog_color_managed_surface::{Request, TransferFunction};
		match request {
			Request::Destroy => {},
			Request::SetKnownTransferFunction { transfer_function } => {
				let kind = match transfer_function {
					WEnum::Value(TransferFunction::St2084Pq) => Some(ContentKind::Pq),
					WEnum::Value(TransferFunction::ScrgbLinear) => Some(ContentKind::ScRgb),
					WEnum::Value(TransferFunction::Srgb | TransferFunction::Gamma22) => Some(ContentKind::Sdr),
					_ => None,
				};
				edit_pending(&self.surface, |state| state.frog.kind = kind);
			},
			// The container volume is implied by the transfer function (HDR10
			// is BT.2020, scRGB and SDR are BT.709).
			Request::SetKnownContainerColorVolume { .. } | Request::SetRenderIntent { .. } => {},
			Request::SetHdrMetadata {
				mastering_display_primary_red_x,
				mastering_display_primary_red_y,
				mastering_display_primary_green_x,
				mastering_display_primary_green_y,
				mastering_display_primary_blue_x,
				mastering_display_primary_blue_y,
				mastering_white_point_x,
				mastering_white_point_y,
				max_display_mastering_luminance,
				min_display_mastering_luminance,
				max_cll,
				max_fall,
			} => {
				let metadata = HdrMetadata {
					primaries: [
						(mastering_display_primary_red_x, mastering_display_primary_red_y),
						(mastering_display_primary_green_x, mastering_display_primary_green_y),
						(mastering_display_primary_blue_x, mastering_display_primary_blue_y),
					],
					white_point: (mastering_white_point_x, mastering_white_point_y),
					max_display_mastering_luminance,
					min_display_mastering_luminance,
					max_cll,
					max_fall,
				};
				edit_pending(&self.surface, |state| state.frog.metadata = Some(metadata));
			},
		}
	}

	fn destroyed(&self, _state: &mut FerriscopeState, _client: ClientId, _resource: &FrogColorManagedSurface) {
		// Destroying the managed surface resets its colour state.
		edit_pending(&self.surface, |state| state.frog = FrogParams::default());
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn pq_builder() -> ParamsBuilder {
		ParamsBuilder {
			transfer: Some(NamedTransfer::Pq),
			primaries: Some(NamedPrimaries::Bt2020),
			..ParamsBuilder::default()
		}
	}

	#[test]
	fn an_incomplete_set_cannot_be_created() {
		assert_eq!(ParamsBuilder::default().build(), Err(BuildError::Incomplete));
		let only_tf = ParamsBuilder {
			transfer: Some(NamedTransfer::Pq),
			..ParamsBuilder::default()
		};
		assert_eq!(only_tf.build(), Err(BuildError::Incomplete));
	}

	#[test]
	fn the_carried_combinations_classify_by_encoding() {
		assert_eq!(pq_builder().build().unwrap().unwrap().kind, ContentKind::Pq);
		let scrgb = ParamsBuilder {
			transfer: Some(NamedTransfer::ExtLinear),
			primaries: Some(NamedPrimaries::Srgb),
			..ParamsBuilder::default()
		};
		assert_eq!(scrgb.build().unwrap().unwrap().kind, ContentKind::ScRgb);
		let sdr = ParamsBuilder {
			transfer: Some(NamedTransfer::Sdr),
			primaries: Some(NamedPrimaries::Srgb),
			..ParamsBuilder::default()
		};
		assert_eq!(sdr.build().unwrap().unwrap().kind, ContentKind::Sdr);
	}

	#[test]
	fn combinations_that_cannot_be_carried_are_unsupported_not_errors() {
		let pq_709 = ParamsBuilder {
			primaries: Some(NamedPrimaries::Srgb),
			..pq_builder()
		};
		assert_eq!(pq_709.build(), Ok(None));
		let linear_2020 = ParamsBuilder {
			transfer: Some(NamedTransfer::ExtLinear),
			..pq_builder()
		};
		assert_eq!(linear_2020.build(), Ok(None));
	}

	#[test]
	fn mastering_data_is_converted_to_cta_units() {
		let builder = ParamsBuilder {
			// BT.2020 primaries and D65, x1e6.
			mastering_primaries: Some(BT2020_PRIMARIES),
			// 0.005 cd/m2 to 1000 cd/m2.
			mastering_luminance: Some((50, 1000)),
			max_cll: Some(900),
			max_fall: Some(300),
			..pq_builder()
		};
		let metadata = builder.build().unwrap().unwrap().metadata;
		assert_eq!(
			metadata.mastering_primaries,
			Some([(35_400, 14_600), (8_500, 39_850), (6_550, 2_300)])
		);
		assert_eq!(metadata.white_point, Some((15_635, 16_450)));
		assert_eq!(metadata.mastering_luminance, Some((50, 1000)));
		assert_eq!((metadata.max_cll, metadata.max_fall), (Some(900), Some(300)));
	}

	#[test]
	fn max_fall_above_max_cll_is_invalid() {
		let builder = ParamsBuilder {
			max_cll: Some(300),
			max_fall: Some(900),
			..pq_builder()
		};
		assert_eq!(builder.build(), Err(BuildError::InvalidLuminance));
	}

	#[test]
	fn sdr_descriptions_drop_hdr_metadata() {
		let builder = ParamsBuilder {
			transfer: Some(NamedTransfer::Sdr),
			primaries: Some(NamedPrimaries::Srgb),
			max_cll: Some(900),
			..ParamsBuilder::default()
		};
		assert_eq!(builder.build().unwrap().unwrap(), ContentColour::default());
	}

	#[test]
	fn a_frog_surface_is_undefined_until_its_transfer_function_is_set() {
		let mut params = FrogParams::default();
		assert_eq!(params.colour(), None);
		params.kind = Some(ContentKind::Pq);
		assert_eq!(params.colour().unwrap().kind, ContentKind::Pq);
	}

	#[test]
	fn frog_metadata_follows_gamescopes_sanity_check() {
		let valid = HdrMetadata {
			primaries: [(34000, 16000), (13250, 34500), (7500, 3000)],
			white_point: (15635, 16450),
			max_display_mastering_luminance: 1000,
			min_display_mastering_luminance: 50,
			max_cll: 900,
			max_fall: 300,
		};
		let mut params = FrogParams {
			kind: Some(ContentKind::Pq),
			metadata: Some(valid),
		};
		assert_eq!(params.colour().unwrap().metadata.max_cll, Some(900));
		params.metadata = Some(HdrMetadata { max_cll: 0, ..valid });
		assert!(params.colour().unwrap().metadata.is_empty());
		// SDR content carries no HDR metadata.
		params = FrogParams {
			kind: Some(ContentKind::Sdr),
			metadata: Some(valid),
		};
		assert!(params.colour().unwrap().metadata.is_empty());
	}

	#[test]
	fn the_preferred_colour_follows_the_session() {
		assert_eq!(preferred_colour(true).kind, ContentKind::Pq);
		assert_eq!(preferred_colour(false).kind, ContentKind::Sdr);
	}
}
