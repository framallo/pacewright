//! `pacewright-adapter-recipe` — run declarative KDL recipes as paced pacewright tasks.
//!
//! This is the consumer side of the recipe engine that lives in the chrome-agent fork
//! (`docs/specs/2026-07-08-recipe-engine-in-chrome-agent.md`). It replaces the
//! hand-written per-site adapters: the site logic is a `.kdl` recipe (installed via
//! `pcw recipe add`), and a single generic [`RecipeAdapter`] routes an `(adapter, action)`
//! to the matching recipe and runs it through a [`RecipeRunner`].
//!
//! - [`registry`] — enumerate installed recipes and read their routing/pacing metadata
//!   (name, `limit-key`s, `var`s) with a shallow KDL walk; no chrome-agent process needed.
//! - [`runner`] — the `chrome-agent recipe run` subprocess boundary + error-class recovery.
//! - [`adapter`] — the `Adapter` impl gluing the two together.

pub mod adapter;
pub mod auth;
pub mod pool;
pub mod registry;
pub mod runner;
pub mod schedule;

pub use adapter::RecipeAdapter;
pub use auth::{
    AccountInfo, AccountStatus, AuthManager, CliLoginLauncher, LoginLauncher, NativeLoginLauncher,
};
pub use pool::{ChromeLease, ChromePool};
pub use registry::{RecipeMeta, RecipeRegistry, RecipeVar};
pub use runner::{
    chrome_agent_bin, recipe_subcommand_available, CliRecipeRunner, NativeRecipeRunner,
    RecipeRunner, RunOpts,
};
pub use schedule::{partition, reconcile, validate, ReconcileReport, ScheduleEntry, Timing};
