//! Whether an X11 window may present straight to a Wayland surface instead of
//! through Xwayland: gamescope's `GamescopeSurfaceData::canBypassXWayland`
//! (`layer/VkLayer_FROG_gamescope_wsi.cpp`), over a query trait so the decision
//! can be tested without an X server.

use crate::config::Flags;

/// A window's position and size, in X11 terms (`VkRect2D`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
	pub x: i32,
	pub y: i32,
	pub width: u32,
	pub height: u32,
}

/// The X11 facts the decision reads. Every query can fail (`None`), which
/// refuses the bypass.
pub trait WindowQuery {
	fn rect(&self, window: u32) -> Option<Rect>;
	/// A `CARDINAL` property of the window.
	fn cardinal(&self, window: u32, name: &str) -> Option<u32>;
	/// The window's parent, `None` for a top-level window (`getParentWindow`).
	fn parent(&self, window: u32) -> Option<u32>;
	fn is_override_redirect(&self, window: u32) -> bool;
	/// Whether the window has `WM_CLASS`.
	fn has_wm_class(&self, window: u32) -> bool;
	/// The size of the largest viewable, non-override-redirect child, clipped
	/// to the window (`getLargestObscuringChildWindowSize`).
	fn largest_obscuring_child(&self, window: u32) -> Option<(u32, u32)>;
	/// The ancestor of the window that is a child of the root (`getToplevelWindow`).
	fn toplevel(&self, window: u32) -> Option<u32>;
}

/// The outcome of a check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bypass {
	pub allowed: bool,
	/// The window's rectangle as read, for detecting a resize.
	pub rect: Option<Rect>,
}

/// Whether `window` can present through the bypass surface.
///
/// `hdr_colourspace` is whether the swapchain in question is HDR: the Wine
/// offscreen gate is skipped for it because the GDI blit it guards can only
/// carry SDR, so an HDR swapchain always bypasses.
pub fn can_bypass<Q: WindowQuery>(query: &Q, window: u32, hdr_colourspace: bool, flags: Flags) -> Bypass {
	let Some(rect) = query.rect(window) else {
		return Bypass {
			allowed: false,
			rect: None,
		};
	};
	let refuse = Bypass {
		allowed: false,
		rect: Some(rect),
	};

	// Never bypass windows Wine presents offscreen to GDI-blit onto the real
	// toplevel: marked `_WINE_ALLOW_FLIP=0`, or parented under Wine's unnamed
	// 1x1 override-redirect dummy window.
	if !hdr_colourspace {
		match query.cardinal(window, "_WINE_ALLOW_FLIP") {
			Some(0) => return refuse,
			Some(_) => {},
			None => {
				if let Some(parent) = query.parent(window)
					&& let Some(parent_rect) = query.rect(parent)
					&& parent_rect.width == 1
					&& parent_rect.height == 1
					&& query.is_override_redirect(parent)
					&& !query.has_wm_class(parent)
				{
					return refuse;
				}
			},
		}
	}

	let (Some(obscuring), Some(toplevel)) = (query.largest_obscuring_child(window), query.toplevel(window)) else {
		return refuse;
	};
	let Some(toplevel_rect) = query.rect(toplevel) else {
		return refuse;
	};

	// Some games have a 1280x800 top-level window and a 1280x720 child for
	// "fullscreen"; bypassing avoids the Glamor work on the Xwayland side.
	if flags.contains(Flags::FORCE_BYPASS) {
		return Bypass {
			allowed: true,
			rect: Some(rect),
		};
	}

	// A child window bigger than 1x1 in front of us (composite redirect
	// dummies are 1x1) means we cannot flip.
	if obscuring.0 > 1 || obscuring.1 > 1 {
		return refuse;
	}

	// The window must be within 2 px of its top level's size and sit at its
	// origin. A 1x1 top level is winex11's way to say "empty", not a size.
	if toplevel != window
		&& (toplevel_rect.width > 1 || toplevel_rect.height > 1)
		&& (rect.x.abs() > 1
			|| rect.y.abs() > 1
			|| (toplevel_rect.width as i64 - rect.width as i64).abs() > 2
			|| (toplevel_rect.height as i64 - rect.height as i64).abs() > 2)
	{
		return refuse;
	}

	Bypass {
		allowed: true,
		rect: Some(rect),
	}
}

#[cfg(test)]
mod tests {
	use std::collections::HashMap;

	use super::*;

	/// A fake window tree.
	#[derive(Default)]
	struct Tree {
		rects: HashMap<u32, Rect>,
		cardinals: HashMap<(u32, &'static str), u32>,
		parents: HashMap<u32, u32>,
		override_redirect: Vec<u32>,
		wm_class: Vec<u32>,
		obscuring: HashMap<u32, (u32, u32)>,
		toplevels: HashMap<u32, u32>,
	}

	impl WindowQuery for Tree {
		fn rect(&self, window: u32) -> Option<Rect> {
			self.rects.get(&window).copied()
		}
		fn cardinal(&self, window: u32, name: &str) -> Option<u32> {
			self.cardinals.get(&(window, name)).copied()
		}
		fn parent(&self, window: u32) -> Option<u32> {
			self.parents.get(&window).copied()
		}
		fn is_override_redirect(&self, window: u32) -> bool {
			self.override_redirect.contains(&window)
		}
		fn has_wm_class(&self, window: u32) -> bool {
			self.wm_class.contains(&window)
		}
		fn largest_obscuring_child(&self, window: u32) -> Option<(u32, u32)> {
			Some(self.obscuring.get(&window).copied().unwrap_or((0, 0)))
		}
		fn toplevel(&self, window: u32) -> Option<u32> {
			Some(self.toplevels.get(&window).copied().unwrap_or(window))
		}
	}

	const fn rect(x: i32, y: i32, width: u32, height: u32) -> Rect {
		Rect { x, y, width, height }
	}

	/// A top-level window 1, with child window 2 filling it.
	fn game() -> Tree {
		let mut tree = Tree::default();
		tree.rects.insert(1, rect(0, 0, 1280, 720));
		tree.rects.insert(2, rect(0, 0, 1280, 720));
		tree.toplevels.insert(2, 1);
		tree.parents.insert(2, 1);
		tree
	}

	fn allowed(tree: &Tree, window: u32, hdr: bool, flags: Flags) -> bool {
		can_bypass(tree, window, hdr, flags).allowed
	}

	#[test]
	fn a_window_that_fills_its_toplevel_bypasses() {
		assert!(allowed(&game(), 2, false, Flags::default()));
		assert!(allowed(&game(), 1, false, Flags::default()));
	}

	#[test]
	fn an_unreadable_window_never_bypasses() {
		let tree = Tree::default();
		assert_eq!(
			can_bypass(&tree, 7, false, Flags::default()),
			Bypass {
				allowed: false,
				rect: None
			}
		);
	}

	#[test]
	fn the_rectangle_is_returned_for_resize_detection() {
		assert_eq!(
			can_bypass(&game(), 2, false, Flags::default()).rect,
			Some(rect(0, 0, 1280, 720))
		);
	}

	#[test]
	fn a_child_within_two_pixels_of_the_toplevel_still_bypasses() {
		let mut tree = game();
		tree.rects.insert(2, rect(0, 0, 1280, 722));
		assert!(allowed(&tree, 2, false, Flags::default()));
		tree.rects.insert(2, rect(0, 0, 1280, 723));
		assert!(!allowed(&tree, 2, false, Flags::default()));
	}

	#[test]
	fn a_child_offset_from_its_parent_does_not_bypass() {
		let mut tree = game();
		tree.rects.insert(2, rect(2, 0, 1280, 720));
		assert!(!allowed(&tree, 2, false, Flags::default()));
		tree.rects.insert(2, rect(1, 1, 1280, 720));
		assert!(allowed(&tree, 2, false, Flags::default()));
	}

	#[test]
	fn a_one_pixel_toplevel_is_not_a_size_to_match() {
		let mut tree = game();
		tree.rects.insert(1, rect(0, 0, 1, 1));
		assert!(allowed(&tree, 2, false, Flags::default()));
	}

	#[test]
	fn an_obscuring_child_bigger_than_one_pixel_blocks_the_bypass() {
		let mut tree = game();
		tree.obscuring.insert(2, (1, 1));
		assert!(allowed(&tree, 2, false, Flags::default()));
		tree.obscuring.insert(2, (2, 1));
		assert!(!allowed(&tree, 2, false, Flags::default()));
		tree.obscuring.insert(2, (1, 640));
		assert!(!allowed(&tree, 2, false, Flags::default()));
	}

	#[test]
	fn force_bypass_overrides_the_geometry_checks() {
		let mut tree = game();
		tree.rects.insert(2, rect(0, 0, 1280, 400));
		tree.obscuring.insert(2, (100, 100));
		assert!(!allowed(&tree, 2, false, Flags::default()));
		assert!(allowed(&tree, 2, false, Flags::FORCE_BYPASS));
	}

	#[test]
	fn wine_windows_marked_not_to_flip_never_bypass_sdr() {
		let mut tree = game();
		tree.cardinals.insert((2, "_WINE_ALLOW_FLIP"), 0);
		assert!(!allowed(&tree, 2, false, Flags::default()));
		assert!(!allowed(&tree, 2, false, Flags::FORCE_BYPASS));
		tree.cardinals.insert((2, "_WINE_ALLOW_FLIP"), 1);
		assert!(allowed(&tree, 2, false, Flags::default()));
	}

	#[test]
	fn hdr_skips_the_wine_gate() {
		let mut tree = game();
		tree.cardinals.insert((2, "_WINE_ALLOW_FLIP"), 0);
		assert!(allowed(&tree, 2, true, Flags::default()));
	}

	#[test]
	fn a_window_parked_under_wines_dummy_parent_never_bypasses_sdr() {
		let mut tree = game();
		// The dummy: 1x1, override-redirect, no WM_CLASS, not a toplevel's own window.
		tree.rects.insert(9, rect(0, 0, 1, 1));
		tree.override_redirect.push(9);
		tree.parents.insert(2, 9);
		assert!(!allowed(&tree, 2, false, Flags::default()));
		assert!(allowed(&tree, 2, true, Flags::default()));

		// A named parent is a real window.
		tree.wm_class.push(9);
		assert!(allowed(&tree, 2, false, Flags::default()));
	}
}
