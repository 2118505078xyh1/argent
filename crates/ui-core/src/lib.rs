//! Frontend-neutral view model for argent.
//!
//! Nothing here renders. A frontend turns [`Transcript`] entries and
//! [`Metrics`] into pixels; this crate only decides *what* is on screen.
//!
//! The point is that the desktop window, the terminal renderer and any future
//! frontend share one description of a run. Swapping the renderer then means
//! writing a new view, not re-deriving the state machine.

pub mod command;
pub mod icon;
pub mod metrics;
pub mod palette;
pub mod transcript;

pub use command::Command;
pub use icon::{rgba as icon_rgba, write_ico};
pub use metrics::{format_duration, format_tokens, progress_bar, Metrics};
pub use palette::{Palette, Rgb, Role};
pub use transcript::{Entry, NoticeKind, ToolRun, ToolState, Transcript};
