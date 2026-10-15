//! Environment-driven behaviour of the layer, as pure functions over an
//! environment lookup so they can be tested.
//!
//! Ported from gamescope's `defaultLayerClientFlags`, `getMinImageCount` and
//! `getEnsureMinImageCount` (`layer/VkLayer_FROG_gamescope_wsi.cpp`), minus the
//! frame-limiter flags: ferriscope has no dynamic limiter, so there is no
//! forced-FIFO state for an application to be aware of.

/// Looks an environment variable up; `None` when unset.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Per-application behaviour switches (gamescope's `GamescopeLayerClient::Flag`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Flags(u32);

impl Flags {
	/// Bypass Xwayland whatever the window tree looks like
	/// (`FERRISCOPE_WSI_FORCE_BYPASS`).
	pub const FORCE_BYPASS: Flags = Flags(1);
	/// Never expose HDR surface formats to this application.
	pub const DISABLE_HDR: Flags = Flags(1 << 1);
	/// Create swapchains at the window's current extent, whatever the
	/// application asked for.
	pub const FORCE_SWAPCHAIN_EXTENT: Flags = Flags(1 << 2);
	/// Never answer a present with `VK_SUBOPTIMAL_KHR`.
	pub const NO_SUBOPTIMAL: Flags = Flags(1 << 3);

	/// Whether every flag of `other` is set.
	pub fn contains(self, other: Flags) -> bool {
		self.0 & other.0 == other.0
	}

	fn set(&mut self, other: Flags, on: bool) {
		if on {
			self.0 |= other.0;
		} else {
			self.0 &= !other.0;
		}
	}
}

/// Steam app id of "My Little Pony: A Maretime Bay Adventure", which picks an
/// HDR colour space when offered one but does not render HDR at all.
const MLP_APP_ID: u32 = 1_600_780;

/// Croteam executables that mishandle `VK_SUBOPTIMAL_KHR` and a changing
/// swapchain extent.
const CROTEAM_EXECUTABLES: [&str; 6] = [
	"Talos",
	"Talos_Unrestricted",
	"Talos_VR",
	"Talos_Unrestricted_VR",
	"Sam2017",
	"Sam2017_Unrestricted",
];

/// The `true`/`1` convention of gamescope's `parseEnv<bool>`: `None` for unset
/// or empty, `Some(true)` for exactly `true` or `1`, `Some(false)` otherwise.
pub fn env_bool(env: Env, name: &str) -> Option<bool> {
	let value = env(name).filter(|value| !value.is_empty())?;
	Some(value == "true" || value == "1")
}

/// `atoi(value) != 0` for a set, non-empty variable.
fn env_atoi_nonzero(env: Env, name: &str) -> bool {
	env(name)
		.filter(|value| !value.is_empty())
		.is_some_and(|value| atoi(&value) != 0)
}

/// C `atoi`: the leading decimal integer, 0 when there is none.
fn atoi(value: &str) -> i64 {
	let trimmed = value.trim_start();
	let (sign, digits) = match trimmed.strip_prefix('-') {
		Some(rest) => (-1, rest),
		None => (1, trimmed.strip_prefix('+').unwrap_or(trimmed)),
	};
	let end = digits.find(|c: char| !c.is_ascii_digit()).unwrap_or(digits.len());
	sign * digits[..end].parse::<i64>().unwrap_or(0)
}

fn env_u32(env: Env, name: &str) -> Option<u32> {
	env(name).filter(|value| !value.is_empty())?.parse().ok()
}

/// The flags an application gets before the swapchain-specific ones.
pub fn default_flags(env: Env, app_id: u32, executable: &str) -> Flags {
	let mut flags = Flags::default();
	if env_atoi_nonzero(env, "FERRISCOPE_WSI_FORCE_BYPASS") {
		flags.set(Flags::FORCE_BYPASS, true);
	}
	if app_id == MLP_APP_ID {
		flags.set(Flags::DISABLE_HDR, true);
	}
	if CROTEAM_EXECUTABLES.contains(&executable) {
		flags.set(Flags::FORCE_SWAPCHAIN_EXTENT, true);
		flags.set(Flags::NO_SUBOPTIMAL, true);
	}
	// Mesa's own knobs override the heuristics, as in gamescope.
	if let Some(value) = env("vk_wsi_force_swapchain_to_current_extent").filter(|value| !value.is_empty()) {
		flags.set(Flags::FORCE_SWAPCHAIN_EXTENT, value == "true");
	}
	if let Some(value) = env("vk_x11_ignore_suboptimal").filter(|value| !value.is_empty()) {
		flags.set(Flags::NO_SUBOPTIMAL, value == "true");
	}
	flags
}

/// The minimum image count the layer reports in surface capabilities
/// (`getMinImageCount`): 3 unless overridden.
pub fn min_image_count(env: Env) -> u32 {
	[
		"FERRISCOPE_WSI_MIN_IMAGE_COUNT",
		"vk_wsi_override_min_image_count",
		"vk_x11_override_min_image_count",
	]
	.into_iter()
	.find_map(|name| env_u32(env, name))
	.unwrap_or(3)
}

/// Whether a swapchain's `minImageCount` is raised to [`min_image_count`]
/// (`getEnsureMinImageCount`).
pub fn ensure_min_image_count(env: Env) -> bool {
	["FERRISCOPE_WSI_ENSURE_MIN_IMAGE_COUNT", "vk_x11_ensure_min_image_count"]
		.into_iter()
		.find_map(|name| env_bool(env, name))
		.unwrap_or(false)
}

/// Whether a FIFO application is held at the end of `vkQueuePresentKHR` until
/// the compositor can take its next frame in time, so its next loop iteration,
/// input included, starts right after a tick. On unless
/// `FERRISCOPE_WSI_PACING` is `0`, `false` or `off`.
pub fn pacing(env: Env) -> bool {
	!matches!(env("FERRISCOPE_WSI_PACING").as_deref(), Some("0" | "false" | "off"))
}

/// The Steam app id (`SteamAppId`), 0 when not under Steam.
pub fn app_id(env: Env) -> u32 {
	env("SteamAppId")
		.filter(|value| !value.is_empty())
		.map_or(0, |value| atoi(&value).max(0) as u32)
}

/// The executable's name as Mesa's driconf derives it (`__getProgramName`):
/// the basename of `/proc/self/exe` when it prefixes the invocation name, else
/// the last path component of the invocation name, where a backslash also
/// splits (Wine passes Windows paths).
pub fn program_name(invocation: &str, exe_path: Option<&str>) -> String {
	if let Some(slash) = invocation.rfind('/') {
		if let Some(path) = exe_path
			&& invocation.starts_with(path)
			&& let Some(base) = path.rfind('/')
		{
			return path[base + 1..].to_string();
		}
		return invocation[slash + 1..].to_string();
	}
	match invocation.rfind('\\') {
		Some(backslash) => invocation[backslash + 1..].to_string(),
		None => invocation.to_string(),
	}
}

/// The running executable's name, with Mesa's overrides.
pub fn executable_name() -> String {
	for name in ["MESA_DRICONF_EXECUTABLE_OVERRIDE", "MESA_PROCESS_NAME"] {
		if let Ok(value) = std::env::var(name)
			&& !value.is_empty()
		{
			return value;
		}
	}
	// SAFETY: glibc's `program_invocation_name` is a valid C string for the
	// life of the process.
	let invocation = unsafe {
		unsafe extern "C" {
			static program_invocation_name: *const std::ffi::c_char;
		}
		let pointer = program_invocation_name;
		if pointer.is_null() {
			return String::new();
		}
		std::ffi::CStr::from_ptr(pointer).to_string_lossy().into_owned()
	};
	let exe = std::fs::read_link("/proc/self/exe")
		.ok()
		.map(|path| path.to_string_lossy().into_owned());
	program_name(&invocation, exe.as_deref())
}

#[cfg(test)]
mod tests {
	use std::collections::HashMap;

	#[test]
	fn program_names_follow_mesa() {
		use super::program_name;
		assert_eq!(program_name("/usr/bin/vkcube", None), "vkcube");
		assert_eq!(
			program_name("/opt/g/Game.exe --arg", Some("/opt/g/Game.exe")),
			"Game.exe"
		);
		assert_eq!(program_name("Z:\\games\\Ori.exe", None), "Ori.exe");
		assert_eq!(program_name("plain", None), "plain");
	}

	use super::*;

	fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
		let map: HashMap<&str, &str> = pairs.iter().copied().collect();
		move |name| map.get(name).map(|value| (*value).to_string())
	}

	#[test]
	fn plain_applications_get_no_flags() {
		let env = env_of(&[]);
		assert_eq!(default_flags(&env, 440, "hl2.exe"), Flags::default());
	}

	#[test]
	fn the_pony_game_never_sees_hdr() {
		let env = env_of(&[]);
		let flags = default_flags(&env, MLP_APP_ID, "game.exe");
		assert!(flags.contains(Flags::DISABLE_HDR));
		assert!(!flags.contains(Flags::NO_SUBOPTIMAL));
	}

	#[test]
	fn croteam_games_get_a_fixed_extent_and_no_suboptimal() {
		let env = env_of(&[]);
		let flags = default_flags(&env, 0, "Talos");
		assert!(flags.contains(Flags::FORCE_SWAPCHAIN_EXTENT));
		assert!(flags.contains(Flags::NO_SUBOPTIMAL));
	}

	#[test]
	fn mesa_knobs_override_the_executable_heuristics() {
		let env = env_of(&[
			("vk_wsi_force_swapchain_to_current_extent", "false"),
			("vk_x11_ignore_suboptimal", "false"),
		]);
		let flags = default_flags(&env, 0, "Talos");
		assert!(!flags.contains(Flags::FORCE_SWAPCHAIN_EXTENT));
		assert!(!flags.contains(Flags::NO_SUBOPTIMAL));

		let env = env_of(&[("vk_x11_ignore_suboptimal", "true")]);
		assert!(default_flags(&env, 0, "other").contains(Flags::NO_SUBOPTIMAL));
	}

	#[test]
	fn force_bypass_follows_atoi() {
		assert!(default_flags(&env_of(&[("FERRISCOPE_WSI_FORCE_BYPASS", "1")]), 0, "x").contains(Flags::FORCE_BYPASS));
		assert!(!default_flags(&env_of(&[("FERRISCOPE_WSI_FORCE_BYPASS", "0")]), 0, "x").contains(Flags::FORCE_BYPASS));
		assert!(!default_flags(&env_of(&[("FERRISCOPE_WSI_FORCE_BYPASS", "")]), 0, "x").contains(Flags::FORCE_BYPASS));
		assert!(
			!default_flags(&env_of(&[("FERRISCOPE_WSI_FORCE_BYPASS", "yes")]), 0, "x").contains(Flags::FORCE_BYPASS)
		);
	}

	#[test]
	fn min_image_count_defaults_to_three_and_takes_the_first_override() {
		assert_eq!(min_image_count(&env_of(&[])), 3);
		assert_eq!(
			min_image_count(&env_of(&[
				("vk_x11_override_min_image_count", "5"),
				("FERRISCOPE_WSI_MIN_IMAGE_COUNT", "2"),
			])),
			2
		);
		assert_eq!(min_image_count(&env_of(&[("vk_wsi_override_min_image_count", "x")])), 3);
	}

	#[test]
	fn ensure_min_image_count_is_off_unless_asked() {
		assert!(!ensure_min_image_count(&env_of(&[])));
		assert!(ensure_min_image_count(&env_of(&[(
			"FERRISCOPE_WSI_ENSURE_MIN_IMAGE_COUNT",
			"true"
		)])));
		assert!(!ensure_min_image_count(&env_of(&[(
			"FERRISCOPE_WSI_ENSURE_MIN_IMAGE_COUNT",
			"0"
		)])));
	}

	#[test]
	fn pacing_is_on_unless_switched_off() {
		assert!(pacing(&env_of(&[])));
		assert!(pacing(&env_of(&[("FERRISCOPE_WSI_PACING", "1")])));
		assert!(!pacing(&env_of(&[("FERRISCOPE_WSI_PACING", "0")])));
		assert!(!pacing(&env_of(&[("FERRISCOPE_WSI_PACING", "off")])));
	}

	#[test]
	fn the_steam_app_id_parses_like_atoi() {
		assert_eq!(app_id(&env_of(&[("SteamAppId", "261570")])), 261_570);
		assert_eq!(app_id(&env_of(&[("SteamAppId", "")])), 0);
		assert_eq!(app_id(&env_of(&[("SteamAppId", "abc")])), 0);
		assert_eq!(app_id(&env_of(&[])), 0);
	}
}
