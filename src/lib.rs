//! AIU — AI coding subscription usage tracker.
//!
//! Privacy: this crate handles usage metadata only (counts, tokens,
//! timestamps, model IDs, machine IDs). Prompts, responses, source code, and
//! project information never enter this codebase.

pub mod cli;
pub mod error;
pub mod migrations;
pub mod paths;
pub mod report;
pub mod store;
pub mod utc;
