//! Optional `--steam` mode.
//!
//! ferriscope classifies Steam atoms unconditionally, so the mode is
//! deliberately small: it gates only the behaviour that is genuinely optional,
//! gamescope's `steamMode` — the **focused-window contract** that lets Steam
//! follow the focused window and the **Steam capability block** of the hosted-app
//! environment ([`app_env`]). The **gamescope session identity** in that
//! environment (`XDG_CURRENT_DESKTOP=gamescope`, `GAMESCOPE_WAYLAND_DISPLAY`,
//! `STEAM_GAME_DISPLAY_0`) is *not* gated: gamescope advertises it regardless of
//! `steamMode` (`main.cpp:1080-1103`), so a plain `ferriscope` identifies as
//! gamescope exactly as a plain `gamescope` does.
//!
//! gamescope writes a root-window contract only when `steamMode` is set
//! (`steamcompmgr.cpp:5376-5395` and the reference commit `4a28808`):
//!
//! | Property | Value | gamescope |
//! |---|---|---|
//! | `GAMESCOPE_FOCUSED_APP` | the app id of the input-focus window | `:5378` |
//! | `GAMESCOPE_FOCUSED_APP_GFX` | the app id of the base focus window | `:5381` |
//! | `GAMESCOPE_FOCUSED_WINDOW` | the base (role-holding) window's X11 id | `:5385` |
//! | `GAMESCOPE_FOCUS_DISPLAY` / `_MOUSE_` / `_KEYBOARD_` | the `:N` display name, `XA_CARDINAL` format 32 (the bytes as 32-bit words) | `:5388` |
//! | `GAMESCOPE_FOCUSABLE_APPS` / `_WINDOWS` | the candidate app ids / `[window, app, pid]` triplets | `:5078` |
//!
//! The app ids and window are one 32-bit `CARDINAL` each on the root window;
//! with the mode off gamescope writes none of them. With it on and a zero id it
//! replaces the property with a zero-length value, i.e. deletes it
//! (`focusedAppId != 0 ? 1: 0`, `:5379`). While an interactive overlay is the
//! focus owner, `_APP` is the Steam UI (769) and `_GFX` the game, and 769 is
//! excluded from the focusable lists (`4a28808`); see [`FocusedApp::for_owner`],
//! [`FocusCandidate`] and [`focusable_apps`].
//!
//! The app id of an Xwayland window is the atom-derived Steam identity from
//! [`crate::overlay`] (`STEAM_GAME`, or 769 for `STEAM_BIGPICTURE`), so an X
//! window that carries no Steam atom has no app id here. A native Wayland
//! window's comes from its process, as gamescope's
//! (`Process::GetAppIdFromPid`, [`crate::process`]). The focusable-window
//! `pid` is the XRes client pid, else `_NET_WM_PID` (0 when unknown).
//!
//! The unit also documents what `--steam` does **not** do: the atom
//! classification, the game-tier focus priority and the overlay/notification
//! split stay unconditional, and the connector/display, MangoApp and
//! VR paths stay out even when the flag is on. The hosted-app environment is split:
//! the Steam capability block is gated by the flag, the identity block is
//! unconditional. `GAMESCOPE_XWAYLAND_SERVER_ID` and `GAMESCOPE_PID` are set
//! unconditionally by the atom connection (server discovery and compositor
//! recognition, not `--steam` behaviours; gamescope writes them regardless of
//! `steamMode`, `steamcompmgr.cpp:9021-9022`).

/// The app id Steam uses for its own UI, re-exported from [`crate::focus`].
pub use crate::focus::STEAM_BIG_PICTURE_APP_ID;

/// The two focused-app values gamescope publishes to Steam
/// (`steamcompmgr.cpp:5344-5383`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FocusedApp {
	/// `GAMESCOPE_FOCUSED_APP`: the app id of the input-focus window.
	pub app_id: u32,
	/// `GAMESCOPE_FOCUSED_APP_GFX`: the app id of the base focus window.
	pub gfx_app_id: u32,
}

impl FocusedApp {
	/// Derive the two values from the focus owner and the role window, applying
	/// gamescope's overlay split.
	///
	/// `owner_app_id` is the keyboard-focus owner's atom-derived app id and
	/// `owner_is_overlay` is true when that owner is an interactive Steam
	/// overlay. While the overlay holds focus, gamescope publishes
	/// the Steam UI app id (769) as the input app while the role-holding window
	/// keeps `_GFX` (`set_focused_app_split(STEAM_BIG_PICTURE_APPID,
	/// focusedAppId)` in the reference commit `4a28808`); otherwise both values
	/// are the owner/role app ids. ferriscope maps the commit's `overlay_raised`
	/// flag onto the interactive overlay holding input focus: the owner is the input
	/// window, the role holder is the gfx window.
	pub fn for_owner(owner_app_id: u32, owner_is_overlay: bool, base_app_id: u32) -> Self {
		Self {
			app_id: if owner_is_overlay {
				STEAM_BIG_PICTURE_APP_ID
			} else {
				owner_app_id
			},
			gfx_app_id: base_app_id,
		}
	}
}

/// The value gamescope writes for one focused-app property.
///
/// `None` means a zero app id: gamescope replaces the property with a
/// zero-length value, deleting it (`steamcompmgr.cpp:5376-5383`; `focusedAppId
/// != 0 ? 1: 0`). `Some` is the single `CARDINAL` to publish.
pub fn property_value(app_id: u32) -> Option<u32> {
	(app_id != 0).then_some(app_id)
}

/// One entry of the focusable-app/window lists gamescope publishes to Steam
/// (`GAMESCOPE_FOCUSABLE_APPS` / `GAMESCOPE_FOCUSABLE_WINDOWS`).
///
/// In ferriscope these are the windows Steam may route controller input to: the
/// role-holding window and every other keyboard-focusable `space` element with a
/// non-zero Steam app id (a displaced peer, for example the Steam UI shell).
/// Dialogs and interactive overlays are not `space` elements with an app id, so
/// they do not appear here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusCandidate {
	/// The candidate's window id: its X11 id, or the synthetic id of a native
	/// Wayland toplevel ([`crate::focus::WindowId`]).
	pub window_id: u32,
	/// Its Steam app id (non-zero; zero-app-id candidates are never published).
	pub app_id: u32,
	/// Its client pid (XRes, else `_NET_WM_PID`), or 0 when unknown.
	pub pid: u32,
}

/// Whether a candidate app id belongs in the focusable lists.
///
/// gamescope publishes only non-zero app ids; the reference commit excludes the
/// Steam UI app id (769) while an interactive overlay holds focus, so Steam
/// routes the gamepad to the overlay rather than to Big Picture (`4a28808`).
/// ferriscope maps that to the interactive overlay holding input focus.
pub fn is_focusable(app_id: u32, overlay_focused: bool) -> bool {
	app_id != 0 && (!overlay_focused || app_id != STEAM_BIG_PICTURE_APP_ID)
}

/// `GAMESCOPE_FOCUSABLE_APPS`: the app ids of the focus candidates, first-seen
/// order, de-duplicated. The writer publishes an empty array when this is
/// empty, clearing the property.
pub fn focusable_apps(candidates: &[FocusCandidate], overlay_focused: bool) -> Vec<u32> {
	let mut apps = Vec::new();
	for candidate in candidates {
		if is_focusable(candidate.app_id, overlay_focused) && !apps.contains(&candidate.app_id) {
			apps.push(candidate.app_id);
		}
	}
	apps
}

/// `GAMESCOPE_FOCUSABLE_WINDOWS`: the `[window_id, app_id, pid]` triplets for
/// the same candidates as [`focusable_apps`], flattened into the single
/// `CARDINAL` array gamescope writes.
pub fn focusable_windows(candidates: &[FocusCandidate], overlay_focused: bool) -> Vec<u32> {
	let mut windows = Vec::new();
	for candidate in candidates {
		if is_focusable(candidate.app_id, overlay_focused) {
			windows.extend_from_slice(&[candidate.window_id, candidate.app_id, candidate.pid]);
		}
	}
	windows
}

/// Convert an Xwayland display number to gamescope's XWayland server id.
///
/// gamescope encodes the display number as `(display_number + 100) << 8 |
/// display_number`; WSI clients read `GAMESCOPE_XWAYLAND_SERVER_ID` from the
/// root window to find the server that `override_window_content` names
/// (`x11_focus.rs`, reference commit `4a28808`).
pub fn display_id_to_server_id(display_number: u32) -> u32 {
	((display_number + 100) << 8) | display_number
}

/// The gamescope-impersonation environment ferriscope gives the hosted
/// application.
///
/// Steam keys its gamescope integration off these variables:
///
/// - the **session identity** (`XDG_CURRENT_DESKTOP=gamescope`,
///   `GAMESCOPE_WAYLAND_DISPLAY`, `ENABLE_FERRISCOPE_WSI`, `DISABLE_GAMESCOPE_WSI`, and
///   `STEAM_GAME_DISPLAY_0` when Xwayland is up) is set **unconditionally**, exactly as gamescope sets it at startup
///   regardless of `steamMode` (`main.cpp:1080-1103`);
/// - the **capability block** (`STEAM_GAMESCOPE_*`) is set only when `steam`
///   is on, matching gamescope's `steamMode`-gated `UpdateCompatEnvVars`
///   (`main.cpp:611-658`).
///
/// A plain `gamescope` (no `-e`) still tells its children they run under
/// gamescope, so a plain `ferriscope` must too; the `--steam` flag only adds
/// the Steam-specific capability advert. Every value is computed from the
/// compositor's **real** Wayland socket and X display, never from the host
/// process environment.
///
/// `xdisplay` is `None` on the Wayland-only path, so `STEAM_GAME_DISPLAY_0` is
/// omitted there. HDR is advertised only when the compositor actually produces
/// HDR (`hdr`). Scaling and VRR capability variables are not advertised, since
/// ferriscope does not back those features.
pub fn app_env(steam: bool, wayland_display: &str, xdisplay: Option<u32>, hdr: bool) -> Vec<(String, String)> {
	let mut env: Vec<(String, String)> = vec![
		("XDG_CURRENT_DESKTOP".to_string(), "gamescope".to_string()),
		("GAMESCOPE_WAYLAND_DISPLAY".to_string(), wayland_display.to_string()),
		// `ferriscope-wsi` replaces gamescope's layer, which does the same
		// job (`steamcompmgr.cpp:9357`) and must not load alongside it. The
		// layer finds the compositor through `WAYLAND_DISPLAY`, the one
		// socket pressure-vessel remaps besides `GAMESCOPE_WAYLAND_DISPLAY`.
		("ENABLE_FERRISCOPE_WSI".to_string(), "1".to_string()),
		("DISABLE_GAMESCOPE_WSI".to_string(), "1".to_string()),
	];
	if let Some(number) = xdisplay {
		env.push(("STEAM_GAME_DISPLAY_0".to_string(), format!(":{number}")));
	}
	if hdr {
		// DXVK offers its HDR swapchain only with this set; `ferriscope-wsi`
		// exposes the HDR formats because the compositor sends
		// `ferriscope_swapchain_factory_v1.hdr_available`.
		env.push(("DXVK_HDR".to_string(), "1".to_string()));
	}
	if steam {
		env.push(("STEAM_GAMESCOPE_DYNAMIC_FPSLIMITER".to_string(), "1".to_string()));
		if hdr {
			env.push(("STEAM_GAMESCOPE_HDR_SUPPORTED".to_string(), "1".to_string()));
		}
	}
	env
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_focused_game_publishes_its_app_id_for_both_properties() {
		let focused = FocusedApp::for_owner(440, false, 440);
		assert_eq!(
			focused,
			FocusedApp {
				app_id: 440,
				gfx_app_id: 440
			}
		);
	}

	#[test]
	fn big_picture_publishes_app_id_769() {
		let focused = FocusedApp::for_owner(STEAM_BIG_PICTURE_APP_ID, false, STEAM_BIG_PICTURE_APP_ID);
		assert_eq!(
			focused,
			FocusedApp {
				app_id: STEAM_BIG_PICTURE_APP_ID,
				gfx_app_id: STEAM_BIG_PICTURE_APP_ID
			}
		);
		// 769 is a legitimate app id, not an "absent" sentinel.
		assert_eq!(property_value(focused.app_id), Some(769));
	}

	#[test]
	fn a_focus_owner_without_an_atom_app_id_leaves_the_gfx_id() {
		// The keyboard-focus owner is a dialog or interactive overlay that
		// carries no Steam atom; the role window is the game. `_APP` is absent
		// while `_GFX` keeps the game (a window with no atom gets a process-derived id, not a keyboard-focus owner without one).
		let focused = FocusedApp::for_owner(0, false, 440);
		assert_eq!(focused.app_id, 0);
		assert_eq!(focused.gfx_app_id, 440);
		assert_eq!(property_value(focused.app_id), None);
	}

	#[test]
	fn a_zero_app_id_deletes_the_property() {
		assert_eq!(property_value(0), None);
		assert_eq!(property_value(440), Some(440));
	}

	#[test]
	fn the_default_publishes_nothing() {
		let focused = FocusedApp::default();
		assert_eq!(property_value(focused.app_id), None);
		assert_eq!(property_value(focused.gfx_app_id), None);
	}

	#[test]
	fn app_env_without_steam_advertises_the_gamescope_identity_only() {
		// The session identity is unconditional, exactly as gamescope sets it
		// at startup regardless of `steamMode` (`main.cpp:1080-1103`); only the
		// Steam capability block is gated on the mode.
		let expected: Vec<(String, String)> = vec![
			("XDG_CURRENT_DESKTOP".to_string(), "gamescope".to_string()),
			("GAMESCOPE_WAYLAND_DISPLAY".to_string(), "wayland-1".to_string()),
			("ENABLE_FERRISCOPE_WSI".to_string(), "1".to_string()),
			("DISABLE_GAMESCOPE_WSI".to_string(), "1".to_string()),
			("STEAM_GAME_DISPLAY_0".to_string(), ":3".to_string()),
		];
		assert_eq!(app_env(false, "wayland-1", Some(3), false), expected);
		// HDR is a capability, not identity: advertised only with the mode on.
		let off_hdr = app_env(false, "wayland-1", Some(3), true);
		assert!(!off_hdr.iter().any(|(key, _)| key == "STEAM_GAMESCOPE_HDR_SUPPORTED"));
		assert!(!off_hdr.iter().any(|(key, _)| key.starts_with("STEAM_GAMESCOPE_")));
		assert!(off_hdr.contains(&("DXVK_HDR".to_string(), "1".to_string())));
	}

	#[test]
	fn app_env_advertises_gamescope_and_the_capability_block() {
		let expected: Vec<(String, String)> = vec![
			("XDG_CURRENT_DESKTOP".to_string(), "gamescope".to_string()),
			("GAMESCOPE_WAYLAND_DISPLAY".to_string(), "wayland-2".to_string()),
			("ENABLE_FERRISCOPE_WSI".to_string(), "1".to_string()),
			("DISABLE_GAMESCOPE_WSI".to_string(), "1".to_string()),
			("STEAM_GAME_DISPLAY_0".to_string(), ":3".to_string()),
			("STEAM_GAMESCOPE_DYNAMIC_FPSLIMITER".to_string(), "1".to_string()),
		];
		assert_eq!(app_env(true, "wayland-2", Some(3), false), expected);
	}

	#[test]
	fn app_env_omits_the_game_display_without_xwayland() {
		// Identity without Xwayland (steam off) and with the capability block
		// (steam on) both omit `STEAM_GAME_DISPLAY_0`.
		for steam in [false, true] {
			let env = app_env(steam, "wayland-2", None, false);
			assert!(!env.iter().any(|(key, _)| key == "STEAM_GAME_DISPLAY_0"));
			assert!(env.contains(&("GAMESCOPE_WAYLAND_DISPLAY".to_string(), "wayland-2".to_string())));
		}
	}

	#[test]
	fn app_env_advertises_hdr_only_when_hdr() {
		let sdr = app_env(true, "wayland-2", Some(3), false);
		assert!(!sdr.iter().any(|(key, _)| key == "STEAM_GAMESCOPE_HDR_SUPPORTED"));
		let hdr = app_env(true, "wayland-2", Some(3), true);
		assert!(hdr.contains(&("STEAM_GAMESCOPE_HDR_SUPPORTED".to_string(), "1".to_string())));
		assert!(!sdr.iter().any(|(key, _)| key == "DXVK_HDR"));
		assert!(hdr.contains(&("DXVK_HDR".to_string(), "1".to_string())));
		// DXVK_HDR is not Steam-specific.
		assert!(app_env(false, "wayland-2", Some(3), true).contains(&("DXVK_HDR".to_string(), "1".to_string())));
	}

	#[test]
	fn an_interactive_overlay_inputs_as_the_steam_ui_and_renders_the_game() {
		// The overlay is the focus owner: `_APP` is the Steam UI (769), `_GFX`
		// stays on the role-holding game, matching the reference commit's
		// `set_focused_app_split(STEAM_BIG_PICTURE_APPID, focusedAppId)`.
		let split = FocusedApp::for_owner(0, true, 548_430);
		assert_eq!(split.app_id, STEAM_BIG_PICTURE_APP_ID);
		assert_eq!(split.gfx_app_id, 548_430);
	}

	#[test]
	fn a_dialog_owner_keeps_its_own_app_id() {
		// Only an interactive overlay takes the Steam UI input id; a dialog
		// whose owner carries no app id contributes zero while `_GFX` keeps the
		// role window.
		let dialog = FocusedApp::for_owner(0, false, 548_430);
		assert_eq!(dialog.app_id, 0);
		assert_eq!(dialog.gfx_app_id, 548_430);
	}

	#[test]
	fn a_window_owner_publishes_its_own_app_id() {
		let window = FocusedApp::for_owner(440, false, 440);
		assert_eq!(
			window,
			FocusedApp {
				app_id: 440,
				gfx_app_id: 440
			}
		);
	}

	#[test]
	fn focusable_apps_skip_zero_ids_and_deduplicate() {
		let candidates = [
			FocusCandidate {
				window_id: 10,
				app_id: 440,
				pid: 1,
			},
			FocusCandidate {
				window_id: 11,
				app_id: 0,
				pid: 2,
			},
			FocusCandidate {
				window_id: 12,
				app_id: 440,
				pid: 3,
			},
			FocusCandidate {
				window_id: 13,
				app_id: 769,
				pid: 0,
			},
		];
		assert_eq!(focusable_apps(&candidates, false), vec![440, 769]);
		assert_eq!(
			focusable_windows(&candidates, false),
			vec![10, 440, 1, 12, 440, 3, 13, 769, 0]
		);
	}

	#[test]
	fn a_wayland_window_is_reported_in_the_apps_and_the_windows() {
		// A Proton game on winewayland has an app id and a synthetic window id.
		// Steam promotes a game only once it sees its window listed.
		let candidates = [
			FocusCandidate {
				window_id: 13,
				app_id: 769,
				pid: 2,
			},
			FocusCandidate {
				window_id: 0x4000_0001,
				app_id: 3_405_340,
				pid: 7,
			},
		];
		assert_eq!(focusable_apps(&candidates, false), vec![769, 3_405_340]);
		assert_eq!(
			focusable_windows(&candidates, false),
			vec![13, 769, 2, 0x4000_0001, 3_405_340, 7]
		);
	}

	#[test]
	fn an_overlay_in_focus_excludes_the_steam_ui_from_the_focusable_lists() {
		let candidates = [
			FocusCandidate {
				window_id: 10,
				app_id: 548_430,
				pid: 1,
			},
			FocusCandidate {
				window_id: 13,
				app_id: 769,
				pid: 2,
			},
		];
		// Without the overlay, both are focusable.
		assert_eq!(focusable_apps(&candidates, false), vec![548_430, 769]);
		assert_eq!(focusable_windows(&candidates, false), vec![10, 548_430, 1, 13, 769, 2]);
		// With the interactive overlay focused, BPM (769) is excluded so Steam
		// routes the gamepad to the overlay.
		assert_eq!(focusable_apps(&candidates, true), vec![548_430]);
		assert_eq!(focusable_windows(&candidates, true), vec![10, 548_430, 1]);
	}

	#[test]
	fn the_focusable_lists_are_empty_without_candidates() {
		assert!(focusable_apps(&[], false).is_empty());
		assert!(focusable_windows(&[], false).is_empty());
		assert!(focusable_apps(&[], true).is_empty());
		assert!(focusable_windows(&[], true).is_empty());
	}

	#[test]
	fn the_xwayland_server_id_encodes_the_display_number() {
		assert_eq!(display_id_to_server_id(0), 100 << 8);
		assert_eq!(display_id_to_server_id(3), (103 << 8) | 3);
	}
}
