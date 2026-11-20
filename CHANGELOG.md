# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [v0.2.0] - 2026-11-20

### Added

- Add `VulkanInstance` and `CompositorConfig::vulkan_instance`, so an embedder that runs compositors one after another in one process can give them one Vulkan instance. Each compositor still creates its own renderer (device). Without a shared instance, the Vulkan loader unloads the driver after each compositor and loads it again for the next one, and NVIDIA's driver fails `vkCreateInstance` with `VK_ERROR_INCOMPATIBLE_DRIVER` after about 26 reloads (`libnvidia-tls.so` uses static TLS, which glibc does not give back).

### Changed

- **Breaking:** `CompositorConfig` has a new public field, `vulkan_instance`. Code that builds it with struct-update syntax (`..CompositorConfig::default()`) is not affected.

## [v0.1.1] - 2026-11-17

### Fixed

- Fix the first pen contact freezing the compositor (a deadlock in smithay's tablet grab), by updating ferriscope-smithay to v0.7.3.
- Draw cursors and other shm buffers that a client keeps at an offset in a shared pool (such as a `wayland-cursor` theme) from their own pixels, by updating ferriscope-smithay to v0.7.3.

### Changed

- Run tests one at a time (`.cargo/config.toml`): parallel GPU tests crash or deadlock the Vulkan loader, and parallel compositors disturb each other.

## [v0.1.0] - 2026-11-14

### Added

- Initial implementation.
