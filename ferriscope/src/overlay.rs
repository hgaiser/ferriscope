//! The second X11 connection: Steam/gamescope atoms in, the focus contract out.
//!
//! There is **no overlay protocol**. Steam's overlay and notifications reach the
//! compositor as ordinary X11 client surfaces (Xwayland windows) tagged with
//! atoms, and the compositor only *composites* their buffers; it never draws
//! overlay pixels. Which window is the overlay, the notification or the base
//! layer is decided by the pure focus selection in [`crate::focus`], from the
//! per-window metadata cache the shell fills with the reads in this module.
//!
//! The atoms, verified against the gamescope sources (`steamcompmgr.cpp`):
//!
//! | Atom | Meaning |
//! |---|---|
//! | `STEAM_OVERLAY` | an overlay or a notification |
//! | `STEAM_INPUT_FOCUS` | non-zero: the overlay wants input (interactive) |
//! | `STEAM_BIGPICTURE` | the Steam UI; appID 769 |
//! | `STEAM_GAME` | the game; its value is the appID |
//! | `GAMESCOPE_EXTERNAL_OVERLAY` | MangoApp/layer-shell |
//! | `_WINE_HWND_STYLE` / `_WINE_HWND_EXSTYLE` | Wine's window styles |
//! | `GAMESCOPECTRL_BASELAYER_WINDOW` / `_APPID` | Steam's choice of base layer (root window) |
//!
//! An in-game Shift+Tab overlay is invisible here: `gameoverlayrenderer.so` is
//! injected into the game process and renders into the game's own swapchain, so
//! the compositor only ever sees the game's finished frame.
//!
//! Reading the atoms needs an X11 connection, and the pinned Smithay exposes no
//! generic property access on `X11Surface`/`X11Wm` (only typed accessors, plus
//! `WmWindowProperty::Other(Atom)` notifications that name an atom without its
//! value). [`SteamAtoms`] therefore opens its own connection to the same
//! Xwayland display and interns the atoms by name.
//!
//! **Reads happen only at map time and on a `PropertyNotify`** (one pipelined
//! batch, [`SteamAtoms::read_window`]); the focus pass and the pointer hit-test
//! read the cache, never this connection. When `--steam` is on the same
//! connection also publishes the focused-window contract on the root, and it
//! watches the root for Steam's focus control and `FocusOut` escapes.

use std::os::fd::{AsFd, OwnedFd};

use smithay::reexports::x11rb::{
	self,
	connection::Connection,
	cookie::Cookie,
	protocol::{
		Event as XEvent,
		xproto::{
			Atom, AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConnectionExt, EventMask, GetPropertyReply,
			InputFocus, PropMode, Window as X11Window,
		},
	},
	rust_connection::RustConnection,
	wrapper::ConnectionExt as _,
};

use crate::{focus::FocusControl, steam::FocusedApp, touch::TouchClickMode};

/// Pack a display name the way gamescope writes `GAMESCOPE_FOCUS_DISPLAY`:
/// the string bytes are reinterpreted as little-endian 32-bit `CARDINAL`s, and
/// the property holds `len + 1` elements (gamescope passes
/// `strlen(name) + 1` as the element count with format 32,
/// `steamcompmgr.cpp:5388-5395`). gamescope over-reads past the name's NUL
/// because of that count; the over-read is zero-filled here instead of copied
/// from uninitialised memory, so the observable type, format, element count
/// and first element match. `":3"` packs to `[0x333A, 0, 0]`, which `xprop`
/// renders as `13114, 0, 0`.
pub fn pack_display_name(name: &str) -> Vec<u32> {
	let bytes = name.as_bytes();
	let mut words = vec![0u32; bytes.len() + 1];
	for (i, byte) in bytes.iter().enumerate() {
		words[i / 4] |= u32::from(*byte) << (8 * (i % 4));
	}
	words
}

/// The raw property state of one X11 window that the focus selection caches.
///
/// Each field is what gamescope reads with `XGetWindowProperty` at map time
/// (`map_win`, `steamcompmgr.cpp:5695-5750`) or on a `PropertyNotify`
/// (`handle_property_notify`, `:6790-7640`): a boolean atom is `true` when
/// present with a non-zero value, `input_focus` and `game` carry their integer
/// value (zero when absent).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WindowProps {
	/// `STEAM_OVERLAY`: an overlay or notification.
	pub overlay: bool,
	/// `STEAM_INPUT_FOCUS`: non-zero when the overlay wants input.
	pub input_focus: u32,
	/// `STEAM_BIGPICTURE`: the Steam UI (`isSteamLegacyBigPicture`).
	pub big_picture: bool,
	/// `STEAM_GAME`: the game's appID (`steamAppID`).
	pub game: u32,
	/// `GAMESCOPE_EXTERNAL_OVERLAY`: MangoApp.
	pub external_overlay: bool,
	/// `_NET_WM_STATE_FULLSCREEN` in `_NET_WM_STATE` (`get_net_wm_state`, `:5603`).
	pub fullscreen: bool,
	/// `_NET_WM_STATE_SKIP_TASKBAR`.
	pub skip_taskbar: bool,
	/// `_NET_WM_STATE_SKIP_PAGER`.
	pub skip_pager: bool,
	/// What `_NET_WM_WINDOW_TYPE` says about dialogs (`get_win_type`, `:5473`):
	/// `Some(true)` for a `DIALOG` entry, `Some(false)` for a later `NORMAL`
	/// one, `None` when neither is listed (a transient-for then decides).
	pub dialog: Option<bool>,
	/// `_WINE_HWND_STYLE` when the property exists.
	pub hwnd_style: Option<u32>,
	/// `_WINE_HWND_EXSTYLE` when the property exists.
	pub hwnd_style_ex: Option<u32>,
	/// `_NET_WM_WINDOW_OPACITY`, `None` when absent (`:5693`).
	pub opacity: Option<u32>,
	/// `WM_TRANSIENT_FOR` (`XGetTransientForHint`, `:5760`).
	pub transient_for: Option<u32>,
	/// `_NET_WM_PID`, 0 when absent.
	pub pid: u32,
	/// The `WM_NORMAL_HINTS` facts gamescope derives (`get_size_hints`, `:5496`).
	pub size_hints: SizeHints,
}

/// What gamescope reads out of `WM_NORMAL_HINTS` (`get_size_hints`,
/// `steamcompmgr.cpp:5496-5525`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SizeHints {
	/// `PPosition | PWinGravity` with a non-negative position and
	/// `StaticGravity`: `maybe_a_dropdown`.
	pub static_position: bool,
	/// `PMinSize | PMaxSize` with equal, non-zero min and max: the size the
	/// window insists on (`requestedWidth` / `requestedHeight`).
	pub fixed_size: Option<(u32, u32)>,
}

impl SizeHints {
	/// Decode the raw `WM_NORMAL_HINTS` words (an `XSizeHints`).
	fn parse(words: &[u32]) -> Self {
		// XSizeHints: flags, x, y, width, height, min_width, min_height,
		// max_width, max_height, width_inc, height_inc, min_aspect x/y,
		// max_aspect x/y, base_width, base_height, win_gravity.
		const P_POSITION: u32 = 1 << 2;
		const P_MIN_SIZE: u32 = 1 << 4;
		const P_MAX_SIZE: u32 = 1 << 5;
		const P_WIN_GRAVITY: u32 = 1 << 9;
		const STATIC_GRAVITY: u32 = 10;
		let word = |index: usize| words.get(index).copied().unwrap_or(0);
		let flags = word(0);

		// :5498
		let static_position = flags & (P_POSITION | P_WIN_GRAVITY) == (P_POSITION | P_WIN_GRAVITY)
			&& (word(1) as i32) >= 0
			&& (word(2) as i32) >= 0
			&& word(17) == STATIC_GRAVITY;

		// :5511
		let (min_w, min_h, max_w, max_h) = (word(5), word(6), word(7), word(8));
		let fixed_size = (flags & (P_MIN_SIZE | P_MAX_SIZE) != 0
			&& max_w != 0
			&& max_h != 0
			&& min_w != 0
			&& min_h != 0
			&& max_w == min_w
			&& min_h == max_h)
			.then_some((max_w, max_h));
		Self {
			static_position,
			fixed_size,
		}
	}
}

/// A watched window property, for routing a `PropertyNotify` to the right
/// cache update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchedAtom {
	/// `STEAM_OVERLAY`.
	Overlay,
	/// `STEAM_INPUT_FOCUS`.
	InputFocus,
	/// `STEAM_BIGPICTURE`.
	BigPicture,
	/// `STEAM_GAME`.
	Game,
	/// `GAMESCOPE_EXTERNAL_OVERLAY`.
	ExternalOverlay,
	/// `_NET_WM_STATE`.
	WmState,
	/// `_WINE_HWND_STYLE`.
	HwndStyle,
	/// `_WINE_HWND_EXSTYLE`.
	HwndStyleEx,
}

/// What one drain of the connection's events saw.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct XEvents {
	/// A `FocusOut` on a window the compositor focused.
	pub focus_out: bool,
	/// Steam changed `GAMESCOPECTRL_BASELAYER_APPID` / `_WINDOW` on the root.
	pub control_changed: bool,
	/// Steam changed `GAMESCOPE_FPS_LIMIT` on the root.
	pub fps_limit_changed: bool,
	/// Steam changed `GAMESCOPE_KEYBOARD_LAYOUT` on the root.
	pub keyboard_layout_changed: bool,
	/// Steam changed `STEAM_TOUCH_CLICK_MODE` on the root.
	pub touch_click_mode_changed: bool,
	/// Steam changed `GAMESCOPE_SDR_ON_HDR_CONTENT_BRIGHTNESS` on the root.
	pub sdr_brightness_changed: bool,
}

/// The ICCCM `WM_STATE` values the compositor writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IcccmState {
	/// `NormalState` (`ICCCM_NORMAL_STATE`).
	Normal = 1,
	/// `IconicState` (`ICCCM_ICONIC_STATE`).
	Iconic = 3,
}

/// The Steam atom names, interned once, over their own Xwayland connection.
///
/// `window` in the queries is an X11 window id, which is valid on any
/// connection to the same server; the connection is kept for the pipelined
/// property reads and the root-window contract.
pub struct SteamAtoms {
	conn: RustConnection,
	root: X11Window,
	/// `:N` of the Xwayland display this connection was opened on; published as
	/// `GAMESCOPE_FOCUS_DISPLAY`/`_MOUSE_`/`_KEYBOARD_`.
	display_name: String,
	overlay: Atom,
	input_focus: Atom,
	big_picture: Atom,
	game: Atom,
	external_overlay: Atom,
	opacity: Atom,
	net_wm_pid: Atom,
	net_wm_state: Atom,
	/// `WM_STATE` (ICCCM 4.1.3.1), written by the compositor.
	wm_state: Atom,
	wm_protocols: Atom,
	wm_take_focus: Atom,
	net_wm_state_fullscreen: Atom,
	net_wm_state_skip_taskbar: Atom,
	net_wm_state_skip_pager: Atom,
	net_wm_window_type: Atom,
	net_wm_window_type_dialog: Atom,
	net_wm_window_type_normal: Atom,
	wine_hwnd_style: Atom,
	wine_hwnd_exstyle: Atom,
	control_app_id: Atom,
	control_window: Atom,
	/// `STEAM_TOUCH_CLICK_MODE`: Steam's choice of touch click mode (root).
	touch_click_mode: Atom,
	focused_app: Atom,
	focused_app_gfx: Atom,
	focused_window: Atom,
	focus_display: Atom,
	mouse_focus_display: Atom,
	keyboard_focus_display: Atom,
	focusable_apps: Atom,
	focusable_windows: Atom,
	/// `GAMESCOPE_XWAYLAND_SERVER_ID`: server discovery for a gamescope WSI
	/// layer.
	xwayland_server_id: Atom,
	/// `FERRISCOPE_XWAYLAND_SERVER_ID`: server discovery for `ferriscope-wsi`,
	/// checked against `ferriscope_swapchain_factory_v1.xwayland_server`.
	ferriscope_server_id: Atom,
	/// `GAMESCOPE_PID`: the compositor's pid, which Steam's gamescope composer
	/// reads to recognise the compositor (gamescope writes it unconditionally,
	/// `steamcompmgr.cpp:9021-9022`).
	compositor_pid: Atom,
	/// `GAMESCOPE_FPS_LIMIT`: Steam's per-game frame-rate cap.
	fps_limit: Atom,
	/// `GAMESCOPE_KEYBOARD_LAYOUT`: the layout Steam pushes for physical keyboards.
	keyboard_layout: Atom,
	/// `GAMESCOPE_HDR_OUTPUT_FEEDBACK`: whether the output can show HDR; the WSI
	/// layer exposes HDR swapchain formats only when it is set.
	hdr_output_feedback: Atom,
	/// `GAMESCOPE_COLOR_APP_WANTS_HDR_FEEDBACK`: whether the base layer is HDR.
	app_wants_hdr_feedback: Atom,
	/// `GAMESCOPE_SDR_ON_HDR_CONTENT_BRIGHTNESS`: Steam's SDR white level in
	/// nits, an `f32` stored as its bits.
	sdr_on_hdr_brightness: Atom,
}

impl SteamAtoms {
	/// Connect to Xwayland display `:display` and intern the Steam atoms, then
	/// publish the two unconditional root properties gamescope writes at X11
	/// init and start watching the root for Steam's focus control.
	///
	/// `GAMESCOPE_XWAYLAND_SERVER_ID` is published so a WSI client can discover
	/// this Xwayland server and complete the `override_window_content`
	/// handshake (`steam::display_id_to_server_id`). `GAMESCOPE_PID` is
	/// published as the compositor's pid, which Steam's gamescope composer reads
	/// to recognise the compositor. Both are unconditional, like the atom
	/// classification: gamescope writes them regardless of `steamMode`
	/// (`steamcompmgr.cpp:9021-9022`), so a plain `ferriscope` identifies as
	/// gamescope exactly as a plain `gamescope` does. The `--steam`-gated
	/// focused-window contract is written later by `shell`.
	pub fn connect(display: u32) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
		let (conn, screen) = x11rb::connect(Some(&format!(":{display}")))?;
		let root = conn.setup().roots[screen].root;
		let atoms = Self {
			root,
			display_name: format!(":{display}"),
			overlay: intern(&conn, b"STEAM_OVERLAY")?,
			input_focus: intern(&conn, b"STEAM_INPUT_FOCUS")?,
			big_picture: intern(&conn, b"STEAM_BIGPICTURE")?,
			game: intern(&conn, b"STEAM_GAME")?,
			external_overlay: intern(&conn, b"GAMESCOPE_EXTERNAL_OVERLAY")?,
			opacity: intern(&conn, b"_NET_WM_WINDOW_OPACITY")?,
			net_wm_pid: intern(&conn, b"_NET_WM_PID")?,
			net_wm_state: intern(&conn, b"_NET_WM_STATE")?,
			wm_state: intern(&conn, b"WM_STATE")?,
			wm_protocols: intern(&conn, b"WM_PROTOCOLS")?,
			wm_take_focus: intern(&conn, b"WM_TAKE_FOCUS")?,
			net_wm_state_fullscreen: intern(&conn, b"_NET_WM_STATE_FULLSCREEN")?,
			net_wm_state_skip_taskbar: intern(&conn, b"_NET_WM_STATE_SKIP_TASKBAR")?,
			net_wm_state_skip_pager: intern(&conn, b"_NET_WM_STATE_SKIP_PAGER")?,
			net_wm_window_type: intern(&conn, b"_NET_WM_WINDOW_TYPE")?,
			net_wm_window_type_dialog: intern(&conn, b"_NET_WM_WINDOW_TYPE_DIALOG")?,
			net_wm_window_type_normal: intern(&conn, b"_NET_WM_WINDOW_TYPE_NORMAL")?,
			wine_hwnd_style: intern(&conn, b"_WINE_HWND_STYLE")?,
			wine_hwnd_exstyle: intern(&conn, b"_WINE_HWND_EXSTYLE")?,
			control_app_id: intern(&conn, b"GAMESCOPECTRL_BASELAYER_APPID")?,
			control_window: intern(&conn, b"GAMESCOPECTRL_BASELAYER_WINDOW")?,
			touch_click_mode: intern(&conn, b"STEAM_TOUCH_CLICK_MODE")?,
			focused_app: intern(&conn, b"GAMESCOPE_FOCUSED_APP")?,
			focused_app_gfx: intern(&conn, b"GAMESCOPE_FOCUSED_APP_GFX")?,
			focused_window: intern(&conn, b"GAMESCOPE_FOCUSED_WINDOW")?,
			focus_display: intern(&conn, b"GAMESCOPE_FOCUS_DISPLAY")?,
			mouse_focus_display: intern(&conn, b"GAMESCOPE_MOUSE_FOCUS_DISPLAY")?,
			keyboard_focus_display: intern(&conn, b"GAMESCOPE_KEYBOARD_FOCUS_DISPLAY")?,
			focusable_apps: intern(&conn, b"GAMESCOPE_FOCUSABLE_APPS")?,
			focusable_windows: intern(&conn, b"GAMESCOPE_FOCUSABLE_WINDOWS")?,
			xwayland_server_id: intern(&conn, b"GAMESCOPE_XWAYLAND_SERVER_ID")?,
			ferriscope_server_id: intern(&conn, b"FERRISCOPE_XWAYLAND_SERVER_ID")?,
			compositor_pid: intern(&conn, b"GAMESCOPE_PID")?,
			fps_limit: intern(&conn, b"GAMESCOPE_FPS_LIMIT")?,
			keyboard_layout: intern(&conn, b"GAMESCOPE_KEYBOARD_LAYOUT")?,
			hdr_output_feedback: intern(&conn, b"GAMESCOPE_HDR_OUTPUT_FEEDBACK")?,
			app_wants_hdr_feedback: intern(&conn, b"GAMESCOPE_COLOR_APP_WANTS_HDR_FEEDBACK")?,
			sdr_on_hdr_brightness: intern(&conn, b"GAMESCOPE_SDR_ON_HDR_CONTENT_BRIGHTNESS")?,
			conn,
		};
		atoms.set_xwayland_server_id(crate::steam::display_id_to_server_id(display));
		atoms.set_compositor_pid(std::process::id());
		if let Err(e) = identify_as_steamcompmgr(&atoms.conn, root) {
			tracing::warn!("cannot name the window manager steamcompmgr: {e}");
		}
		// gamescope watches the root for Steam's focus control
		// (`handle_property_notify`, `steamcompmgr.cpp:6881-6886`).
		let _ = atoms.conn.change_window_attributes(
			root,
			&ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
		);
		let _ = atoms.conn.flush();
		Ok(atoms)
	}

	/// Which watched window property `atom` is, if any.
	pub fn watched(&self, atom: Atom) -> Option<WatchedAtom> {
		match atom {
			a if a == self.overlay => Some(WatchedAtom::Overlay),
			a if a == self.input_focus => Some(WatchedAtom::InputFocus),
			a if a == self.big_picture => Some(WatchedAtom::BigPicture),
			a if a == self.game => Some(WatchedAtom::Game),
			a if a == self.external_overlay => Some(WatchedAtom::ExternalOverlay),
			a if a == self.net_wm_state => Some(WatchedAtom::WmState),
			a if a == self.wine_hwnd_style => Some(WatchedAtom::HwndStyle),
			a if a == self.wine_hwnd_exstyle => Some(WatchedAtom::HwndStyleEx),
			_ => None,
		}
	}

	/// Read every watched property of a window in one pipelined batch.
	///
	/// All requests are sent before the first reply is read, so this costs one
	/// round trip however many atoms there are. A property that is absent, or
	/// that the server reports with a non-cardinal format, reads as zero/false;
	/// a read error degrades to the same, so a wedged or disconnected X server
	/// cannot make the cache panic. Called at map time and on `PropertyNotify`
	/// only.
	pub fn read_window(&self, window: X11Window) -> WindowProps {
		let overlay = self.request(window, self.overlay, 1);
		let input_focus = self.request(window, self.input_focus, 1);
		let big_picture = self.request(window, self.big_picture, 1);
		let game = self.request(window, self.game, 1);
		let external_overlay = self.request(window, self.external_overlay, 1);
		let wm_state = self.request(window, self.net_wm_state, 2048);
		let window_type = self.request(window, self.net_wm_window_type, 2048);
		let hwnd_style = self.request(window, self.wine_hwnd_style, 1);
		let hwnd_style_ex = self.request(window, self.wine_hwnd_exstyle, 1);
		let opacity = self.request(window, self.opacity, 1);
		let transient_for = self.request(window, AtomEnum::WM_TRANSIENT_FOR.into(), 1);
		let pid = self.request(window, self.net_wm_pid, 1);
		let normal_hints = self.request(window, AtomEnum::WM_NORMAL_HINTS.into(), 18);

		let mut props = WindowProps {
			overlay: first_u32(finish(overlay)).unwrap_or(0) != 0,
			input_focus: first_u32(finish(input_focus)).unwrap_or(0),
			big_picture: first_u32(finish(big_picture)).unwrap_or(0) != 0,
			game: first_u32(finish(game)).unwrap_or(0),
			external_overlay: first_u32(finish(external_overlay)).unwrap_or(0) != 0,
			hwnd_style: first_u32(finish(hwnd_style)),
			hwnd_style_ex: first_u32(finish(hwnd_style_ex)),
			opacity: first_u32(finish(opacity)),
			transient_for: first_u32(finish(transient_for)).filter(|id| *id != 0),
			pid: first_u32(finish(pid)).unwrap_or(0),
			..WindowProps::default()
		};
		if let Some(reply) = finish(normal_hints) {
			props.size_hints = SizeHints::parse(&all_u32(&reply));
		}
		if let Some(reply) = finish(wm_state) {
			for atom in all_u32(&reply) {
				if atom == self.net_wm_state_fullscreen {
					props.fullscreen = true;
				} else if atom == self.net_wm_state_skip_taskbar {
					props.skip_taskbar = true;
				} else if atom == self.net_wm_state_skip_pager {
					props.skip_pager = true;
				}
			}
		}
		if let Some(reply) = finish(window_type) {
			for atom in all_u32(&reply) {
				if atom == self.net_wm_window_type_dialog {
					props.dialog = Some(true);
				}
				if atom == self.net_wm_window_type_normal {
					props.dialog = Some(false);
				}
			}
		}
		props
	}

	/// Read Steam's focus control from the root window
	/// (`GAMESCOPECTRL_BASELAYER_WINDOW` and `_APPID`, `steamcompmgr.cpp:6881`).
	pub fn read_focus_control(&self) -> FocusControl {
		let window = first_u32(finish(self.request(self.root, self.control_window, 1))).filter(|id| *id != 0);
		let app_ids = finish(self.request(self.root, self.control_app_id, 1024))
			.map(|reply| all_u32(&reply))
			.unwrap_or_default();
		FocusControl { window, app_ids }
	}

	/// `GAMESCOPE_FPS_LIMIT` on the root: the frame-rate cap in Hz, 0 when
	/// unset (`get_prop( ..., gamescopeFPSLimit, 0 )`, `steamcompmgr.cpp:7163`).
	pub fn read_fps_limit(&self) -> u32 {
		first_u32(finish(self.request(self.root, self.fps_limit, 1))).unwrap_or(0)
	}

	/// `GAMESCOPE_KEYBOARD_LAYOUT` on the root: `layout[:variant]`, empty when
	/// unset (`get_string_prop`, `steamcompmgr.cpp:7366`).
	pub fn read_keyboard_layout(&self) -> String {
		finish(self.request(self.root, self.keyboard_layout, 256))
			.map(|reply| String::from_utf8_lossy(&reply.value).trim_end_matches('\0').to_string())
			.unwrap_or_default()
	}

	/// `GAMESCOPE_SDR_ON_HDR_CONTENT_BRIGHTNESS` on the root: the SDR white
	/// level in nits (`steamcompmgr.cpp:7335`).
	pub fn read_sdr_on_hdr_brightness(&self) -> f32 {
		crate::hdr::sdr_white_nits(first_u32(finish(self.request(
			self.root,
			self.sdr_on_hdr_brightness,
			1,
		))))
	}

	/// Publish `GAMESCOPE_HDR_OUTPUT_FEEDBACK`: the output can show HDR
	/// (`steamcompmgr.cpp:10240`). The WSI layer reads it when it creates a
	/// surface, so it is written before any client runs.
	pub fn set_hdr_output_feedback(&self, capable: bool) {
		let _ = self.conn.change_property32(
			PropMode::REPLACE,
			self.root,
			self.hdr_output_feedback,
			AtomEnum::CARDINAL,
			&[u32::from(capable)],
		);
		let _ = self.conn.flush();
	}

	/// Publish `GAMESCOPE_COLOR_APP_WANTS_HDR_FEEDBACK`: the base layer is HDR
	/// (`steamcompmgr.cpp:10402`).
	pub fn set_app_wants_hdr(&self, wants: bool) {
		let _ = self.conn.change_property32(
			PropMode::REPLACE,
			self.root,
			self.app_wants_hdr_feedback,
			AtomEnum::CARDINAL,
			&[u32::from(wants)],
		);
		let _ = self.conn.flush();
	}

	/// Read Steam's touch click mode from the root window
	/// (`STEAM_TOUCH_CLICK_MODE`, `steamcompmgr.cpp:6844`).
	///
	/// gamescope reads it with a default of `0`, so an absent property is
	/// [`TouchClickMode::Hover`].
	pub fn read_touch_click_mode(&self) -> TouchClickMode {
		let raw = first_u32(finish(self.request(self.root, self.touch_click_mode, 1))).unwrap_or(0);
		TouchClickMode::from_raw(raw)
	}

	/// Whether `atom` is one of Steam's focus-control root properties.
	fn is_control_atom(&self, atom: Atom) -> bool {
		atom == self.control_app_id || atom == self.control_window
	}

	/// Publish the focused-window contract on the root window.
	///
	/// Writes `GAMESCOPE_FOCUSED_APP` (input-focus owner) and
	/// `GAMESCOPE_FOCUSED_APP_GFX` (role-holding window) as single 32-bit
	/// `CARDINAL`s, deleting a property whose app id is zero, exactly as
	/// gamescope does when `steamMode` is set (`steamcompmgr.cpp:5376-5383`).
	/// `GAMESCOPE_FOCUSED_WINDOW` carries the role-holding (base) window's X11
	/// id, gamescope's `focusWindow` (`:5385`), and is deleted when zero: it
	/// stays the base window while an interactive overlay holds input focus. The display name is written as an `XA_CARDINAL`
	/// to `GAMESCOPE_FOCUS_DISPLAY`, `GAMESCOPE_MOUSE_FOCUS_DISPLAY` and
	/// `GAMESCOPE_KEYBOARD_FOCUS_DISPLAY`, matching gamescope's raw
	/// `XChangeProperty(..., XA_CARDINAL, 32, PropModeReplace, (unsigned
	/// char *)name, strlen(name) + 1)` (`steamcompmgr.cpp:5388-5395`); the mode
	/// gate lives in `shell`. This is the raw write over the connection already
	/// open for the property reads.
	pub fn set_focused_app(&self, focused: FocusedApp, window_id: u32) {
		self.set_root_cardinal(self.focused_app, focused.app_id);
		self.set_root_cardinal(self.focused_app_gfx, focused.gfx_app_id);
		self.set_root_cardinal(self.focused_window, window_id);
		self.write_focus_display(self.focus_display, &self.display_name);
		self.write_focus_display(self.mouse_focus_display, &self.display_name);
		self.write_focus_display(self.keyboard_focus_display, &self.display_name);
		let _ = self.conn.flush();
	}

	/// Publish the focusable-app and focusable-window lists on the root window.
	///
	/// `apps` is the `GAMESCOPE_FOCUSABLE_APPS` array and `windows` the
	/// flattened `[window, app, pid]` triplets of `GAMESCOPE_FOCUSABLE_WINDOWS`,
	/// both computed by [`crate::steam::focusable_apps`] /
	/// [`crate::steam::focusable_windows`]. An empty array replaces the property
	/// with a zero-length value (clears it), matching gamescope's
	/// `XChangeProperty` with `nelements = 0`.
	pub fn set_focusable(&self, apps: &[u32], windows: &[u32]) {
		self.replace_cardinal_array(self.focusable_apps, apps);
		self.replace_cardinal_array(self.focusable_windows, windows);
		let _ = self.conn.flush();
	}

	/// Publish `GAMESCOPE_XWAYLAND_SERVER_ID` and `FERRISCOPE_XWAYLAND_SERVER_ID`
	/// on the root window.
	fn set_xwayland_server_id(&self, server_id: u32) {
		self.set_root_cardinal(self.xwayland_server_id, server_id);
		self.set_root_cardinal(self.ferriscope_server_id, server_id);
		let _ = self.conn.flush();
	}

	/// Force the X input focus to `window`, the way gamescope does.
	///
	/// gamescope calls `XSetInputFocus` unconditionally for its focus window
	/// (`steamcompmgr.cpp:4693`), ignoring the client's ICCCM input model.
	/// Smithay's `X11Surface::enter` only sends `WM_TAKE_FOCUS` for a
	/// `GloballyActive` client (`WM_HINTS.input = False`, e.g. a Proton/Wine
	/// game) and never moves the X input focus, so ferriscope's seat keyboard
	/// target and the X input focus diverge: keyboard input keeps reaching the
	/// previous X focus window (the Steam UI) while the focused-app contract
	/// already names the game. Any X client may set the input focus; this reuses
	/// the connection already open for the property reads. `RevertToNone` matches
	/// gamescope's choice when the target is the focus window itself.
	pub fn set_input_focus(&self, window: X11Window) {
		let _ = self.conn.set_input_focus(InputFocus::NONE, window, x11rb::CURRENT_TIME);
		// ICCCM: a window that lists `WM_TAKE_FOCUS` is also told it has the
		// focus. Wine needs it: under `steamcompmgr` it drops a game to the
		// background when it loses the X focus (the Steam overlay) and makes it
		// the foreground window again only on `WM_TAKE_FOCUS`, not on
		// `FocusIn`. A focus corrected here without it left the game in the
		// background for good: Stick Fight drew black after the overlay.
		if self.takes_focus(window) {
			let event = ClientMessageEvent::new(
				32,
				window,
				self.wm_protocols,
				[self.wm_take_focus, x11rb::CURRENT_TIME, 0, 0, 0],
			);
			let _ = self.conn.send_event(false, window, EventMask::NO_EVENT, event);
		}
		// Watch the target for `FocusOut`: an X client (notably Steam's CEF) can
		// move the X input focus to one of its own subwindows after we set it,
		// and gamescope corrects that from its `FocusOut` handler
		// (`steamcompmgr.cpp:8602-8638`). Selecting the event here lets the
		// compositor do the same (see `FerriscopeState::poll_x11_events`).
		let _ = self.conn.change_window_attributes(
			window,
			&ChangeWindowAttributesAux::new().event_mask(EventMask::FOCUS_CHANGE),
		);
		let _ = self.conn.flush();
	}

	/// Whether `window`'s `WM_PROTOCOLS` lists `WM_TAKE_FOCUS`.
	fn takes_focus(&self, window: X11Window) -> bool {
		self.conn
			.get_property(false, window, self.wm_protocols, AtomEnum::ATOM, 0, 32)
			.ok()
			.and_then(|cookie| cookie.reply().ok())
			.is_some_and(|reply| {
				reply
					.value32()
					.is_some_and(|mut atoms| atoms.any(|atom| atom == self.wm_take_focus))
			})
	}

	/// Write `WM_STATE` on a client window (`set_wm_state`,
	/// `steamcompmgr.cpp:4501`): `[state, icon window = None]`, type `WM_STATE`.
	pub fn set_wm_state(&self, window: X11Window, state: IcccmState) {
		let _ = self.conn.change_property32(
			PropMode::REPLACE,
			window,
			self.wm_state,
			self.wm_state,
			&[state as u32, x11rb::NONE],
		);
		let _ = self.conn.flush();
	}

	/// The connection's file descriptor, for registering an X-event source.
	pub fn duplicate_fd(&self) -> Option<OwnedFd> {
		self.conn.stream().as_fd().try_clone_to_owned().ok()
	}

	/// Drain pending X events.
	///
	/// Only `FocusChange` on focused windows and `PropertyChange` on the root are
	/// selected, so the events are exactly the focus transitions of the windows
	/// the compositor has focused (see [`Self::set_input_focus`]) and Steam's
	/// focus-control writes.
	pub fn poll_events(&self) -> XEvents {
		let mut events = XEvents::default();
		while let Ok(Some(event)) = self.conn.poll_for_event() {
			match event {
				XEvent::FocusOut(_) => events.focus_out = true,
				XEvent::PropertyNotify(notify) if notify.window == self.root && self.is_control_atom(notify.atom) => {
					events.control_changed = true;
				},
				XEvent::PropertyNotify(notify) if notify.window == self.root && notify.atom == self.fps_limit => {
					events.fps_limit_changed = true;
				},
				XEvent::PropertyNotify(notify) if notify.window == self.root && notify.atom == self.keyboard_layout => {
					events.keyboard_layout_changed = true;
				},
				XEvent::PropertyNotify(notify)
					if notify.window == self.root && notify.atom == self.sdr_on_hdr_brightness =>
				{
					events.sdr_brightness_changed = true;
				},
				XEvent::PropertyNotify(notify)
					if notify.window == self.root && notify.atom == self.touch_click_mode =>
				{
					events.touch_click_mode_changed = true;
				},
				_ => {},
			}
		}
		events
	}

	/// The window the X server currently reports as the input focus.
	pub fn input_focus(&self) -> Option<X11Window> {
		self.conn.get_input_focus().ok()?.reply().ok().map(|reply| reply.focus)
	}

	/// Whether the X input focus `focus` is `owner` or one of its descendants.
	///
	/// gamescope keeps the focus when it moved inside the same toplevel (a CEF
	/// dropdown, `steamcompmgr.cpp:8619-8623`) and only corrects it when it
	/// escaped to a different toplevel (`:8624-8628`). The parent chain is
	/// walked with `QueryTree`; the depth is bounded so a malformed tree cannot
	/// loop.
	pub fn focus_is_within(&self, focus: X11Window, owner: X11Window) -> bool {
		let mut current = focus;
		for _ in 0..16 {
			if current == owner {
				return true;
			}
			let Ok(cookie) = self.conn.query_tree(current) else {
				return false;
			};
			let Ok(reply) = cookie.reply() else {
				return false;
			};
			if reply.parent == x11rb::NONE {
				return false;
			}
			current = reply.parent;
		}
		false
	}

	/// The top-level ancestor of `window`: the last window before the root
	/// (`x11_find_toplevel_for_xid`, `steamcompmgr.cpp:10714`). `None` when the
	/// window is gone or the chain is malformed.
	pub fn toplevel_of(&self, window: X11Window) -> Option<X11Window> {
		let mut current = window;
		for _ in 0..32 {
			let reply = self.conn.query_tree(current).ok()?.reply().ok()?;
			if reply.parent == reply.root || reply.parent == x11rb::NONE {
				return Some(current);
			}
			current = reply.parent;
		}
		None
	}

	/// Publish `GAMESCOPE_PID` on the root window, the compositor's pid, which
	/// Steam's gamescope composer reads to recognise the compositor. gamescope
	/// writes one `CARDINAL` unconditionally (`steamcompmgr.cpp:9021`), and so
	/// does ferriscope: it is compositor identity, not a `--steam` behaviour.
	fn set_compositor_pid(&self, pid: u32) {
		self.set_root_cardinal(self.compositor_pid, pid);
		let _ = self.conn.flush();
	}

	/// Replace root property `atom` with one `CARDINAL`, or delete it when the
	/// app id is zero (`None`), matching gamescope's `PropModeReplace` with
	/// `nelements = 0`.
	fn set_root_cardinal(&self, atom: Atom, app_id: u32) {
		let mode = PropMode::REPLACE;
		let _ = match crate::steam::property_value(app_id) {
			Some(value) => self
				.conn
				.change_property32(mode, self.root, atom, AtomEnum::CARDINAL, &[value]),
			None => self
				.conn
				.change_property32(mode, self.root, atom, AtomEnum::CARDINAL, &[]),
		};
	}

	/// Replace root property `atom` with a `CARDINAL` array, clearing it when
	/// `values` is empty.
	fn replace_cardinal_array(&self, atom: Atom, values: &[u32]) {
		let _ = self
			.conn
			.change_property32(PropMode::REPLACE, self.root, atom, AtomEnum::CARDINAL, values);
	}

	/// Replace root property `atom` with the display name as an `XA_CARDINAL`
	/// (format 32), matching gamescope's
	/// `XChangeProperty(..., XA_CARDINAL, 32, PropModeReplace, (unsigned
	/// char *)focused_display, strlen(focused_display) + 1)`
	/// (`steamcompmgr.cpp:5388-5395`). The string bytes are reinterpreted as
	/// little-endian 32-bit words by [`pack_display_name`]; this is what `xprop`
	/// shows as the `13114, 0, 88`-style `CARDINAL` gamescope publishes (not the
	/// `UTF8_STRING` the reference commit wrote).
	fn write_focus_display(&self, atom: Atom, value: &str) {
		let words = pack_display_name(value);
		let _ = self
			.conn
			.change_property32(PropMode::REPLACE, self.root, atom, AtomEnum::CARDINAL, &words);
	}

	/// Send a `GetProperty` without waiting for the reply.
	fn request(
		&self,
		window: X11Window,
		atom: Atom,
		length: u32,
	) -> Option<Cookie<'_, RustConnection, GetPropertyReply>> {
		self.conn
			.get_property(false, window, atom, AtomEnum::ANY, 0, length)
			.ok()
	}
}

/// Wait for a pipelined `GetProperty` and drop errors and absent properties.
fn finish(cookie: Option<Cookie<'_, RustConnection, GetPropertyReply>>) -> Option<GetPropertyReply> {
	let reply = cookie?.reply().ok()?;
	(reply.type_ != u32::from(AtomEnum::NONE)).then_some(reply)
}

/// The first 32-bit value of a reply; a property present in another format
/// counts as set (`1`).
fn first_u32(reply: Option<GetPropertyReply>) -> Option<u32> {
	let reply = reply?;
	if reply.format == 32 {
		reply.value32().and_then(|mut values| values.next())
	} else {
		Some(1)
	}
}

/// Every 32-bit value of a reply.
fn all_u32(reply: &GetPropertyReply) -> Vec<u32> {
	reply.value32().map(Iterator::collect).unwrap_or_default()
}

/// Name the window manager `steamcompmgr`, as gamescope does
/// (`Xutf8SetWMProperties`, `steamcompmgr.cpp:7758`).
///
/// Proton's winex11 reads the name from the `_NET_SUPPORTING_WM_CHECK`
/// window and takes gamescope's paths for `steamcompmgr`: it skips the
/// pointer-grab dance on every focus change, sets the foreground window
/// directly, drops fullscreen and maximised states and skips iconifying. Under
/// smithay's "Smithay X WM" it did a root pointer grab on focus changes,
/// after which Xwayland delivered clicks to a window Wine could not map back
/// to the game, and Proton games stopped seeing mouse buttons.
fn identify_as_steamcompmgr(
	conn: &RustConnection,
	root: X11Window,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
	const NAME: &[u8] = b"steamcompmgr";
	let check = intern(conn, b"_NET_SUPPORTING_WM_CHECK")?;
	let reply = conn.get_property(false, root, check, AtomEnum::WINDOW, 0, 1)?.reply()?;
	let window = reply
		.value32()
		.and_then(|mut values| values.next())
		.ok_or("no _NET_SUPPORTING_WM_CHECK window")?;
	let utf8 = intern(conn, b"UTF8_STRING")?;
	let net_wm_name = intern(conn, b"_NET_WM_NAME")?;
	conn.change_property8(PropMode::REPLACE, window, net_wm_name, utf8, NAME)?;
	conn.change_property8(PropMode::REPLACE, window, AtomEnum::WM_NAME, AtomEnum::STRING, NAME)?;
	conn.flush()?;
	Ok(())
}

/// Intern `name`, surfacing a connection error as a boxed error.
fn intern(conn: &RustConnection, name: &[u8]) -> Result<Atom, Box<dyn std::error::Error + Send + Sync>> {
	Ok(conn.intern_atom(false, name)?.reply()?.atom)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_focus_display_name_is_packed_as_gamescope_cardinals() {
		// gamescope writes the name bytes as 32-bit CARDINALs with one element
		// per byte plus the NUL (`steamcompmgr.cpp:5388-5395`), so `:3` is
		// `0x333A, 0, 0` — xprop's `13114, 0, 0`.
		assert_eq!(pack_display_name(":3"), vec![0x333A, 0, 0]);
		assert_eq!(pack_display_name(":0"), vec![0x303A, 0, 0]);
		// Longer names spill across words; the byte order is little-endian.
		assert_eq!(pack_display_name(":123"), vec![0x3332_313A, 0, 0, 0, 0]);
	}
}
