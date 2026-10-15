//! The smithay delegate and handler implementations for `FerriscopeState`.

mod compositor;
mod constraints;
pub(crate) mod seat;
mod xdg;
mod xwm;

use crate::shell::FerriscopeState;

smithay::delegate_dispatch2!(FerriscopeState);
