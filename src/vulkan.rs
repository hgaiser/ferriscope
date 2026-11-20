//! The Vulkan instance a compositor renders with.

use smithay::backend::vulkan::{Instance, version::Version};

use crate::error::{Error, Result};

/// A Vulkan instance that compositors can share.
///
/// Give one to every compositor through [`crate::CompositorConfig::vulkan_instance`]
/// when an embedder runs compositors one after another in one process. Without
/// it, each compositor creates and destroys its own instance, so the Vulkan
/// loader unloads the driver after each compositor and loads it again for the
/// next one. Each reload costs a driver initialisation, and some drivers do not
/// survive many of them: NVIDIA's `libnvidia-tls.so` uses static TLS, which glibc
/// does not give back on unload, so after about 26 reloads `vkCreateInstance`
/// fails with `VK_ERROR_INCOMPATIBLE_DRIVER`.
///
/// Cloning is cheap: clones share the same instance, which is destroyed when the
/// last clone (including those held by running compositors) is dropped.
#[derive(Debug, Clone)]
pub struct VulkanInstance(Instance);

impl VulkanInstance {
	/// Create an instance with the settings the compositor's renderer needs.
	pub fn new() -> Result<Self> {
		Instance::new(Version::VERSION_1_3, None)
			.map(Self)
			.map_err(|e| Error::Gpu(format!("vulkan instance: {e:?}")))
	}

	/// The underlying smithay instance, for an embedder that also uses smithay's
	/// Vulkan API on it (for example to enumerate physical devices).
	pub fn smithay(&self) -> &Instance {
		&self.0
	}
}

impl From<Instance> for VulkanInstance {
	/// Use an instance the embedder created itself. It must support Vulkan 1.3.
	fn from(instance: Instance) -> Self {
		Self(instance)
	}
}
