//! Focus policy: a stateless port of gamescope's focus selection.
//!
//! gamescope never tracks "who holds the role" incrementally. Every time a
//! window maps, unmaps, commits for the first time or changes a focus-relevant
//! property it bumps a serial (`MakeFocusDirty`, `steamcompmgr.cpp:992`); the
//! main loop then recomputes the whole focus from the window list
//! (`determine_and_apply_focus`, `:5005`, which calls
//! `xwayland_ctx_t::DetermineAndApplyFocus`, `:4509`). This module is that
//! computation as a **pure function** over a snapshot of per-window metadata:
//!
//! - [`WindowMeta`] is the cached state of one window (gamescope's
//!   `steamcompmgr_win_t`). The shell fills it at map time and refreshes it on
//!   `PropertyNotify` / `ConfigureNotify` / commit, so the pass never talks to
//!   X.
//! - [`pick`] is `GetPossibleFocusWindows` + `DetermineAndApplyFocus` (the part
//!   that selects windows) + the global `determine_and_apply_focus` selection.
//!   It returns a [`Picked`]; the shell diffs it against what it applied last
//!   and applies only the difference.
//! - [`is_focus_priority_greater`] and [`pick_primary_focus_and_override`] are
//!   the comparator and the primary/override selection.
//!
//! Every clause carries the `steamcompmgr.cpp:NNNN` line it is ported from
//! (gamescope at `05949f81`). Clauses gamescope has and ferriscope deliberately
//! does not (VR forwarding, per-connector strategies other than the two below,
//! MangoApp message types, streaming-client windows, the system tray, Xalia
//! decoration windows and override underlays) are left out on purpose, not
//! silently dropped.
//!
//! The rest of the file is the parts of the policy that were never an election:
//! the focus dirty serial, the focus-window geometry rule and the
//! pointer-capture state machine.

/// A window id: the X11 window id for an Xwayland window, a synthetic id in the
/// range `0x8000_0000..` for a native Wayland toplevel (X11 ids are 29 bits, so
/// the two never collide and `transient_for` can name either).
pub type WindowId = u32;

/// First synthetic id handed to a native Wayland toplevel.
pub const WAYLAND_ID_BASE: WindowId = 0x8000_0000;

/// `OPAQUE` (`steamcompmgr.cpp:1520`).
pub const OPAQUE: u32 = 0xffff_ffff;

/// `TRANSLUCENT` (`steamcompmgr.cpp:1519`).
pub const TRANSLUCENT: u32 = 0;

/// The app id Steam uses for its own UI (Big Picture), `window_is_steam`
/// (`steamcompmgr.cpp:1291`).
pub const STEAM_BIG_PICTURE_APP_ID: u32 = 769;

/// `WS_DISABLED` (`win32_styles.h:8`).
const WS_DISABLED: u32 = 0x0800_0000;
/// `WS_EX_TRANSPARENT` (`win32_styles.h:36`).
const WS_EX_TRANSPARENT: u32 = 0x0000_0020;
/// `WS_EX_CONTROLPARENT` (`win32_styles.h:48`).
const WS_EX_CONTROLPARENT: u32 = 0x0001_0000;
/// `WS_EX_APPWINDOW` (`win32_styles.h:50`).
const WS_EX_APPWINDOW: u32 = 0x0004_0000;
/// `WS_EX_LAYERED` (`win32_styles.h:51`).
const WS_EX_LAYERED: u32 = 0x0008_0000;

/// What kind of client a window is (gamescope's `steamcompmgr_win_type_t`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowKind {
	/// An Xwayland window (`XWAYLAND`).
	Xwayland,
	/// A native Wayland `xdg_toplevel` (`XDG`).
	Xdg,
}

/// A window's geometry in output space.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Geometry {
	/// Left edge.
	pub x: i32,
	/// Top edge.
	pub y: i32,
	/// Width.
	pub w: i32,
	/// Height.
	pub h: i32,
}

/// The cached state of one window: gamescope's `steamcompmgr_win_t` restricted
/// to what the focus selection reads.
///
/// Filled when the window maps and refreshed on property, configure and commit
/// events; the focus pass and the pointer hit-test read only this. Nothing here
/// is fetched from the X server on the input path.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowMeta {
	/// The window id ([`WindowId`]).
	pub id: WindowId,
	/// Xwayland or native Wayland.
	pub kind: WindowKind,
	/// `xwayland().a.override_redirect` (`:3954`).
	pub override_redirect: bool,
	/// Current geometry (`GetGeometry()`), kept from `ConfigureNotify`.
	pub geometry: Geometry,
	/// `_NET_WM_WINDOW_OPACITY`, [`OPAQUE`] when absent (`:5693`).
	pub opacity: u32,
	/// `appID` (`:3933`). See [`crate::shell`]'s `WindowTable` for how it is
	/// derived and kept in step with `STEAM_GAME` / `STEAM_BIGPICTURE`.
	pub app_id: u32,
	/// `steamAppID`: the raw `STEAM_GAME` value (`:5707`).
	pub steam_app_id: u32,
	/// The client pid (`pid`), 0 when unknown.
	pub pid: u32,
	/// The app id derived from the client's process (`GetAppIdFromPid`), 0 for
	/// none. Only an X11 window in Steam mode has one; `STEAM_GAME` overrides it.
	pub pid_app_id: u32,
	/// `STEAM_OVERLAY` (`isOverlay`).
	pub is_overlay: bool,
	/// `GAMESCOPE_EXTERNAL_OVERLAY` (`isExternalOverlay`).
	pub is_external_overlay: bool,
	/// `STEAM_BIGPICTURE` (`isSteamLegacyBigPicture`).
	pub is_steam_legacy_big_picture: bool,
	/// `STEAM_INPUT_FOCUS` (`inputFocusMode`).
	pub input_focus_mode: u32,
	/// `WM_TRANSIENT_FOR` (`xwayland().transientFor`); for a Wayland toplevel the
	/// id of its `xdg_toplevel.set_parent` parent.
	pub transient_for: Option<WindowId>,
	/// `is_dialog` (`get_win_type`, `:5473`).
	pub is_dialog: bool,
	/// `maybe_a_dropdown` (`get_size_hints`, `:5496`).
	pub maybe_a_dropdown: bool,
	/// `sizeHintsSpecified` (`:5522`).
	pub size_hints_specified: bool,
	/// `requestedWidth` / `requestedHeight` (`:5517`).
	pub requested_size: (u32, u32),
	/// `skipTaskbar`.
	pub skip_taskbar: bool,
	/// `skipPager`.
	pub skip_pager: bool,
	/// `isFullscreen` (`_NET_WM_STATE_FULLSCREEN`).
	pub is_fullscreen: bool,
	/// `_WINE_HWND_STYLE` when the property is present (`hasHwndStyle`).
	pub hwnd_style: Option<u32>,
	/// `_WINE_HWND_EXSTYLE` when the property is present (`hasHwndStyleEx`).
	pub hwnd_style_ex: Option<u32>,
	/// `map_sequence`: a counter taken when the window mapped (`:5771`).
	pub map_seq: u64,
	/// `damage_sequence`: a counter taken on every commit, 0 until the first
	/// (`:6295`).
	pub damage_seq: u64,
	/// `window_has_commits` (`:1868`): a buffer has been committed.
	pub has_commits: bool,
}

impl WindowMeta {
	/// A freshly mapped window with gamescope's defaults (`:5919-5995`).
	pub fn new(id: WindowId, kind: WindowKind) -> Self {
		Self {
			id,
			kind,
			override_redirect: false,
			geometry: Geometry::default(),
			opacity: OPAQUE,
			app_id: 0,
			steam_app_id: 0,
			pid_app_id: 0,
			pid: 0,
			is_overlay: false,
			is_external_overlay: false,
			is_steam_legacy_big_picture: false,
			input_focus_mode: 0,
			transient_for: None,
			is_dialog: false,
			maybe_a_dropdown: false,
			size_hints_specified: false,
			requested_size: (0, 0),
			skip_taskbar: false,
			skip_pager: false,
			is_fullscreen: false,
			hwnd_style: None,
			hwnd_style_ex: None,
			map_seq: 0,
			damage_seq: 0,
			has_commits: false,
		}
	}

	fn is_xwayland(&self) -> bool {
		self.kind == WindowKind::Xwayland
	}

	/// `window_is_steam` (`:1289`): the Steam UI.
	pub fn is_steam(&self) -> bool {
		self.is_steam_legacy_big_picture || self.app_id == STEAM_BIG_PICTURE_APP_ID
	}

	/// `window_is_fullscreen` (`:1921`).
	pub fn is_fullscreen_window(&self) -> bool {
		self.is_steam() || self.is_fullscreen
	}

	/// `win_has_game_id` (`:3931`): `appID != 0`, so the Steam UI (769) counts.
	pub fn has_game_id(&self) -> bool {
		self.app_id != 0
	}

	/// `win_is_useless` (`:3936`): a 1x1 window. (`isSysTrayIcon` is not
	/// tracked.)
	pub fn is_useless(&self) -> bool {
		self.geometry.w == 1 && self.geometry.h == 1
	}

	/// `win_is_override_redirect` (`:3948`). (`ignoreOverrideRedirect`, the SDL
	/// fullscreen-reparent heuristic, is not tracked.)
	pub fn is_override_redirect(&self) -> bool {
		if !self.is_xwayland() {
			return false;
		}
		self.override_redirect && !self.is_useless()
	}

	/// `win_skip_taskbar_and_pager` (`:3957`).
	pub fn skip_taskbar_and_pager(&self) -> bool {
		self.skip_taskbar && self.skip_pager
	}

	/// `win_skip_and_not_fullscreen` (`:3963`).
	pub fn skip_and_not_fullscreen(&self) -> bool {
		self.skip_taskbar_and_pager() && !self.is_fullscreen
	}

	/// `win_maybe_a_dropdown` (`:3969`).
	pub fn maybe_dropdown(&self) -> bool {
		// :3972
		if !self.is_xwayland() {
			return false;
		}

		// :3981 Fallout 4 options menu vs Warframe language dropdown.
		if self.app_id == 230_410
			&& self.maybe_a_dropdown
			&& self.transient_for.is_some()
			&& (self.skip_pager || self.skip_taskbar)
		{
			return !self.is_useless();
		}

		// :3986 Antichamber splash screen.
		if self.app_id == 219_890 {
			return false;
		}

		// :4007-4012 Witcher 2 / Age of Empires 2 launchers: a layered
		// control-parent window that is not an app window is a dropdown.
		let valid_layered = WS_EX_CONTROLPARENT | WS_EX_LAYERED;
		let invalid_layered = WS_EX_APPWINDOW;
		if let Some(ex) = self.hwnd_style_ex
			&& ex & valid_layered == valid_layered
			&& ex & invalid_layered == 0
		{
			return true;
		}

		// :4016 Forza Horizon 4 and 5 black background window.
		if (self.app_id == 1_293_830 || self.app_id == 1_551_360)
			&& self.maybe_a_dropdown
			&& self.requested_size == (0, 0)
		{
			return false;
		}

		// :4031-4033 the general rule (see the comment at :4020-4030).
		let valid_maybe_a_dropdown = self.maybe_a_dropdown
			&& (!self.is_dialog || (self.transient_for.is_none() && self.skip_and_not_fullscreen()))
			&& (self.skip_pager || self.skip_taskbar);
		(valid_maybe_a_dropdown || self.is_override_redirect()) && !self.is_useless()
	}

	/// `win_is_disabled` (`:4036`): Wine's `WS_DISABLED`.
	pub fn is_disabled(&self) -> bool {
		match self.hwnd_style {
			Some(style) => style & WS_DISABLED != 0,
			None => false,
		}
	}
}

/// Which `backend_virtual_connector_strategy` the focus pass runs under
/// (`backend.h:40`). Only the two single-output strategies are ported. The
/// shell derives it from [`crate::CompositorConfig::steam`]: Steam mode is
/// Steam-controlled focus, otherwise single-application.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum FocusStrategy {
	/// gamescope's default: the best-ranked window wins, and the root-window
	/// focus-control atoms only restrict which override windows are eligible
	/// (`:4254`, `:4290`, `:4381`).
	#[default]
	SingleApplication,
	/// Steam picks the base layer: `GAMESCOPECTRL_BASELAYER_WINDOW` /
	/// `GAMESCOPECTRL_BASELAYER_APPID` name the focus window (`:4257-4287`) and
	/// windows with neither a game id nor the Steam UI are not focusable
	/// (`:4483`).
	SteamControlled,
}

/// Steam's root-window focus control (`GAMESCOPECTRL_BASELAYER_WINDOW` and
/// `GAMESCOPECTRL_BASELAYER_APPID`, `:6881-6886`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FocusControl {
	/// `focusControlWindow`: the window Steam wants as the base layer.
	pub window: Option<WindowId>,
	/// `vecFocuscontrolAppIDs`: the app ids Steam wants, in preference order.
	pub app_ids: Vec<u32>,
}

/// Returns true if `a`'s focus priority is greater than `b`'s
/// (`is_focus_priority_greater`, `:4058`).
///
/// The criteria are checked in order; the first that differs decides. A tie
/// falls through to the next.
pub fn is_focus_priority_greater(a: &WindowMeta, b: &WindowMeta) -> bool {
	// :4060 a window with a game id beats one without; the Steam UI (769) has one.
	if a.has_game_id() != b.has_game_id() {
		return a.has_game_id();
	}

	// :4066 prefer the non-override-redirect window.
	if a.is_override_redirect() != b.is_override_redirect() {
		return !a.is_override_redirect();
	}

	// :4070 prefer anything over a 1x1 window.
	if a.is_useless() != b.is_useless() {
		return !a.is_useless();
	}

	// :4073 prefer a window that is not a dropdown.
	if a.maybe_dropdown() != b.maybe_dropdown() {
		return !a.maybe_dropdown();
	}

	// :4076 prefer an enabled Wine window.
	if a.is_disabled() != b.is_disabled() {
		return !a.is_disabled();
	}

	// :4081 Wine sets SKIP_TASKBAR and SKIP_PAGER for WS_EX_NOACTIVATE windows.
	if a.skip_and_not_fullscreen() != b.skip_and_not_fullscreen() {
		return !a.skip_and_not_fullscreen();
	}

	// :4086 among dropdowns, prefer normal windows over dialogs.
	if a.maybe_dropdown() && b.maybe_dropdown() && a.is_dialog != b.is_dialog {
		return !a.is_dialog;
	}

	// :4090 gamescope answers `true` for every tie between non-Xwayland
	// windows, which makes the *later* of two native Wayland toplevels win.
	// ferriscope keeps the established order instead (the earlier toplevel
	// keeps focus, a later peer never steals it); this is the one deliberate
	// departure from the comparator, pinned by
	// `a_later_wayland_toplevel_does_not_displace_the_earlier_one`.
	if !a.is_xwayland() {
		return false;
	}

	// :4096 tie-break dropdowns by transient-for.
	if a.maybe_dropdown() && b.maybe_dropdown() && a.transient_for.is_none() != b.transient_for.is_none() {
		return a.transient_for.is_none();
	}

	// :4102 a newly mapped window (or sibling menu) displaces an older one.
	if (a.has_game_id() || (a.maybe_dropdown() && b.maybe_dropdown())) && a.map_seq != b.map_seq {
		return a.map_seq > b.map_seq;
	}

	// :4107 the damage sequence only matters for game windows.
	if a.has_game_id() && a.damage_seq != b.damage_seq {
		return a.damage_seq > b.damage_seq;
	}

	false
}

/// `windows_share_app` (`:4113`).
fn windows_share_app(a: &WindowMeta, b: &WindowMeta) -> bool {
	(a.app_id != 0 && a.app_id == b.app_id) || (a.steam_app_id != 0 && a.steam_app_id == b.steam_app_id)
}

/// `is_same_app_override_decoration` (`:4119`): a non-interactive transparent
/// same-app popup from another process (Xalia's highlight overlay).
fn is_same_app_override_decoration(candidate: &WindowMeta, focus: &WindowMeta) -> bool {
	if candidate.pid == focus.pid || !windows_share_app(candidate, focus) {
		return false;
	}
	let decoration = WS_EX_LAYERED | WS_EX_TRANSPARENT;
	candidate.is_override_redirect() && candidate.hwnd_style_ex.is_some_and(|ex| ex & decoration == decoration)
}

/// `is_good_override_candidate` (`:4132`).
fn is_good_override_candidate(candidate: &WindowMeta, focus: &WindowMeta) -> bool {
	// :4149 non-interactive same-app helpers are decorations, not overrides.
	if is_same_app_override_decoration(candidate, focus) {
		return false;
	}

	// :4153 an override must belong to the focus window's process or app.
	if candidate.pid != focus.pid && !windows_share_app(candidate, focus) {
		return false;
	}

	// :4156 and must be on screen (some dropdowns close when focused off-screen).
	let rect = candidate.geometry;
	candidate.id != focus.id && rect.x + rect.w > 0 && rect.y + rect.h > 0
}

/// `win_treat_as_per_window` (`:4210`) for the two ported strategies: never.
fn win_treat_as_per_window(_focus: &WindowMeta, _strategy: FocusStrategy) -> bool {
	false
}

/// The result of one [`pick_primary_focus_and_override`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Primary {
	/// `out->focusWindow` after the commit gating, `None` when gamescope leaves
	/// the field as it was.
	focus: Option<WindowId>,
	/// The window the focus window is a dialog of (itself when it is none).
	/// ferriscope applies the fullscreen state to this window, so a dialog that
	/// becomes the focus window does not steal `_NET_WM_STATE_FULLSCREEN` from
	/// its parent.
	role: Option<WindowId>,
	/// `out->overrideWindow`.
	override_window: Option<WindowId>,
}

fn find<'a>(windows: &[&'a WindowMeta], id: WindowId) -> Option<&'a WindowMeta> {
	windows.iter().copied().find(|window| window.id == id)
}

/// `resolveTransientOverrides` (`:4328`): follow `WM_TRANSIENT_FOR` links from the
/// focus window (or the override found so far) to dropdowns that hang off it.
/// `maybe` selects `win_maybe_a_dropdown` over `win_is_override_redirect` as the
/// qualifying test (`:4337`).
fn resolve_transient_overrides<'a>(
	possible: &[&'a WindowMeta],
	focus: &WindowMeta,
	override_focus: &mut Option<&'a WindowMeta>,
	maybe: bool,
) {
	// :4330
	if !focus.is_xwayland() {
		return;
	}
	// Hopefully there are no transient cycles (gamescope's own caveat, `:4352`);
	// the loop is bounded by the candidate count regardless.
	for _ in 0..=possible.len() {
		let current_id = override_focus.map(|window| window.id);
		let parent_id = current_id.unwrap_or(focus.id);
		let found = possible.iter().copied().find(|candidate| {
			// :4335
			if !candidate.is_xwayland() {
				return false;
			}
			let is_dropdown = if maybe {
				candidate.maybe_dropdown()
			} else {
				candidate.is_override_redirect()
			};
			// :4341
			Some(candidate.id) != current_id
				&& candidate.id != focus.id
				&& candidate.transient_for == Some(parent_id)
				&& is_dropdown
		});
		match found {
			Some(candidate) => *override_focus = Some(candidate),
			None => break,
		}
	}
}

/// The window at the top of `window`'s `WM_TRANSIENT_FOR` chain among the
/// candidates, `window` itself when it has none.
fn transient_root(possible: &[&WindowMeta], window: &WindowMeta) -> WindowId {
	let mut current = window;
	// Bounded by the candidate count in case of a cycle.
	for _ in 0..=possible.len() {
		match current
			.transient_for
			.and_then(|parent| possible.iter().copied().find(|candidate| candidate.id == parent))
		{
			Some(parent) => current = parent,
			None => break,
		}
	}
	current.id
}

/// `pick_primary_focus_and_override` (`:4242`).
///
/// `possible` is the sorted candidate list; `global` is `globalFocus` (the
/// cross-context pass, where a window without commits does not become the
/// presented focus window).
fn pick_primary_focus_and_override(
	possible: &[&WindowMeta],
	control: &FocusControl,
	global: bool,
	strategy: FocusStrategy,
) -> Primary {
	let mut focus: Option<&WindowMeta> = None;
	let mut override_focus: Option<&WindowMeta> = None;

	// :4254
	let controlled_focus =
		strategy != FocusStrategy::SingleApplication || control.window.is_some() || !control.app_ids.is_empty();
	if controlled_focus {
		if strategy == FocusStrategy::SteamControlled {
			// :4259 Steam names the base window.
			if let Some(control_window) = control.window {
				focus = possible
					.iter()
					.copied()
					.find(|window| window.is_xwayland() && window.id == control_window);
			}
			// :4275 or the base app, in Steam's order.
			if focus.is_none() {
				'apps: for app_id in &control.app_ids {
					for window in possible {
						if window.app_id == *app_id {
							focus = Some(window);
							break 'apps;
						}
					}
				}
			}
		} else {
			// :4290 `GetVirtualConnectorKey` is 0 for `SingleApplication`, so the
			// first candidate matches the (single) connector key.
			focus = possible.first().copied();
		}
	}

	// :4304. Deviates from gamescope: the Steam updater (bootstrapper) runs
	// before Steam writes any focus control, so fall back to it too, or it is never shown.
	let no_steam_control = control.window.is_none() && control.app_ids.is_empty();
	if focus.is_none() && (!global || !controlled_focus || no_steam_control) {
		focus = possible.first().copied();
	}

	// :4357
	if let Some(mut current) = focus
		&& current.is_xwayland()
	{
		// :4361 follow transient links for the focus window itself.
		if control.window.is_none() && !win_treat_as_per_window(current, strategy) {
			for _ in 0..=possible.len() {
				let next = possible.iter().copied().find(|candidate| {
					candidate.is_xwayland()
						&& candidate.id != current.id
						&& candidate.transient_for == Some(current.id)
						&& !candidate.maybe_dropdown()
				});
				match next {
					Some(candidate) => current = candidate,
					None => break,
				}
			}
			focus = Some(current);
		}

		// :4375 pick an override-redirect window as the override.
		if override_focus.is_none() {
			if !control.app_ids.is_empty() {
				override_focus = possible.iter().copied().find(|window| {
					window.is_override_redirect()
						&& is_good_override_candidate(window, current)
						&& window.app_id == current.app_id
				});
			} else if !possible.is_empty() {
				override_focus = possible
					.iter()
					.copied()
					.find(|window| window.is_override_redirect() && is_good_override_candidate(window, current));
			}
			resolve_transient_overrides(possible, current, &mut override_focus, false);
		}
	}

	// ferriscope: the fullscreen state belongs to the window the focus window is
	// a dialog of, found by walking `WM_TRANSIENT_FOR` up. gamescope has no such
	// state, so this is ferriscope's own: a dialog that becomes the focus window
	// must not take `_NET_WM_STATE_FULLSCREEN` from the window it belongs to.
	let role = focus.map(|window| transient_root(possible, window));

	// :4402
	let mut out_focus = None;
	if let Some(window) = focus {
		if window.has_commits {
			out_focus = Some(window.id);
		}
		// :4417 ctx passes always present the chosen window; the global pass
		// waits for its first commit (`outdatedInteractiveFocus` re-dirties
		// focus once it has one, `:7931`).
		if !global {
			out_focus = Some(window.id);
		}
	}

	// :4420 fall back to a dropdown that is not override-redirect.
	if override_focus.is_none()
		&& let Some(focus) = focus
	{
		if controlled_focus {
			// :4424
			'apps: for app_id in &control.app_ids {
				for fake_override in possible {
					if fake_override.app_id == *app_id
						&& fake_override.maybe_dropdown()
						&& is_good_override_candidate(fake_override, focus)
						&& fake_override.app_id == focus.app_id
					{
						override_focus = Some(fake_override);
						break 'apps;
					}
				}
			}
		} else {
			// :4440
			override_focus = possible.iter().copied().find(|fake_override| {
				fake_override.maybe_dropdown() && is_good_override_candidate(fake_override, focus)
			});
		}
		resolve_transient_overrides(possible, focus, &mut override_focus, true);
	}

	// ferriscope extension (native Wayland only; gamescope has no XDG
	// override concept): a toplevel parented to the focus window is drawn over
	// it and takes the keyboard, as `xdg_toplevel.set_parent` dialogs always
	// have here. Pinned by `wayland_transient_takes_focus`.
	if override_focus.is_none()
		&& let Some(focus) = focus
		&& !focus.is_xwayland()
	{
		override_focus = possible
			.iter()
			.copied()
			.find(|window| !window.is_xwayland() && window.transient_for == Some(focus.id));
	}

	Primary {
		focus: out_focus,
		role,
		override_window: override_focus.map(|window| window.id),
	}
}

/// `GetPossibleFocusWindows` (`:4459`) for the Xwayland list, and the sort of
/// `GetGlobalPossibleFocusWindows` (`:4902`) for the combined one.
///
/// `windows` is gamescope's list order (topmost first). The sort is a stable
/// insertion sort, which is what `std::stable_sort` degenerates to for the
/// handful of windows a session has; unlike `slice::sort_by` it tolerates a
/// comparator that is not a strict weak order.
fn sort_by_priority(windows: Vec<&WindowMeta>) -> Vec<&WindowMeta> {
	let mut sorted: Vec<&WindowMeta> = Vec::with_capacity(windows.len());
	for window in windows {
		let mut index = sorted.len();
		while index > 0 && is_focus_priority_greater(window, sorted[index - 1]) {
			index -= 1;
		}
		sorted.insert(index, window);
	}
	sorted
}

/// `xwayland_ctx_t::GetPossibleFocusWindows` (`:4459`).
fn possible_focus_windows<'a>(x11: &[&'a WindowMeta], strategy: FocusStrategy) -> Vec<&'a WindowMeta> {
	let mut possible = Vec::new();
	for window in x11 {
		// :4465 overlays never take focus.
		if window.is_overlay || window.is_external_overlay {
			continue;
		}

		// :4483 under Steam-controlled focus only Steam's own windows count.
		if strategy == FocusStrategy::SteamControlled && !(window.has_game_id() || window.is_steam()) {
			continue;
		}

		// :4491 viewable windows that are not fully transparent. (`InputOutput`
		// is implied: an `InputOnly` window never reaches the table.)
		if window.opacity > TRANSLUCENT {
			possible.push(*window);
		}
	}
	sort_by_priority(possible)
}

/// What the X side of the pass applies (`DetermineAndApplyFocus`, `:4509`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CtxPick {
	/// `ctx->focus.focusWindow`.
	pub focus: Option<WindowId>,
	/// The window the fullscreen state belongs to (see [`Primary::role`]).
	pub role: Option<WindowId>,
	/// `ctx->focus.overrideWindow`.
	pub override_window: Option<WindowId>,
	/// `ctx->focus.overlayWindow`: the interactive Steam overlay.
	pub overlay: Option<WindowId>,
	/// `ctx->focus.notificationWindow`.
	pub notification: Option<WindowId>,
	/// `ctx->focus.externalOverlayWindow`.
	pub external_overlay: Option<WindowId>,
	/// `inputFocus` before the override swap (`:4547`, `:4588`).
	pub input_focus: Option<WindowId>,
	/// `keyboardFocusWin` (`:4611`): where `XSetInputFocus` goes.
	pub keyboard_focus: Option<WindowId>,
	/// The window `Raise` is applied to (`:4728`, `:4784-4789`).
	pub raise: Option<WindowId>,
}

/// What the presentation side of the pass applies (the global
/// `determine_and_apply_focus`, `:5005`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GlobalPick {
	/// `pFocus->focusWindow`: the base layer. Held at its previous value until
	/// the chosen window has committed a buffer.
	pub focus: Option<WindowId>,
	/// The window the fullscreen state belongs to.
	pub role: Option<WindowId>,
	/// `pFocus->overrideWindow`: the dropdown or menu drawn over the base.
	pub override_window: Option<WindowId>,
	/// `pFocus->overlayWindow`.
	pub overlay: Option<WindowId>,
	/// `pFocus->notificationWindow`.
	pub notification: Option<WindowId>,
	/// `pFocus->externalOverlayWindow`.
	pub external_overlay: Option<WindowId>,
	/// `pFocus->inputFocusWindow`: the pointer's window.
	pub input_focus: Option<WindowId>,
	/// `pFocus->keyboardFocusWindow`: the Wayland keyboard's window.
	pub keyboard_focus: Option<WindowId>,
}

/// Everything one pass decides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Picked {
	/// The Xwayland side.
	pub ctx: CtxPick,
	/// The presentation side.
	pub global: GlobalPick,
	/// The `GAMESCOPE_FOCUSABLE_*` candidates (`:5048-5073`), in order.
	pub focusable: Vec<WindowId>,
}

/// What the previous pass left behind, which gamescope reads back through
/// `ctx->focus` / `pFocus`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PreviousFocus {
	/// `ctx->focus.focusWindow` (kept when the pick finds no window, `:4512`).
	pub ctx_focus: Option<WindowId>,
	/// The previous global focus window (`:5013`).
	pub global_focus: Option<WindowId>,
}

/// The whole focus selection: `DetermineAndApplyFocus` plus the global
/// `determine_and_apply_focus`, for one output.
///
/// `x11` is the Xwayland window list, topmost first (`ctx->list`); `xdg` the
/// native Wayland toplevels in creation order (`g_steamcompmgr_xdg_wins`).
/// `root_width` is the output width. Pure: it reads only its arguments.
pub fn pick(
	x11: &[&WindowMeta],
	xdg: &[&WindowMeta],
	root_width: i32,
	control: &FocusControl,
	strategy: FocusStrategy,
	previous: PreviousFocus,
) -> Picked {
	// ---- xwayland_ctx_t::DetermineAndApplyFocus (:4509) --------------------
	let x11_possible = possible_focus_windows(x11, strategy);

	let mut overlay: Option<&WindowMeta> = None;
	let mut notification: Option<&WindowMeta> = None;
	let mut external_overlay: Option<&WindowMeta> = None;
	let mut input_focus: Option<&WindowMeta> = None;
	let mut max_opacity = 0;
	let mut max_opacity_external = 0;
	for window in x11 {
		if window.is_overlay {
			// :4527 the interactive overlay spans the root width or asks for input.
			if (window.geometry.w >= root_width || window.input_focus_mode != 0) && window.opacity >= max_opacity {
				overlay = Some(window);
				max_opacity = window.opacity;
			} else {
				notification = Some(window);
			}
		}
		// :4540 (MangoApp message types are not tracked).
		if window.is_external_overlay && window.opacity > max_opacity_external {
			external_overlay = Some(window);
			max_opacity_external = window.opacity;
		}
		// :4553 single output: an overlay that wants input is the input focus.
		if window.is_overlay && window.input_focus_mode != 0 {
			input_focus = Some(window);
		}
	}

	// :4573 the per-context pick (connector key 0).
	let ctx_primary = pick_primary_focus_and_override(&x11_possible, control, false, strategy);
	// `ctx->focus.focusWindow` is only assigned when a window was picked, so it
	// keeps its old value otherwise (`:4512`); a window that is gone is gone.
	let ctx_focus = ctx_primary
		.focus
		.or(previous.ctx_focus.filter(|id| find(x11, *id).is_some()));

	let mut ctx = CtxPick {
		focus: ctx_focus,
		role: ctx_primary.role,
		override_window: ctx_primary.override_window,
		overlay: overlay.map(|window| window.id),
		notification: notification.map(|window| window.id),
		external_overlay: external_overlay.map(|window| window.id),
		..CtxPick::default()
	};

	// :4578 / :4661 with no focus window the pass stops here.
	if let Some(focus_id) = ctx_focus {
		// :4578
		let input_focus_id = input_focus.map(|window| window.id).unwrap_or(focus_id);
		ctx.input_focus = Some(input_focus_id);

		// :4611 the keyboard goes to the input window, except in mode 2
		// where the overlay only takes the pointer.
		let mut keyboard_focus = input_focus_id;
		if find(x11, input_focus_id).is_some_and(|window| window.input_focus_mode == 2) {
			keyboard_focus = focus_id;
		}
		ctx.keyboard_focus = Some(keyboard_focus);

		// :4728 when the base window is also the input focus its override takes
		// the pointer, and that window is the one raised (:4784-4789).
		ctx.raise = if input_focus_id == focus_id && ctx_primary.override_window.is_some() {
			ctx_primary.override_window
		} else {
			Some(input_focus_id)
		};
	}

	// ---- determine_and_apply_focus (:5005) ---------------------------------
	// :5041 GetGlobalPossibleFocusWindows: the Xwayland list followed by the
	// native Wayland toplevels (:4889 skips overlays), sorted again.
	let mut combined = x11_possible.clone();
	for window in xdg {
		if window.is_overlay || window.is_external_overlay {
			continue;
		}
		combined.push(*window);
	}
	let combined = sort_by_priority(combined);

	// :5048 the windows reported to Steam as focusable. gamescope reports only
	// Xwayland windows; ferriscope also reports native Wayland ones, whose app
	// id goes into `GAMESCOPE_FOCUSABLE_APPS` (they have no X id for
	// `GAMESCOPE_FOCUSABLE_WINDOWS`), so Steam learns a Wayland game has a
	// window and puts it ahead of its own UI in the base layer.
	let focusable = combined
		.iter()
		.filter(|window| !(window.is_useless() || window.skip_and_not_fullscreen() || window.override_redirect))
		.map(|window| window.id)
		.collect();

	// :5084 the global pick, with the focus window held at its previous value
	// until the new one has committed (:5013).
	let global_primary = pick_primary_focus_and_override(&combined, control, true, strategy);
	let global_focus = global_primary.focus.or(previous
		.global_focus
		.filter(|id| find(x11, *id).is_some() || find(xdg, *id).is_some()));

	// :5094 the overlay and notification come from the Xwayland context.
	let global_overlay = ctx.overlay;
	let global_notification = ctx.notification;
	let global_external = ctx.external_overlay;

	// :5141 pick the input and keyboard focus windows (single output).
	let overlay_wants_input = global_overlay
		.and_then(|id| find(x11, id))
		.is_some_and(|window| window.input_focus_mode != 0);
	let (global_input, mut global_keyboard);
	if overlay_wants_input {
		global_input = global_overlay;
		global_keyboard = global_overlay;
	} else {
		global_input = global_focus;
		global_keyboard = global_primary.override_window.or(global_focus);
	}

	// :5206 `inputFocusMode == 2`: the pointer is the overlay's but the keyboard
	// stays with the application.
	let input_mode = global_input
		.and_then(|id| find(x11, id).or_else(|| find(xdg, id)))
		.map(|window| window.input_focus_mode)
		.unwrap_or(0);
	if input_mode == 2 {
		global_keyboard = global_primary.override_window.or(global_focus);
	}

	let global = GlobalPick {
		focus: global_focus,
		role: global_primary.role,
		override_window: global_primary.override_window,
		overlay: global_overlay,
		notification: global_notification,
		external_overlay: global_external,
		input_focus: global_input,
		keyboard_focus: global_keyboard,
	};

	Picked { ctx, global, focusable }
}

/// gamescope's focus dirty serial (`s_ulFocusSerial` + `focus_t::IsDirty`,
/// `steamcompmgr.cpp:991-1005`).
///
/// gamescope bumps a serial with `MakeFocusDirty()` on every map, unmap and
/// focus-affecting property change, and the main loop re-runs
/// `DetermineAndApplyFocus` **only while the serial an observer last applied
/// lags the current one** (`:5030`). ferriscope has a single focus
/// observer, so the current serial and the one the pass last recorded live
/// together; a pass reads [`Self::serial`] before it runs and records it with
/// [`Self::applied`] when it finishes, so a change made *during* the pass is
/// not silently swallowed.
#[derive(Debug, Default)]
pub struct FocusDirty {
	serial: u64,
	applied: u64,
}

impl FocusDirty {
	/// A fresh, clean serial.
	pub fn new() -> Self {
		Self::default()
	}

	/// `MakeFocusDirty()`: bump the serial so the next pass is due.
	pub fn mark(&mut self) {
		self.serial = self.serial.wrapping_add(1);
	}

	/// `focus_t::IsDirty()`: whether a pass is due.
	pub fn is_dirty(&self) -> bool {
		self.applied != self.serial
	}

	/// The serial a pass should record when it runs (gamescope's
	/// `GetFocusSerial()`).
	pub fn serial(&self) -> u64 {
		self.serial
	}

	/// Record that a pass ran against `serial` (`focus_t::ulCurrentFocusSerial`).
	pub fn applied(&mut self, serial: u64) {
		self.applied = serial;
	}
}

/// gamescope's `DetermineAndApplyFocus` geometry rule for the focus window
/// (`steamcompmgr.cpp:4816-4841`).
///
/// The window only takes part when it has a game id (`win_has_game_id`,
/// `appID != 0`). A `win_has_game_id` window that is fullscreen
/// (`window_is_fullscreen` = `window_is_steam(w) || w->isFullscreen`) or when
/// `force_windows_fullscreen` is on is resized to the root; otherwise it is
/// resized back to its requested size when size hints were specified. Anything
/// else is left alone (gamescope's `handle_desktop_window`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusGeometry {
	/// Resize the focus window to the output/root (the force-fullscreen branch).
	FillOutput,
	/// Resize the focus window back to its requested size (the windowed branch).
	Requested,
	/// Leave the focus window at the size its client chose.
	Keep,
}

/// The geometry branch for a focus window, mirroring gamescope's rule.
///
/// Pure so the branch selection is unit-tested without an X server. The caller
/// scopes the actual `XResizeWindow` to the windows ferriscope resizes; see
/// [`crate::shell`].
pub fn focus_geometry(
	win_has_game_id: bool,
	window_is_fullscreen: bool,
	force_windows_fullscreen: bool,
	has_requested_size: bool,
) -> FocusGeometry {
	if !win_has_game_id {
		return FocusGeometry::Keep;
	}
	if window_is_fullscreen || force_windows_fullscreen {
		FocusGeometry::FillOutput
	} else if has_requested_size {
		FocusGeometry::Requested
	} else {
		FocusGeometry::Keep
	}
}

/// Pointer-capture state from `zwp_pointer_constraints`.
///
/// A locked pointer is held in place and only relative motion reaches the
/// client; a confined pointer may move but is clamped to its region. Capture
/// affects **position only**: cursor visibility is the client's business — an
/// image is drawn whether the pointer is unconstrained, confined or locked, and
/// only the client hiding it (or supplying none) clears it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PointerCapture {
	/// No active constraint.
	#[default]
	None,
	/// Confined: absolute motion is clamped, relative motion still flows.
	Confined,
	/// Locked in place: absolute motion is ignored, relative motion flows.
	Locked,
}

impl PointerCapture {
	/// Whether a constraint is active.
	pub fn is_active(self) -> bool {
		!matches!(self, Self::None)
	}

	/// Whether the pointer is locked in place.
	///
	/// A locked pointer is held on **relative** motion: `apply_input` skips the
	/// cursor advance for `PointerMotionDelta` and seeds focus when
	/// there is none. Absolute motion is still delivered — gamescope's
	/// `wlserver_mousewarp` does the same.
	pub fn is_locked(self) -> bool {
		matches!(self, Self::Locked)
	}

	/// Whether absolute motion is clamped to a region.
	pub fn is_confined(self) -> bool {
		matches!(self, Self::Confined)
	}
}

/// A `wl_region` rectangle in surface-local coordinates:
/// `(add, x, y, width, height)`.
pub type RegionRect = (bool, i32, i32, i32, i32);

/// Whether `point` is inside an ordered add/subtract rectangle region.
///
/// Mirrors the semantics Smithay's `RegionAttributes::contains` uses for the
/// constraint region: a later rectangle overrides an earlier one, and the
/// right/bottom edges are exclusive.
fn region_contains(point: (f64, f64), rects: &[RegionRect]) -> bool {
	let mut contains = false;
	for &(add, x, y, width, height) in rects {
		if point.0 >= f64::from(x)
			&& point.0 < f64::from(x.saturating_add(width))
			&& point.1 >= f64::from(y)
			&& point.1 < f64::from(y.saturating_add(height))
		{
			contains = add;
		}
	}
	contains
}

/// Clamp `point` into a `wl_region`'s allowed area, for pointer confinement.
///
/// A point already inside is returned unchanged. Otherwise it is projected onto
/// the nearest added rectangle, landing on the last coordinate inside the
/// exclusive right/bottom edge. Subtractive rectangles only disqualify a
/// candidate, so a region with holes is not handled exactly; confine regions
/// are additive in practice. An empty or fully subtracted region returns the
/// point unchanged.
pub fn clamp_to_region(point: (f64, f64), rects: &[RegionRect]) -> (f64, f64) {
	if region_contains(point, rects) {
		return point;
	}
	let mut best: Option<(f64, f64, f64)> = None;
	for &(add, x, y, width, height) in rects {
		if !add || width <= 0 || height <= 0 {
			continue;
		}
		let clamped = (
			point
				.0
				.clamp(f64::from(x), f64::from(x.saturating_add(width).saturating_sub(1))),
			point
				.1
				.clamp(f64::from(y), f64::from(y.saturating_add(height).saturating_sub(1))),
		);
		if !region_contains(clamped, rects) {
			continue;
		}
		let distance = (clamped.0 - point.0).powi(2) + (clamped.1 - point.1).powi(2);
		if best.is_none_or(|(best_distance, _, _)| distance < best_distance) {
			best = Some((distance, clamped.0, clamped.1));
		}
	}
	match best {
		Some((_, x, y)) => (x, y),
		None => point,
	}
}

/// Clip a `wl_region` to a `width` x `height` surface bounds rectangle.
///
/// Pointer confinement is the intersection of the client's region with the
/// surface, not the region alone (wlroots: `pointer_constraint_commit` in
/// `types/wlr_pointer_constraints_v1.c` intersects the region with the
/// surface's input region). A region larger than the surface would otherwise
/// clamp the pointer past the surface edge, where it loses focus and the
/// compositor drops the capture. Added and subtracted rectangles are clipped in
/// order; empty intersections are dropped.
pub fn clip_region_to_bounds(rects: &[RegionRect], width: i32, height: i32) -> Vec<RegionRect> {
	if width <= 0 || height <= 0 {
		return Vec::new();
	}
	let mut clipped = Vec::with_capacity(rects.len());
	for &(add, x, y, w, h) in rects {
		if w <= 0 || h <= 0 {
			continue;
		}
		let x0 = x.max(0);
		let y0 = y.max(0);
		let x1 = x.saturating_add(w).min(width);
		let y1 = y.saturating_add(h).min(height);
		if x1 > x0 && y1 > y0 {
			clipped.push((add, x0, y0, x1 - x0, y1 - y0));
		}
	}
	clipped
}

/// The whole surface as an additive confine region.
pub fn surface_region(width: i32, height: i32) -> Vec<RegionRect> {
	if width <= 0 || height <= 0 {
		Vec::new()
	} else {
		vec![(true, 0, 0, width, height)]
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	// ------------------------------------------------------------------
	// Focus selection (pure): one test per gamescope criterion.
	// ------------------------------------------------------------------

	/// A mapped, committed, on-screen Xwayland window whose app id is its id
	/// (gamescope without `--steam`) and whose map order follows its id.
	fn win(id: WindowId) -> WindowMeta {
		let mut meta = WindowMeta::new(id, WindowKind::Xwayland);
		meta.app_id = id;
		meta.pid = 100;
		meta.geometry = Geometry {
			x: 0,
			y: 0,
			w: 640,
			h: 480,
		};
		meta.map_seq = u64::from(id);
		meta.has_commits = true;
		meta
	}

	fn xdg_win(id: WindowId) -> WindowMeta {
		let mut meta = WindowMeta::new(WAYLAND_ID_BASE + id, WindowKind::Xdg);
		meta.geometry = Geometry {
			x: 0,
			y: 0,
			w: 640,
			h: 480,
		};
		meta.map_seq = u64::from(id);
		meta.has_commits = true;
		meta
	}

	fn greater(a: &WindowMeta, b: &WindowMeta) -> bool {
		is_focus_priority_greater(a, b)
	}

	/// `a` ranks strictly above `b`, and not the other way round.
	fn assert_ranks_above(a: &WindowMeta, b: &WindowMeta) {
		assert!(greater(a, b), "{a:?} should outrank {b:?}");
		assert!(!greater(b, a), "{b:?} should not outrank {a:?}");
	}

	/// Neither outranks the other.
	fn assert_tie(a: &WindowMeta, b: &WindowMeta) {
		assert!(!greater(a, b) && !greater(b, a), "{a:?} and {b:?} should tie");
	}

	fn run_with(
		x11: &[WindowMeta],
		control: &FocusControl,
		strategy: FocusStrategy,
		previous: PreviousFocus,
	) -> Picked {
		let refs: Vec<&WindowMeta> = x11.iter().collect();
		pick(&refs, &[], 1280, control, strategy, previous)
	}

	fn run(x11: &[WindowMeta]) -> Picked {
		run_with(
			x11,
			&FocusControl::default(),
			FocusStrategy::SingleApplication,
			PreviousFocus::default(),
		)
	}

	#[test]
	fn clause_4060_a_game_id_beats_none_and_the_steam_ui_has_one() {
		let game = {
			let mut meta = win(1);
			meta.app_id = 440;
			meta
		};
		let plain = {
			let mut meta = win(2);
			meta.app_id = 0;
			meta
		};
		assert_ranks_above(&game, &plain);
		// STEAM_BIGPICTURE is app id 769 and therefore a game id: this is the bug
		// that used to drop the Steam UI out of the game tier.
		let mut steam_ui = win(3);
		steam_ui.is_steam_legacy_big_picture = true;
		steam_ui.app_id = STEAM_BIG_PICTURE_APP_ID;
		assert!(steam_ui.has_game_id() && steam_ui.is_steam());
		assert_ranks_above(&steam_ui, &plain);
	}

	#[test]
	fn clause_4066_override_redirect_loses() {
		let normal = win(1);
		let mut popup = win(2);
		popup.override_redirect = true;
		assert_ranks_above(&normal, &popup);
	}

	#[test]
	fn clause_4070_a_1x1_window_is_useless_and_loses() {
		let mut tiny = win(2);
		tiny.geometry.w = 1;
		tiny.geometry.h = 1;
		assert!(tiny.is_useless());
		assert_ranks_above(&win(1), &tiny);
		// An override-redirect 1x1 window is not override-redirect (:3954).
		tiny.override_redirect = true;
		assert!(!tiny.is_override_redirect());
	}

	#[test]
	fn clause_4073_a_dropdown_loses() {
		let normal = win(1);
		let mut dropdown = win(2);
		dropdown.maybe_a_dropdown = true;
		dropdown.skip_taskbar = true;
		assert!(dropdown.maybe_dropdown());
		assert_ranks_above(&normal, &dropdown);
	}

	#[test]
	fn clause_4076_a_wine_ws_disabled_window_loses() {
		let enabled = win(1);
		let mut disabled = win(2);
		disabled.hwnd_style = Some(WS_DISABLED | 0x1);
		assert!(disabled.is_disabled());
		assert_ranks_above(&enabled, &disabled);
		// No style property at all is not disabled.
		assert!(!win(3).is_disabled());
	}

	#[test]
	fn clause_4081_skip_taskbar_and_pager_loses_unless_fullscreen() {
		let normal = win(1);
		let mut skipped = win(2);
		skipped.skip_taskbar = true;
		skipped.skip_pager = true;
		assert_ranks_above(&normal, &skipped);
		// Only one of the two hints does not count.
		let mut half = win(3);
		half.skip_taskbar = true;
		assert!(!half.skip_and_not_fullscreen());
		// A fullscreen window with both hints is a real window.
		skipped.is_fullscreen = true;
		assert!(!skipped.skip_and_not_fullscreen());
	}

	#[test]
	fn clause_4086_between_dropdowns_a_dialog_loses() {
		let mut menu = win(1);
		menu.maybe_a_dropdown = true;
		menu.skip_pager = true;
		let mut dialog = win(2);
		dialog.maybe_a_dropdown = true;
		dialog.skip_pager = true;
		dialog.is_dialog = true;
		dialog.skip_taskbar = true;
		assert!(menu.maybe_dropdown() && dialog.maybe_dropdown());
		assert_ranks_above(&menu, &dialog);
	}

	#[test]
	fn a_later_wayland_toplevel_does_not_displace_the_earlier_one() {
		// :4090 gamescope answers `true` here and so lets the later toplevel win;
		// ferriscope deliberately keeps the earlier one (documented deviation).
		let first = xdg_win(1);
		let second = xdg_win(2);
		assert_tie(&first, &second);
		let picked = pick(
			&[],
			&[&first, &second],
			1280,
			&FocusControl::default(),
			FocusStrategy::SingleApplication,
			PreviousFocus::default(),
		);
		assert_eq!(picked.global.focus, Some(first.id));
	}

	#[test]
	fn clause_4096_between_dropdowns_the_one_without_transient_for_wins() {
		let mut free = win(1);
		free.maybe_a_dropdown = true;
		free.skip_pager = true;
		let mut tied = win(2);
		tied.maybe_a_dropdown = true;
		tied.skip_pager = true;
		tied.transient_for = Some(7);
		// The newer map would otherwise decide for `tied`; transient-for first.
		assert_ranks_above(&free, &tied);
	}

	#[test]
	fn clause_4102_the_newer_map_wins_only_with_a_game_id() {
		let older = win(1);
		let newer = win(2);
		assert_ranks_above(&newer, &older);

		// Without a game id the map sequence is not consulted.
		let mut a = win(1);
		let mut b = win(2);
		a.app_id = 0;
		b.app_id = 0;
		assert_tie(&a, &b);

		// Sibling menus use it as well.
		for meta in [&mut a, &mut b] {
			meta.maybe_a_dropdown = true;
			meta.skip_pager = true;
		}
		assert_ranks_above(&b, &a);
	}

	#[test]
	fn clause_4107_the_newer_damage_wins_for_game_windows() {
		let mut a = win(1);
		let mut b = win(2);
		a.map_seq = 5;
		b.map_seq = 5;
		a.damage_seq = 9;
		b.damage_seq = 3;
		assert_ranks_above(&a, &b);
		a.app_id = 0;
		b.app_id = 0;
		assert_tie(&a, &b);
	}

	#[test]
	fn clause_4031_the_dropdown_heuristic_and_its_game_exceptions() {
		let mut hint = win(1);
		hint.maybe_a_dropdown = true;
		// Needs a skip hint.
		assert!(!hint.maybe_dropdown());
		hint.skip_taskbar = true;
		assert!(hint.maybe_dropdown());
		// A dialog is not a dropdown ...
		hint.is_dialog = true;
		assert!(!hint.maybe_dropdown());
		// ... unless it has no transient-for and skips both.
		hint.skip_pager = true;
		assert!(hint.maybe_dropdown());
		hint.transient_for = Some(9);
		assert!(!hint.maybe_dropdown());

		// :3986 the Antichamber splash is never a dropdown.
		let mut splash = win(2);
		splash.app_id = 219_890;
		splash.maybe_a_dropdown = true;
		splash.skip_taskbar = true;
		assert!(!splash.maybe_dropdown());

		// :4007 a layered control-parent Wine window is.
		let mut layered = win(3);
		layered.hwnd_style_ex = Some(WS_EX_CONTROLPARENT | WS_EX_LAYERED);
		assert!(layered.maybe_dropdown());
		layered.hwnd_style_ex = Some(WS_EX_CONTROLPARENT | WS_EX_LAYERED | WS_EX_APPWINDOW);
		assert!(!layered.maybe_dropdown());

		// :4016 Forza's black background window.
		let mut forza = win(4);
		forza.app_id = 1_293_830;
		forza.maybe_a_dropdown = true;
		forza.skip_taskbar = true;
		assert!(!forza.maybe_dropdown());
		forza.requested_size = (1280, 720);
		assert!(forza.maybe_dropdown());
	}

	#[test]
	fn clause_4459_overlays_external_overlays_and_invisible_windows_are_not_candidates() {
		let mut overlay = win(2);
		overlay.is_overlay = true;
		let mut external = win(3);
		external.is_external_overlay = true;
		let mut invisible = win(4);
		invisible.opacity = TRANSLUCENT;
		let game = win(1);
		let windows = [&game, &overlay, &external, &invisible];
		let possible = possible_focus_windows(&windows, FocusStrategy::SingleApplication);
		assert_eq!(possible.iter().map(|w| w.id).collect::<Vec<_>>(), vec![1]);
	}

	#[test]
	fn clause_4483_steam_controlled_only_considers_game_and_steam_windows() {
		let mut anonymous = win(1);
		anonymous.app_id = 0;
		let game = win(2);
		let windows = [&anonymous, &game];
		let possible = possible_focus_windows(&windows, FocusStrategy::SteamControlled);
		assert_eq!(possible.iter().map(|w| w.id).collect::<Vec<_>>(), vec![2]);
		let possible = possible_focus_windows(&windows, FocusStrategy::SingleApplication);
		assert_eq!(possible.len(), 2);
	}

	#[test]
	fn clause_4304_the_best_window_is_the_focus_window() {
		let picked = run(&[win(1), win(2)]);
		assert_eq!(picked.ctx.focus, Some(2));
		assert_eq!(picked.global.focus, Some(2));
		assert_eq!(picked.ctx.keyboard_focus, Some(2));
		assert_eq!(picked.global.keyboard_focus, Some(2));
	}

	#[test]
	fn the_steam_ui_that_maps_after_the_game_takes_focus_like_gamescope() {
		let mut game = win(1);
		game.app_id = 440;
		let mut steam = win(2);
		steam.is_steam_legacy_big_picture = true;
		steam.app_id = STEAM_BIG_PICTURE_APP_ID;
		assert_eq!(run(&[game.clone(), steam.clone()]).ctx.focus, Some(2));
		// And the other way round the game wins.
		game.map_seq = 9;
		assert_eq!(run(&[game, steam]).ctx.focus, Some(1));
	}

	#[test]
	fn clause_4361_a_transient_dialog_becomes_the_focus_window() {
		let parent = win(1);
		let mut dialog = win(2);
		dialog.transient_for = Some(1);
		dialog.is_dialog = true;
		let picked = run(&[parent, dialog]);
		assert_eq!(picked.ctx.focus, Some(2), "focus follows the transient link");
		assert_eq!(picked.global.focus, Some(2));
	}

	#[test]
	fn clause_4361_the_role_stays_with_the_window_the_dialog_belongs_to() {
		// The newest window wins the ordering, so map the parent last.
		let mut parent = win(1);
		parent.map_seq = 9;
		let mut dialog = win(2);
		dialog.transient_for = Some(1);
		dialog.is_dialog = true;
		let picked = run(&[dialog, parent]);
		assert_eq!(picked.ctx.role, Some(1), "the fullscreen state belongs to the parent");
		assert_eq!(picked.ctx.focus, Some(2), "but focus moved to the dialog");
	}

	#[test]
	fn clause_4361_a_dropdown_is_not_followed_as_a_transient() {
		let mut parent = win(1);
		parent.map_seq = 9;
		let mut menu = win(2);
		menu.transient_for = Some(1);
		menu.maybe_a_dropdown = true;
		menu.skip_taskbar = true;
		let picked = run(&[parent, menu]);
		assert_eq!(picked.ctx.focus, Some(1));
		assert_eq!(picked.ctx.override_window, Some(2), "the dropdown is the override");
	}

	#[test]
	fn clause_4375_an_override_redirect_window_of_the_same_process_is_the_override() {
		let base = win(1);
		let mut popup = win(2);
		popup.override_redirect = true;
		popup.geometry = Geometry {
			x: 10,
			y: 10,
			w: 100,
			h: 80,
		};
		let picked = run(&[base, popup]);
		assert_eq!(picked.ctx.focus, Some(1));
		assert_eq!(picked.ctx.override_window, Some(2));
		assert_eq!(picked.global.override_window, Some(2));
		// The keyboard goes to the override on the presentation side, but X input
		// focus stays with the base (:4688).
		assert_eq!(picked.global.keyboard_focus, Some(2));
		assert_eq!(picked.ctx.keyboard_focus, Some(1));
	}

	#[test]
	fn clause_4156_an_override_must_share_a_process_and_be_on_screen() {
		let base = win(1);
		let mut stranger = win(2);
		stranger.override_redirect = true;
		stranger.pid = 999;
		assert_eq!(run(&[base.clone(), stranger]).ctx.override_window, None);

		let mut off_screen = win(3);
		off_screen.override_redirect = true;
		off_screen.geometry = Geometry {
			x: -200,
			y: 0,
			w: 100,
			h: 100,
		};
		assert_eq!(run(&[base.clone(), off_screen]).ctx.override_window, None);

		// Sharing the app id is enough across processes.
		let mut same_app = win(4);
		same_app.override_redirect = true;
		same_app.pid = 999;
		same_app.app_id = base.app_id;
		same_app.map_seq = 0;
		assert_eq!(run(&[base, same_app]).ctx.override_window, Some(4));
	}

	#[test]
	fn clause_4119_a_cross_process_transparent_layered_popup_is_a_decoration_not_an_override() {
		let base = win(1);
		let mut highlight = win(2);
		highlight.override_redirect = true;
		highlight.pid = 999;
		highlight.app_id = base.app_id;
		highlight.map_seq = 0;
		highlight.hwnd_style_ex = Some(WS_EX_LAYERED | WS_EX_TRANSPARENT);
		assert_eq!(run(&[base, highlight]).ctx.override_window, None);
	}

	#[test]
	fn clause_4440_a_dropdown_without_override_redirect_is_a_fake_override() {
		let mut base = win(1);
		base.map_seq = 9;
		let mut menu = win(2);
		menu.maybe_a_dropdown = true;
		menu.skip_pager = true;
		let picked = run(&[base, menu]);
		assert_eq!(picked.ctx.override_window, Some(2));
	}

	#[test]
	fn clause_4333_transient_chains_of_overrides_are_followed() {
		let mut base = win(1);
		base.map_seq = 9;
		let mut menu = win(2);
		menu.override_redirect = true;
		let mut submenu = win(3);
		submenu.override_redirect = true;
		submenu.transient_for = Some(2);
		let picked = run(&[base, menu, submenu]);
		assert_eq!(picked.ctx.override_window, Some(3), "the submenu hangs off the menu");
	}

	#[test]
	fn clause_4402_the_global_focus_waits_for_the_first_commit() {
		let old = win(1);
		let mut fresh = win(2);
		fresh.has_commits = false;
		let picked = run_with(
			&[fresh.clone(), old],
			&FocusControl::default(),
			FocusStrategy::SingleApplication,
			PreviousFocus {
				ctx_focus: Some(1),
				global_focus: Some(1),
			},
		);
		assert_eq!(picked.ctx.focus, Some(2), "X focus follows the new window at once");
		assert_eq!(picked.global.focus, Some(1), "the presented focus waits for a buffer");

		// With no previous focus there is nothing to present yet.
		let picked = run(&[fresh]);
		assert_eq!(picked.ctx.focus, Some(2));
		assert_eq!(picked.global.focus, None);
	}

	#[test]
	fn steam_controlled_focus_falls_back_before_steam_writes_focus_control() {
		// The Steam updater: a Steam window (`STEAM_BIGPICTURE`), override
		// redirect, mapped before Steam writes any `GAMESCOPECTRL_*` property.
		let updater = {
			let mut meta = win(1);
			meta.is_steam_legacy_big_picture = true;
			meta.app_id = STEAM_BIG_PICTURE_APP_ID;
			meta.override_redirect = true;
			meta
		};
		let picked = run_with(
			&[updater],
			&FocusControl::default(),
			FocusStrategy::SteamControlled,
			PreviousFocus::default(),
		);
		assert_eq!(picked.global.focus, Some(1));
	}

	#[test]
	fn clause_4257_steam_controlled_focus_selects_by_window_then_app() {
		let a = win(1);
		let b = win(2);
		let c = {
			let mut meta = win(3);
			meta.app_id = 440;
			meta.map_seq = 0;
			meta
		};
		let all = [a, b, c];
		let by_window = FocusControl {
			window: Some(1),
			app_ids: vec![440],
		};
		let picked = run_with(
			&all,
			&by_window,
			FocusStrategy::SteamControlled,
			PreviousFocus::default(),
		);
		assert_eq!(picked.global.focus, Some(1), "the window wins over the app list");

		let by_app = FocusControl {
			window: None,
			app_ids: vec![440],
		};
		let picked = run_with(&all, &by_app, FocusStrategy::SteamControlled, PreviousFocus::default());
		assert_eq!(picked.global.focus, Some(3));

		// An app Steam asked for that has no window leaves nothing focused
		// globally (:4304: `!globalFocus || !controlledFocus` is false).
		let missing = FocusControl {
			window: None,
			app_ids: vec![12345],
		};
		let picked = run_with(&all, &missing, FocusStrategy::SteamControlled, PreviousFocus::default());
		assert_eq!(picked.global.focus, None);
		assert_eq!(
			picked.ctx.focus,
			Some(2),
			"the per-context pick still falls back to the best"
		);
	}

	#[test]
	fn clause_4254_single_application_ignores_the_control_atoms_for_the_base_layer() {
		let control = FocusControl {
			window: Some(1),
			app_ids: vec![1],
		};
		let picked = run_with(
			&[win(1), win(2)],
			&control,
			FocusStrategy::SingleApplication,
			PreviousFocus::default(),
		);
		assert_eq!(picked.global.focus, Some(2), "the best window still wins");
	}

	#[test]
	fn clause_4375_focus_control_app_ids_restrict_the_override_to_the_same_app() {
		let mut base = win(1);
		base.map_seq = 9;
		let mut other_app = win(2);
		other_app.override_redirect = true;
		let control = FocusControl {
			window: None,
			app_ids: vec![1],
		};
		let picked = run_with(
			&[base, other_app],
			&control,
			FocusStrategy::SingleApplication,
			PreviousFocus::default(),
		);
		assert_eq!(picked.ctx.override_window, None);
	}

	#[test]
	fn clause_4527_the_interactive_overlay_spans_the_root_or_wants_input() {
		let game = win(1);
		let mut wide = win(2);
		wide.is_overlay = true;
		wide.geometry.w = 1280;
		let mut narrow = win(3);
		narrow.is_overlay = true;
		narrow.geometry.w = 80;
		let picked = run(&[game.clone(), wide.clone(), narrow.clone()]);
		assert_eq!(picked.ctx.overlay, Some(2));
		assert_eq!(picked.ctx.notification, Some(3));
		assert_eq!(picked.global.overlay, Some(2));
		assert_eq!(picked.global.notification, Some(3));
		// Neither is ever the base layer.
		assert_eq!(picked.global.focus, Some(1));

		narrow.input_focus_mode = 1;
		let picked = run(&[game, wide, narrow]);
		assert_eq!(
			picked.ctx.overlay,
			Some(3),
			"the later input-focus overlay replaces the wide one"
		);
	}

	#[test]
	fn clause_4527_a_more_opaque_interactive_overlay_wins() {
		let mut faint = win(1);
		faint.is_overlay = true;
		faint.geometry.w = 1280;
		faint.opacity = 100;
		let mut solid = win(2);
		solid.is_overlay = true;
		solid.geometry.w = 1280;
		solid.opacity = 200;
		// List order is topmost first: the faint one is visited first.
		let picked = run(&[faint.clone(), solid.clone()]);
		assert_eq!(picked.ctx.overlay, Some(2));
		// The loser is demoted to the notification slot (:4531-4534).
		let picked = run(&[solid, faint]);
		assert_eq!(picked.ctx.overlay, Some(2));
		assert_eq!(picked.ctx.notification, Some(1));
	}

	#[test]
	fn clause_4538_the_most_opaque_external_overlay_is_picked() {
		let game = win(1);
		let mut dim = win(2);
		dim.is_external_overlay = true;
		dim.opacity = 10;
		let mut bright = win(3);
		bright.is_external_overlay = true;
		bright.opacity = 200;
		let picked = run(&[game, dim, bright]);
		assert_eq!(picked.ctx.external_overlay, Some(3));
		assert_eq!(picked.global.external_overlay, Some(3));
	}

	#[test]
	fn clause_5143_an_overlay_that_wants_input_takes_pointer_and_keyboard() {
		let game = win(1);
		let mut overlay = win(2);
		overlay.is_overlay = true;
		overlay.input_focus_mode = 1;
		overlay.geometry.w = 80;
		let picked = run(&[game, overlay]);
		assert_eq!(picked.global.focus, Some(1), "the game keeps the base layer");
		assert_eq!(picked.global.input_focus, Some(2));
		assert_eq!(picked.global.keyboard_focus, Some(2));
		assert_eq!(
			picked.ctx.keyboard_focus,
			Some(2),
			"X input focus follows the overlay too"
		);
		assert_eq!(picked.ctx.raise, Some(2));
	}

	#[test]
	fn clause_4596_input_focus_mode_2_keeps_the_keyboard_on_the_application() {
		let game = win(1);
		let mut overlay = win(2);
		overlay.is_overlay = true;
		overlay.input_focus_mode = 2;
		let picked = run(&[game, overlay]);
		assert_eq!(picked.global.input_focus, Some(2));
		assert_eq!(picked.global.keyboard_focus, Some(1));
		assert_eq!(picked.ctx.keyboard_focus, Some(1));
		assert_eq!(picked.ctx.input_focus, Some(2));
	}

	#[test]
	fn clause_5141_the_override_is_raised_instead_of_a_base_that_is_the_input_focus() {
		let base = win(1);
		let mut popup = win(2);
		popup.override_redirect = true;
		let picked = run(&[base, popup]);
		assert_eq!(picked.ctx.raise, Some(2));
		assert_eq!(picked.global.input_focus, Some(1));
	}

	#[test]
	fn clause_5033_the_focusable_list_skips_useless_skip_and_override_redirect_windows() {
		let game = win(1);
		let mut tiny = win(2);
		tiny.geometry.w = 1;
		tiny.geometry.h = 1;
		let mut skipped = win(3);
		skipped.skip_taskbar = true;
		skipped.skip_pager = true;
		let mut popup = win(4);
		popup.override_redirect = true;
		let steam = {
			let mut meta = win(5);
			meta.app_id = STEAM_BIG_PICTURE_APP_ID;
			meta
		};
		let picked = run(&[game, tiny, skipped, popup, steam]);
		assert_eq!(
			picked.focusable,
			vec![5, 1],
			"best first: the newer Steam UI, then the game"
		);
	}

	#[test]
	fn an_empty_scene_picks_nothing() {
		let picked = run(&[]);
		assert_eq!(picked, Picked::default());
	}

	#[test]
	fn a_focus_window_that_goes_away_is_forgotten() {
		let picked = run_with(
			&[],
			&FocusControl::default(),
			FocusStrategy::SingleApplication,
			PreviousFocus {
				ctx_focus: Some(5),
				global_focus: Some(5),
			},
		);
		assert_eq!(picked.ctx.focus, None);
		assert_eq!(picked.global.focus, None);
		assert_eq!(picked.ctx.keyboard_focus, None);
	}

	#[test]
	fn an_xwayland_window_outranks_a_native_wayland_one_by_game_id() {
		let x = win(1);
		let w = xdg_win(1);
		let picked = pick(
			&[&x],
			&[&w],
			1280,
			&FocusControl::default(),
			FocusStrategy::SingleApplication,
			PreviousFocus::default(),
		);
		assert_eq!(picked.global.focus, Some(1));
	}

	#[test]
	fn a_wayland_toplevel_parented_to_the_focus_window_is_its_override() {
		let parent = xdg_win(1);
		let mut child = xdg_win(2);
		child.transient_for = Some(parent.id);
		let picked = pick(
			&[],
			&[&parent, &child],
			1280,
			&FocusControl::default(),
			FocusStrategy::SingleApplication,
			PreviousFocus::default(),
		);
		assert_eq!(picked.global.focus, Some(parent.id), "the parent keeps the base layer");
		assert_eq!(picked.global.override_window, Some(child.id));
		assert_eq!(
			picked.global.keyboard_focus,
			Some(child.id),
			"and the dialog takes the keyboard"
		);
	}

	#[test]
	fn the_stable_sort_keeps_list_order_for_ties() {
		let mut a = win(1);
		let mut b = win(2);
		let mut c = win(3);
		for meta in [&mut a, &mut b, &mut c] {
			meta.app_id = 0;
		}
		let sorted = sort_by_priority(vec![&b, &a, &c]);
		assert_eq!(sorted.iter().map(|w| w.id).collect::<Vec<_>>(), vec![2, 1, 3]);
	}

	#[test]
	fn capture_affects_position_not_cursor_visibility() {
		// gamescope parity: a lock/confine never hides the cursor;
		// `focus` only gates absolute motion. There is deliberately no
		// `suppresses_cursor` here any more.
		assert!(!PointerCapture::None.is_active());
		assert!(PointerCapture::Locked.is_active());
		assert!(PointerCapture::Confined.is_active());
	}

	#[test]
	fn lock_is_locked_and_is_not_confined() {
		assert!(PointerCapture::Locked.is_locked());
		assert!(!PointerCapture::Locked.is_confined());
		assert!(!PointerCapture::Confined.is_locked());
		assert!(PointerCapture::Confined.is_confined());
	}

	#[test]
	fn clamp_to_region_keeps_inside_points() {
		let region: &[RegionRect] = &[(true, 100, 100, 50, 50)];
		assert_eq!(clamp_to_region((120.0, 120.0), region), (120.0, 120.0));
		assert_eq!(clamp_to_region((100.0, 149.0), region), (100.0, 149.0));
	}

	#[test]
	fn clamp_to_region_projects_outside_points_to_the_nearest_edge() {
		let region: &[RegionRect] = &[(true, 100, 100, 50, 50)];
		assert_eq!(clamp_to_region((0.0, 0.0), region), (100.0, 100.0));
		// The exclusive edge clamps to the last coordinate inside the region.
		assert_eq!(clamp_to_region((400.0, 400.0), region), (149.0, 149.0));
		assert_eq!(clamp_to_region((120.0, 0.0), region), (120.0, 100.0));
		assert_eq!(clamp_to_region((400.0, 120.0), region), (149.0, 120.0));
	}

	#[test]
	fn clamp_to_region_without_rectangles_is_the_identity() {
		assert_eq!(clamp_to_region((5.0, 6.0), &[]), (5.0, 6.0));
	}

	#[test]
	fn clip_region_to_bounds_intersects_an_oversize_region() {
		// A region larger than the surface is clipped to the surface, so a
		// clamp can never land off it (which would drop the capture).
		let region: &[RegionRect] = &[(true, 0, 0, 1000, 1000)];
		assert_eq!(clip_region_to_bounds(region, 160, 120), vec![(true, 0, 0, 160, 120)]);
		// A region partly outside keeps only the inside part.
		let region: &[RegionRect] = &[(true, -20, -10, 100, 50)];
		assert_eq!(clip_region_to_bounds(region, 160, 120), vec![(true, 0, 0, 80, 40)]);
	}

	#[test]
	fn clip_region_to_bounds_drops_empty_rectangles() {
		let region: &[RegionRect] = &[(true, 200, 200, 50, 50), (false, 0, 0, 10, 10)];
		assert_eq!(clip_region_to_bounds(region, 160, 120), vec![(false, 0, 0, 10, 10)]);
		assert!(clip_region_to_bounds(region, 0, 0).is_empty());
	}

	#[test]
	fn surface_region_is_the_whole_surface() {
		assert_eq!(surface_region(160, 120), vec![(true, 0, 0, 160, 120)]);
		assert!(surface_region(0, 120).is_empty());
	}

	#[test]
	fn a_regionless_confine_clamps_to_the_surface_edge() {
		// Xwayland sends a NULL region for an X confine-to grab; the effective
		// region is the whole surface. An output point one pixel outside the
		// surface edge projects to the last coordinate inside it, never off it.
		let rects = surface_region(160, 120);
		assert_eq!(clamp_to_region((-5.0, 60.0), &rects), (0.0, 60.0));
		assert_eq!(clamp_to_region((200.0, 60.0), &rects), (159.0, 60.0));
		assert_eq!(clamp_to_region((80.0, -5.0), &rects), (80.0, 0.0));
		assert_eq!(clamp_to_region((80.0, 200.0), &rects), (80.0, 119.0));
	}

	#[test]
	fn a_fresh_focus_serial_is_clean() {
		let dirty = FocusDirty::new();
		assert!(!dirty.is_dirty(), "no event has marked focus dirty yet");
	}

	#[test]
	fn marking_focus_dirty_makes_a_pass_due_until_it_is_applied() {
		// Mirrors MakeFocusDirty()/IsDirty(): the serial advances, the pass is
		// due, and recording the serial it observed clears it.
		let mut dirty = FocusDirty::new();
		dirty.mark();
		assert!(dirty.is_dirty(), "a mark must make the pass due");
		let serial = dirty.serial();
		dirty.applied(serial);
		assert!(!dirty.is_dirty(), "recording the observed serial must clear the pass");
	}

	#[test]
	fn a_mark_during_a_pass_keeps_it_dirty() {
		// A change made while the pass runs must not be swallowed: the pass
		// records the serial it read before running, so a later mark leaves the
		// next pass due.
		let mut dirty = FocusDirty::new();
		dirty.mark();
		let observed = dirty.serial();
		dirty.mark();
		dirty.applied(observed);
		assert!(dirty.is_dirty(), "a mark after the pass read the serial must re-arm it");
	}

	#[test]
	fn focus_geometry_keeps_a_window_without_a_game_id() {
		// win_has_game_id is appID != 0: a plain window is left to its client.
		assert_eq!(focus_geometry(false, true, true, true), FocusGeometry::Keep);
	}

	#[test]
	fn focus_geometry_fills_the_output_for_a_fullscreen_game() {
		assert_eq!(focus_geometry(true, true, false, false), FocusGeometry::FillOutput);
		// force_windows_fullscreen widens the branch even without isFullscreen.
		assert_eq!(focus_geometry(true, false, true, true), FocusGeometry::FillOutput);
	}

	#[test]
	fn focus_geometry_restores_the_requested_size_when_windowed() {
		// A game-id window that is not fullscreen and has a requested size is
		// resized back to it (gamescope's :4839 branch).
		assert_eq!(focus_geometry(true, false, false, true), FocusGeometry::Requested);
		// With no requested size there is nothing to restore.
		assert_eq!(focus_geometry(true, false, false, false), FocusGeometry::Keep);
	}
}
