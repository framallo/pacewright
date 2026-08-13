//! chrome-agent as a library.
//!
//! Historically chrome-agent was binary-only (`[[bin]]`, no `lib.rs`), so consumers like pacewright
//! had to shell out to the CLI (`chrome-agent recipe run …`) — a fork/exec per verb, a PATH/symlink
//! dependency, and stdout-JSON parsing. Exposing the same modules as a library lets a Rust consumer
//! link the CDP driver + KDL recipe engine directly and drive Chrome in-process.
//!
//! The binary (`src/main.rs`) is now a thin wrapper over [`run::run`]; the recipe engine entry points
//! are [`recipe::run_recipe`] / [`recipe::run_recipe_native`], and the low-level browser session
//! lives in [`browser`]/[`session`].
pub mod api;
pub mod base64;
pub mod browser;
pub mod cdp;
pub mod cli;
pub mod commands;
#[cfg(unix)]
pub mod daemon;
pub mod element;
pub mod element_ref;
pub mod geometry;
pub mod pipe;
pub mod pipe_dispatch;
pub mod recipe;
pub mod run;
pub mod run_helpers;
pub mod session;
pub mod setup;
pub mod snapshot;
pub mod truncate;

/// Shared error type alias used across the crate.
pub type BoxError = Box<dyn std::error::Error>;
