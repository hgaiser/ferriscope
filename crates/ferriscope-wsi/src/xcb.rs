//! X11 access through the application's own `xcb_connection_t`, with libxcb
//! loaded by `dlopen` (the process always has it; the layer does not link it).
//!
//! Ports gamescope's `layer/xcb_helpers.hpp`.

use std::{
	collections::HashMap,
	ffi::{CStr, c_char, c_void},
	sync::{Mutex, OnceLock},
};

use crate::bypass::{Rect, WindowQuery};

#[repr(C)]
#[derive(Clone, Copy)]
struct Cookie {
	sequence: u32,
}

#[repr(C)]
struct InternAtomReply {
	response_type: u8,
	pad0: u8,
	sequence: u16,
	length: u32,
	atom: u32,
}

#[repr(C)]
struct GetPropertyReply {
	response_type: u8,
	format: u8,
	sequence: u16,
	length: u32,
	type_: u32,
	bytes_after: u32,
	value_len: u32,
	pad0: [u8; 12],
}

#[repr(C)]
struct GetGeometryReply {
	response_type: u8,
	depth: u8,
	sequence: u16,
	length: u32,
	root: u32,
	x: i16,
	y: i16,
	width: u16,
	height: u16,
	border_width: u16,
	pad0: [u8; 2],
}

#[repr(C)]
struct QueryTreeReply {
	response_type: u8,
	pad0: u8,
	sequence: u16,
	length: u32,
	root: u32,
	parent: u32,
	children_len: u16,
	pad1: [u8; 14],
}

#[repr(C)]
struct GetWindowAttributesReply {
	response_type: u8,
	backing_store: u8,
	sequence: u16,
	length: u32,
	visual: u32,
	class: u16,
	bit_gravity: u8,
	win_gravity: u8,
	backing_planes: u32,
	backing_pixel: u32,
	save_under: u8,
	map_is_installed: u8,
	map_state: u8,
	override_redirect: u8,
	colormap: u32,
	all_event_masks: u32,
	your_event_mask: u32,
	do_not_propagate_mask: u16,
	pad0: [u8; 2],
}

const XCB_ATOM_CARDINAL: u32 = 6;
const XCB_ATOM_WM_CLASS: u32 = 67;
const XCB_GET_PROPERTY_TYPE_ANY: u32 = 0;
const XCB_MAP_STATE_VIEWABLE: u8 = 2;

type Conn = *mut c_void;

/// The libxcb entry points the layer uses.
struct Fns {
	intern_atom: unsafe extern "C" fn(Conn, u8, u16, *const c_char) -> Cookie,
	intern_atom_reply: unsafe extern "C" fn(Conn, Cookie, *mut *mut c_void) -> *mut InternAtomReply,
	get_property: unsafe extern "C" fn(Conn, u8, u32, u32, u32, u32, u32) -> Cookie,
	get_property_reply: unsafe extern "C" fn(Conn, Cookie, *mut *mut c_void) -> *mut GetPropertyReply,
	get_property_value: unsafe extern "C" fn(*const GetPropertyReply) -> *mut c_void,
	get_geometry: unsafe extern "C" fn(Conn, u32) -> Cookie,
	get_geometry_reply: unsafe extern "C" fn(Conn, Cookie, *mut *mut c_void) -> *mut GetGeometryReply,
	query_tree: unsafe extern "C" fn(Conn, u32) -> Cookie,
	query_tree_reply: unsafe extern "C" fn(Conn, Cookie, *mut *mut c_void) -> *mut QueryTreeReply,
	query_tree_children: unsafe extern "C" fn(*const QueryTreeReply) -> *mut u32,
	get_window_attributes: unsafe extern "C" fn(Conn, u32) -> Cookie,
	get_window_attributes_reply: unsafe extern "C" fn(Conn, Cookie, *mut *mut c_void) -> *mut GetWindowAttributesReply,
}

fn fns() -> Option<&'static Fns> {
	static FNS: OnceLock<Option<Fns>> = OnceLock::new();
	FNS.get_or_init(|| unsafe {
		let lib = libc::dlopen(c"libxcb.so.1".as_ptr(), libc::RTLD_LAZY | libc::RTLD_GLOBAL);
		if lib.is_null() {
			crate::log_warn!("dlopen(libxcb.so.1) failed");
			return None;
		}
		macro_rules! symbol {
			($name:literal) => {{
				let pointer = libc::dlsym(lib, $name.as_ptr());
				if pointer.is_null() {
					crate::log_warn!("dlsym({}) failed", $name.to_string_lossy());
					return None;
				}
				std::mem::transmute(pointer)
			}};
		}
		Some(Fns {
			intern_atom: symbol!(c"xcb_intern_atom"),
			intern_atom_reply: symbol!(c"xcb_intern_atom_reply"),
			get_property: symbol!(c"xcb_get_property"),
			get_property_reply: symbol!(c"xcb_get_property_reply"),
			get_property_value: symbol!(c"xcb_get_property_value"),
			get_geometry: symbol!(c"xcb_get_geometry"),
			get_geometry_reply: symbol!(c"xcb_get_geometry_reply"),
			query_tree: symbol!(c"xcb_query_tree"),
			query_tree_reply: symbol!(c"xcb_query_tree_reply"),
			query_tree_children: symbol!(c"xcb_query_tree_children"),
			get_window_attributes: symbol!(c"xcb_get_window_attributes"),
			get_window_attributes_reply: symbol!(c"xcb_get_window_attributes_reply"),
		})
	})
	.as_ref()
}

/// An `xcb_connection_t` the application owns. It is thread-safe by xcb's
/// design and outlives every surface made on it, by Vulkan's rules.
#[derive(Clone, Copy)]
pub struct X11 {
	connection: Conn,
}

// SAFETY: xcb connections are thread-safe.
unsafe impl Send for X11 {}
unsafe impl Sync for X11 {}

/// Convert a libX11 `Display*` to its `xcb_connection_t*` (`XGetXCBConnection`).
///
/// # Safety
///
/// `display` must be a valid `Display*`.
pub unsafe fn xlib_to_xcb_connection(display: *mut c_void) -> Conn {
	type GetXcbConnection = unsafe extern "C" fn(*mut c_void) -> Conn;
	static SYMBOL: OnceLock<Option<GetXcbConnection>> = OnceLock::new();
	let symbol = SYMBOL.get_or_init(|| unsafe {
		let lib = libc::dlopen(c"libX11-xcb.so.1".as_ptr(), libc::RTLD_LAZY | libc::RTLD_GLOBAL);
		if lib.is_null() {
			crate::log_warn!("dlopen(libX11-xcb.so.1) failed");
			return None;
		}
		let pointer = libc::dlsym(lib, c"XGetXCBConnection".as_ptr());
		(!pointer.is_null()).then(|| std::mem::transmute::<*mut c_void, GetXcbConnection>(pointer))
	});
	match symbol {
		Some(f) => unsafe { f(display) },
		None => std::ptr::null_mut(),
	}
}

/// Interned atoms per connection.
fn atom_cache() -> &'static Mutex<HashMap<(usize, String), u32>> {
	static CACHE: OnceLock<Mutex<HashMap<(usize, String), u32>>> = OnceLock::new();
	CACHE.get_or_init(Default::default)
}

impl X11 {
	/// # Safety
	///
	/// `connection` must be a valid `xcb_connection_t*` that outlives the value.
	pub unsafe fn new(connection: Conn) -> Option<Self> {
		(!connection.is_null()).then_some(Self { connection })
	}

	fn atom(&self, name: &str) -> Option<u32> {
		let key = (self.connection as usize, name.to_string());
		if let Some(atom) = atom_cache().lock().ok()?.get(&key) {
			return Some(*atom);
		}
		let f = fns()?;
		let atom = unsafe {
			let cookie = (f.intern_atom)(self.connection, 0, name.len() as u16, name.as_ptr().cast());
			let reply = (f.intern_atom_reply)(self.connection, cookie, std::ptr::null_mut());
			if reply.is_null() {
				return None;
			}
			let atom = (*reply).atom;
			libc::free(reply.cast());
			atom
		};
		atom_cache().lock().ok()?.insert(key, atom);
		Some(atom)
	}

	/// The first 32-bit `CARDINAL` of `window`'s property `atom`.
	fn cardinal_by_atom(&self, window: u32, atom: u32) -> Option<u32> {
		let f = fns()?;
		unsafe {
			let cookie = (f.get_property)(self.connection, 0, window, atom, XCB_ATOM_CARDINAL, 0, 1);
			let reply = (f.get_property_reply)(self.connection, cookie, std::ptr::null_mut());
			if reply.is_null() {
				return None;
			}
			let value = ((*reply).type_ == XCB_ATOM_CARDINAL && (*reply).format == 32 && (*reply).value_len >= 1)
				.then(|| *((f.get_property_value)(reply) as *const u32));
			libc::free(reply.cast());
			value
		}
	}

	fn query_tree(&self, window: u32) -> Option<(u32, u32, Vec<u32>)> {
		let f = fns()?;
		unsafe {
			let cookie = (f.query_tree)(self.connection, window);
			let reply = (f.query_tree_reply)(self.connection, cookie, std::ptr::null_mut());
			if reply.is_null() {
				return None;
			}
			let children = (f.query_tree_children)(reply);
			let list = std::slice::from_raw_parts(children, (*reply).children_len as usize).to_vec();
			let result = ((*reply).root, (*reply).parent, list);
			libc::free(reply.cast());
			Some(result)
		}
	}

	fn attributes(&self, window: u32) -> Option<(u8, bool)> {
		let f = fns()?;
		unsafe {
			let cookie = (f.get_window_attributes)(self.connection, window);
			let reply = (f.get_window_attributes_reply)(self.connection, cookie, std::ptr::null_mut());
			if reply.is_null() {
				return None;
			}
			let result = ((*reply).map_state, (*reply).override_redirect != 0);
			libc::free(reply.cast());
			Some(result)
		}
	}

	/// The root window that `window` is on.
	fn root_of(&self, window: u32) -> Option<u32> {
		self.query_tree(window).map(|(root, ..)| root)
	}

	/// A 32-bit `CARDINAL` property of the root window of `window`'s screen
	/// (gamescope's `xcb::getPropertyValue(connection, name)`).
	pub fn root_cardinal(&self, window: u32, name: &str) -> Option<u32> {
		let root = self.root_of(window)?;
		self.cardinal_by_atom(root, self.atom(name)?)
	}
}

impl WindowQuery for X11 {
	fn rect(&self, window: u32) -> Option<Rect> {
		let f = fns()?;
		unsafe {
			let cookie = (f.get_geometry)(self.connection, window);
			let reply = (f.get_geometry_reply)(self.connection, cookie, std::ptr::null_mut());
			if reply.is_null() {
				return None;
			}
			let rect = Rect {
				x: i32::from((*reply).x),
				y: i32::from((*reply).y),
				width: u32::from((*reply).width),
				height: u32::from((*reply).height),
			};
			libc::free(reply.cast());
			Some(rect)
		}
	}

	fn cardinal(&self, window: u32, name: &str) -> Option<u32> {
		self.cardinal_by_atom(window, self.atom(name)?)
	}

	fn parent(&self, window: u32) -> Option<u32> {
		let (root, parent, _) = self.query_tree(window)?;
		(root != window && parent != root).then_some(parent)
	}

	fn is_override_redirect(&self, window: u32) -> bool {
		self.attributes(window)
			.is_some_and(|(_, override_redirect)| override_redirect)
	}

	fn has_wm_class(&self, window: u32) -> bool {
		let Some(f) = fns() else { return false };
		unsafe {
			let cookie = (f.get_property)(
				self.connection,
				0,
				window,
				XCB_ATOM_WM_CLASS,
				XCB_GET_PROPERTY_TYPE_ANY,
				0,
				0,
			);
			let reply = (f.get_property_reply)(self.connection, cookie, std::ptr::null_mut());
			if reply.is_null() {
				return false;
			}
			// XCB_NONE means the property does not exist.
			let present = (*reply).type_ != 0;
			libc::free(reply.cast());
			present
		}
	}

	fn largest_obscuring_child(&self, window: u32) -> Option<(u32, u32)> {
		let (_, _, children) = self.query_tree(window)?;
		let ours = self.rect(window)?;
		let mut largest = (0u32, 0u32);
		for child in children {
			let obscuring = self.attributes(child).is_some_and(|(map_state, override_redirect)| {
				map_state == XCB_MAP_STATE_VIEWABLE && !override_redirect
			});
			if !obscuring {
				continue;
			}
			if let Some(rect) = self.rect(child) {
				let clipped = clip_to(ours, rect);
				largest = (largest.0.max(clipped.0), largest.1.max(clipped.1));
			}
		}
		Some(largest)
	}

	fn toplevel(&self, mut window: u32) -> Option<u32> {
		loop {
			let (root, parent, _) = self.query_tree(window)?;
			if root == parent {
				return Some(window);
			}
			window = parent;
		}
	}
}

/// The size of `child` clipped to `parent` (`xcb::clip`): the child's offset is
/// relative to the parent.
fn clip_to(parent: Rect, child: Rect) -> (u32, u32) {
	let width = (i64::from(parent.width) - i64::from(child.x)).max(0) as u32;
	let height = (i64::from(parent.height) - i64::from(child.y)).max(0) as u32;
	(child.width.min(width), child.height.min(height))
}

/// Read `name` from a C string for logging.
pub fn c_name(ptr: *const c_char) -> String {
	if ptr.is_null() {
		String::new()
	} else {
		unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn reply_layouts_match_xcb() {
		assert_eq!(std::mem::size_of::<GetPropertyReply>(), 32);
		assert_eq!(std::mem::offset_of!(GetPropertyReply, type_), 8);
		assert_eq!(std::mem::offset_of!(GetPropertyReply, value_len), 16);
		assert_eq!(std::mem::size_of::<GetWindowAttributesReply>(), 44);
		assert_eq!(std::mem::offset_of!(GetWindowAttributesReply, map_state), 26);
		assert_eq!(std::mem::offset_of!(GetWindowAttributesReply, override_redirect), 27);
		assert_eq!(std::mem::size_of::<GetGeometryReply>(), 24);
		assert_eq!(std::mem::offset_of!(GetGeometryReply, width), 16);
		assert_eq!(std::mem::size_of::<QueryTreeReply>(), 32);
		assert_eq!(std::mem::offset_of!(QueryTreeReply, children_len), 16);
		assert_eq!(std::mem::size_of::<InternAtomReply>(), 12);
	}

	#[test]
	fn children_are_clipped_to_their_parent() {
		let parent = Rect {
			x: 0,
			y: 0,
			width: 100,
			height: 80,
		};
		let inside = Rect {
			x: 10,
			y: 20,
			width: 50,
			height: 30,
		};
		assert_eq!(clip_to(parent, inside), (50, 30));
		let overhanging = Rect {
			x: 60,
			y: 70,
			width: 100,
			height: 100,
		};
		assert_eq!(clip_to(parent, overhanging), (40, 10));
		let outside = Rect {
			x: 200,
			y: 0,
			width: 10,
			height: 10,
		};
		assert_eq!(clip_to(parent, outside), (0, 10));
	}
}
