//! The Vulkan renderer on the configured render node.

use std::{os::unix::fs::MetadataExt, path::Path};

use smithay::{
	backend::{
		drm::{DrmDeviceFd, DrmNode},
		renderer::vulkan::VulkanRenderer,
		vulkan::{Instance, PhysicalDevice, version::Version},
	},
	utils::DeviceFd,
};

use crate::error::{Error, Result};

/// Create the renderer on the GPU behind `render_node`.
///
/// The renderer composites on a compute-only queue family when the GPU has one
/// (ADR 0008), so the composite does not share a hardware queue with the
/// game's graphics work. `file` is an open handle to `render_node`; the
/// renderer uses it to export sync_files.
pub(in crate::shell) fn create_renderer(render_node: &Path, file: std::fs::File) -> Result<VulkanRenderer> {
	let dev = std::fs::metadata(render_node)
		.map_err(|e| Error::Gpu(format!("stat {}: {e}", render_node.display())))?
		.rdev();
	let node = DrmNode::from_dev_id(dev).map_err(|e| Error::Gpu(format!("drm node {}: {e}", render_node.display())))?;
	let instance =
		Instance::new(Version::VERSION_1_3, None).map_err(|e| Error::Gpu(format!("vulkan instance: {e:?}")))?;
	let phd = PhysicalDevice::enumerate(&instance)
		.map_err(|e| Error::Gpu(format!("enumerate vulkan devices: {e:?}")))?
		.find(|phd| phd.render_node().ok().flatten() == Some(node) || phd.primary_node().ok().flatten() == Some(node))
		.ok_or_else(|| Error::Gpu(format!("no vulkan device for {}", render_node.display())))?;
	let drm = DrmDeviceFd::new(DeviceFd::from(std::os::fd::OwnedFd::from(file)));
	let renderer = VulkanRenderer::new(&phd, Some(drm)).map_err(|e| Error::Gpu(format!("vulkan renderer: {e:?}")))?;
	tracing::info!(
		device = phd.name(),
		queue_family = renderer.queue_family_index(),
		dedicated_compute = renderer.is_dedicated_compute_queue(),
		"vulkan renderer"
	);
	Ok(renderer)
}

/// The renderer and a GBM allocator on `/dev/dri/renderD128`, for GPU tests.
#[cfg(test)]
pub(in crate::shell) fn test_renderer() -> (
	VulkanRenderer,
	smithay::backend::allocator::gbm::GbmAllocator<std::sync::Arc<std::fs::File>>,
) {
	use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};

	const RENDER_NODE: &str = "/dev/dri/renderD128";
	let open = || {
		std::fs::OpenOptions::new()
			.read(true)
			.write(true)
			.open(RENDER_NODE)
			.expect("open the render node")
	};
	let renderer = create_renderer(Path::new(RENDER_NODE), open()).expect("vulkan renderer");
	let gbm = GbmDevice::new(std::sync::Arc::new(open())).expect("gbm device");
	(renderer, GbmAllocator::new(gbm, GbmBufferFlags::RENDERING))
}
