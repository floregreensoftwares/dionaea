//! The rules of app health that do not depend on a database or a network:
//! what is masked in an error message, how a route becomes a pattern, and how
//! stack frames are written and read. Pure functions, no I/O, no clock, and
//! every one is bounded in time and in output size by its input.
//!
//! Each rule is pinned by the conformance vectors in `vectors/health/`, which
//! every SDK that scrubs or templates on the device has to pass too.

pub mod frames;
pub mod route;
pub mod scrub;

pub use frames::{Frame, normalize as normalize_frames};
pub use route::template_route;
pub use scrub::{scrub_message, strip_query};
