//! Frontend-neutral view model for argent.
//!
//! Nothing here renders. A frontend turns [`Transcript`] entries and
//! [`Metrics`] into pixels; this crate only decides *what* is on screen.
//!
//! The point is that the desktop window, the terminal renderer and any future
//! frontend share one description of a run. Swapping the renderer then means
//! writing a new view, not re-deriving the state machine.

pub mod approval;
pub mod command;
#[cfg(any(test, feature = "test-support"))]
pub mod failing_provider;
pub mod icon;
pub mod metrics;
pub mod palette;
pub mod session;
pub mod transcript;
pub mod turn;
pub mod ui_request;

pub use approval::{check_approval, ApprovalEvents, ApprovalViolation};
pub use command::Command;
#[cfg(any(test, feature = "test-support"))]
pub use failing_provider::failing_base_url;
pub use icon::{rgba as icon_rgba, write_ico};
pub use metrics::{format_duration, format_tokens, progress_bar, Metrics};
pub use palette::{Palette, Rgb, Role};
pub use session::{
    check_session_switch, new_session, opened_session, SessionEvents, SessionViolation,
};
pub use transcript::{Entry, NoticeKind, ToolRun, ToolState, Transcript};
pub use turn::{check_turn, failed_turn, finished_turn, ContractViolation, TurnEvents};
pub use ui_request::{UiBridge, UiMethod, UiRequest, UiResponse};
