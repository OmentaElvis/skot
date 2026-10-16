//! skot — manage a patch series against an upstream git repository and build the
//! result, without a fork.
//!
//! skot knows how to manage a sandbox and a patch series
//! Three modules carry most of the weight, and the split between them is the design:
//!
//! - [`git`] is the only place that talks to git, through libgit2. Nothing shells out to a
//!   `git` binary, so skot works where git is not installed.
//! - [`check`] replays a series in order, each patch against the tree the previous patches
//!   produced. The `apply` command uses the same replay and checks out the tree it built, so
//!   checking and applying cannot disagree.
//! - [`layout`] is the only module that names a path, and the only one that deletes one.

pub mod check;
pub mod cli;
pub mod commands;
pub mod config;
pub mod git;
pub mod hints;
pub mod hooks;
pub mod invocation;
pub mod layout;
pub mod phase;
pub mod report;
pub mod series;
pub mod seriesdir;
pub mod session;
pub mod state;
pub mod ui;

pub use anyhow::{Result, bail, ensure};
