//! Build script for `ferriscope-core`.
//!
//! The server bindings for the vendored gamescope protocols are generated at
//! compile time by the `wayland-scanner` proc macros in
//! `src/action_binding.rs`, `src/ime.rs`, `src/swapchain.rs` and
//! `src/colour_management.rs`. At the pinned 0.31 line `wayland-scanner` exposes
//! code generation only as proc macros, not as a build-script library, so there
//! is no generated include under `OUT_DIR`; this script only declares the XML so
//! cargo rebuilds when it changes.
fn main() {
	for xml in [
		"gamescope-action-binding",
		"gamescope-input-method",
		"gamescope-xwayland",
		"gamescope-swapchain",
		"ferriscope-swapchain-v1",
		"frog-color-management-v1",
	] {
		println!("cargo:rerun-if-changed=protocol/{xml}.xml");
	}
}
