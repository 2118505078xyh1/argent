//! argent background jobs: long-running commands that outlive a turn.
//!
//! - [`job`] owns one process and its log.
//! - [`manager`] owns the registry and mints ids.
//! - [`tools`] is the agent-facing surface.

pub mod job;
pub mod manager;
pub mod tools;

pub use job::{Job, JobStatus};
pub use manager::{JobManager, JobSummary};
pub use tools::JobsTool;
