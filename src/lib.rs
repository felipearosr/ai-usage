//! AIU — AI coding subscription usage tracker.
//!
//! Privacy: this crate handles usage metadata only (counts, tokens,
//! timestamps, model IDs, machine IDs). Prompts, responses, source code, and
//! project information never enter this codebase.

pub mod adapters;
pub mod cli;
pub mod collect;
pub mod discover;
pub mod error;
pub mod hash;
pub mod identity;
pub mod import;
pub mod migrations;
pub mod paths;
pub mod report;
pub mod sources;
pub mod store;
pub mod sync;
pub mod utc;
