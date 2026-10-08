//! Build phases.
//!
//! A run is a sequence of phases. `sync` and `apply` sandwich work skot does itself
//! between their `pre_` and `post_` points; `build` is three points where your hooks
//! do everything: prepare, build, finish. Ordering lives here rather than in a struct
//! field order so that `--from` / `--only` and the hook execution order can never
//! disagree with each other.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A named hook point in the lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HookPoint {
    PreSync,
    PostSync,
    PreApply,
    PostApply,
    PreBuild,
    Build,
    PostBuild,
}

impl HookPoint {
    /// The name this point is displayed and logged under, e.g. `post_build`.
    pub const fn key(self) -> &'static str {
        match self {
            Self::PreSync => "pre_sync",
            Self::PostSync => "post_sync",
            Self::PreApply => "pre_apply",
            Self::PostApply => "post_apply",
            Self::PreBuild => "pre_build",
            Self::Build => "build",
            Self::PostBuild => "post_build",
        }
    }

    /// The key this point is written under in `skot.toml`, without the `hooks.`
    /// prefix. Serde reads the struct with `rename_all = "kebab-case"`, so this and
    /// [`HookPoint::key`] are not the same string; conflating them is how a config
    /// that looks right silently runs no hooks at all.
    pub const fn toml_key(self) -> &'static str {
        match self {
            Self::PreSync => "pre-sync",
            Self::PostSync => "post-sync",
            Self::PreApply => "pre-apply",
            Self::PostApply => "post-apply",
            Self::PreBuild => "pre-build",
            Self::Build => "build",
            Self::PostBuild => "post-build",
        }
    }

    /// Whether a relative hook `cwd` for this point is measured from the project root
    /// rather than from the sandbox.
    ///
    /// `pre_sync`, `post_sync`, and `pre_apply` run before the sandbox exists on a first
    /// run, so measuring from it would make a cold build fail on a hook whose working
    /// directory was never there. The points from `post_apply` on run against the patched
    /// tree, where the sandbox is exactly what the hook should see; a hook that wants the
    /// tree belongs at `post_apply` or later.
    pub const fn runs_in_project_root(self) -> bool {
        matches!(self, Self::PreSync | Self::PostSync | Self::PreApply)
    }

    /// Every hook point, in lifecycle order.
    pub const ALL: [Self; 7] = [
        Self::PreSync,
        Self::PostSync,
        Self::PreApply,
        Self::PostApply,
        Self::PreBuild,
        Self::Build,
        Self::PostBuild,
    ];
}

impl fmt::Display for HookPoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.key())
    }
}

/// A phase of a run.
///
/// Serialisable because an interrupted run is recorded in `.skot/state.json` and
/// read back by the next `status`, possibly by a different build of skot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Phase {
    Sync,
    Apply,
    Build,
}

impl Phase {
    /// Every phase, in execution order.
    pub const ORDER: [Self; 3] = [Self::Sync, Self::Apply, Self::Build];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Sync => "sync",
            Self::Apply => "apply",
            Self::Build => "build",
        }
    }

    /// The hook points that run in this phase, in order.
    pub const fn hooks(self) -> &'static [HookPoint] {
        match self {
            Self::Sync => &[HookPoint::PreSync, HookPoint::PostSync],
            Self::Apply => &[HookPoint::PreApply, HookPoint::PostApply],
            Self::Build => &[HookPoint::PreBuild, HookPoint::Build, HookPoint::PostBuild],
        }
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Phase {
    type Err = UnknownPhase;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Phase::ORDER
            .into_iter()
            .find(|p| p.name().eq_ignore_ascii_case(s))
            .ok_or_else(|| UnknownPhase(s.to_owned(), UnknownPhase::expected()))
    }
}

#[derive(Debug, thiserror::Error)]
#[error("unknown phase `{0}`, expected one of: {1}")]
pub struct UnknownPhase(String, String);

impl UnknownPhase {
    pub fn expected() -> String {
        Phase::ORDER
            .iter()
            .map(|p| p.name())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Which phases a `--from` / `--only` selection resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub from: Option<Phase>,
    pub only: Option<Phase>,
}

impl Selection {
    pub const fn full() -> Self {
        Self {
            from: None,
            only: None,
        }
    }

    /// Expand into the concrete phases to run.
    ///
    /// `only` is that phase alone; `from` is that phase and everything after it.
    pub fn resolve(self) -> Vec<Phase> {
        if let Some(only) = self.only {
            let i = Phase::ORDER
                .iter()
                .position(|p| *p == only)
                .expect("phase is from ORDER");
            return Phase::ORDER[i..=i].to_vec();
        }
        match self.from {
            Some(from) => {
                let i = Phase::ORDER
                    .iter()
                    .position(|p| *p == from)
                    .expect("phase is from ORDER");
                Phase::ORDER[i..].to_vec()
            }
            None => Phase::ORDER.to_vec(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phases_round_trip_through_strings() {
        for p in Phase::ORDER {
            assert_eq!(p.name().parse::<Phase>().unwrap(), p);
        }
        assert_eq!("BUILD".parse::<Phase>().unwrap(), Phase::Build);
    }

    #[test]
    fn unknown_phase_error_lists_the_valid_ones() {
        let err = "nope".parse::<Phase>().unwrap_err().to_string();
        for p in Phase::ORDER {
            assert!(err.contains(p.name()), "error should mention {p}: {err}");
        }
    }

    #[test]
    fn only_means_just_that_phase() {
        let sel = Selection {
            from: None,
            only: Some(Phase::Apply),
        };
        assert_eq!(sel.resolve(), vec![Phase::Apply]);
    }

    #[test]
    fn from_means_that_phase_and_everything_after() {
        let sel = Selection {
            from: Some(Phase::Apply),
            only: None,
        };
        assert_eq!(sel.resolve(), vec![Phase::Apply, Phase::Build]);
    }

    #[test]
    fn from_and_only_together_means_only() {
        let sel = Selection {
            from: Some(Phase::Sync),
            only: Some(Phase::Apply),
        };
        assert_eq!(sel.resolve(), vec![Phase::Apply]);
    }

    #[test]
    fn every_hook_point_is_reachable_from_some_phase() {
        let reached: std::collections::BTreeSet<_> = Phase::ORDER
            .iter()
            .flat_map(|p| p.hooks().iter().copied())
            .collect();
        let all: std::collections::BTreeSet<_> = HookPoint::ALL.into_iter().collect();
        assert_eq!(reached, all, "a hook point exists that no phase ever runs");
    }

    #[test]
    fn the_points_that_run_on_a_cold_run_measure_from_the_project_root() {
        let cold_run = [HookPoint::PreSync, HookPoint::PostSync, HookPoint::PreApply];
        for point in HookPoint::ALL {
            assert_eq!(
                point.runs_in_project_root(),
                cold_run.contains(&point),
                "{point} has the wrong base directory"
            );
        }
    }
}
