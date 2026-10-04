//! `skot.toml` — hooks, upstream, sandbox policy. Nothing else.
//!
//! skot deliberately has no opinion about your language, your build system, or
//! where your artifact goes. Everything of that sort is a hook string here, and the
//! tool never looks inside one.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::phase::HookPoint;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    #[serde(default)]
    pub upstream: Upstream,
    #[serde(default)]
    pub sandbox: Sandbox,
    /// Extra environment for every hook, in addition to the injected `SKOT_*` vars.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub hooks: Hooks,
    /// Named hooks for anything outside the fixed lifecycle.
    #[serde(default, rename = "hook")]
    pub extra: Vec<ExtraHook>,
}

impl Config {
    /// Parse a config file. Errors name the file and, where toml can tell us, the key.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).context(format!("reading {}", path.display()))?;
        Self::parse(&text).context(format!("parsing {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        toml::from_str(text).map_err(|err| anyhow::anyhow!("{err}"))
    }

    /// The command configured at a lifecycle hook point, if any.
    pub fn hook(&self, point: HookPoint) -> Option<&str> {
        let cmd = self.hooks.get(point)?;
        (!cmd.trim().is_empty()).then_some(cmd)
    }

    /// Every configured hook point, in lifecycle order. Used by `skot init` output
    /// and by the hook runner's dry-run listing.
    pub fn configured_hooks(&self) -> Vec<(HookPoint, &str)> {
        HookPoint::ALL
            .into_iter()
            .filter_map(|p| self.hook(p).map(|cmd| (p, cmd)))
            .collect()
    }

    /// The annotated template `skot init` writes. Every hook point is present but
    /// empty, so a new user can see every place their build could hook in without
    /// having to know the vocabulary.
    pub fn template() -> String {
        include_str!("../templates/skot.toml.template").to_owned()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Upstream {
    pub url: Option<String>,
    pub default_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Sandbox {
    /// `persistent` keeps the sandbox across runs and resets it to a ref.
    /// `ephemeral` creates it, uses it, and destroys it. Right for CI.
    ///
    /// Defaults to `persistent` so that a `[sandbox]` section carrying only `keep` — the
    /// common case, since that is the setting with something to say — is not rejected for
    /// omitting the other half.
    #[serde(default)]
    pub policy: SandboxPolicy,
    /// Paths the sandbox reset must not touch, so expensive dependency state survives a ref
    /// change.
    ///
    /// A reset force-checks out without removing untracked files, which is what actually
    /// keeps an untracked cache such as `node_modules/` or `target/` alive. Listing it still
    /// matters: it records the dependency where a reader will look for it.
    ///
    /// For a path upstream *tracks*, listing it means the tracked version is put back when a
    /// reset would delete it because the new base does not have it. Not the user's unsaved
    /// edits to it — a reset discards sandbox work by design — and only for the transition
    /// onto a base that dropped the path, since after that there is nothing to restore from.
    #[serde(default)]
    pub keep: Vec<String>,
}

impl Default for Sandbox {
    fn default() -> Self {
        Self {
            policy: SandboxPolicy::Persistent,
            keep: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxPolicy {
    #[default]
    Persistent,
    Ephemeral,
}

impl std::fmt::Display for SandboxPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Persistent => "persistent",
            Self::Ephemeral => "ephemeral",
        })
    }
}

/// The fixed lifecycle hook points.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Hooks {
    pub pre_sync: Option<String>,
    pub post_sync: Option<String>,
    pub pre_apply: Option<String>,
    pub post_apply: Option<String>,
    pub deps: Option<String>,
    pub pre_build: Option<String>,
    pub post_build: Option<String>,
    pub pre_verify: Option<String>,
    pub post_verify: Option<String>,
}

impl Hooks {
    pub fn get(&self, point: HookPoint) -> Option<&str> {
        let s = match point {
            HookPoint::PreSync => &self.pre_sync,
            HookPoint::PostSync => &self.post_sync,
            HookPoint::PreApply => &self.pre_apply,
            HookPoint::PostApply => &self.post_apply,
            HookPoint::Deps => &self.deps,
            HookPoint::PreBuild => &self.pre_build,
            HookPoint::PostBuild => &self.post_build,
            HookPoint::PreVerify => &self.pre_verify,
            HookPoint::PostVerify => &self.post_verify,
        };
        s.as_deref()
    }
}

/// A user-named hook, invocable by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ExtraHook {
    pub name: String,
    pub cmd: String,
    /// Working directory, relative to the sandbox. Defaults to the sandbox root.
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// When false, a non-zero exit warns instead of aborting.
    #[serde(default = "default_true")]
    pub required: bool,
}

const fn default_true() -> bool {
    true
}

impl ExtraHook {
    pub fn find<'a>(hooks: &'a [ExtraHook], name: &str) -> Option<&'a ExtraHook> {
        hooks.iter().find(|h| h.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_config_is_valid_and_inert() {
        let cfg = Config::parse("").unwrap();
        assert_eq!(cfg.hooks, Hooks::default());
        assert_eq!(cfg.sandbox.policy, SandboxPolicy::Persistent);
        assert!(cfg.configured_hooks().is_empty());
        assert!(cfg.upstream.url.is_none());
    }

    #[test]
    fn defaults_apply_when_sections_are_absent() {
        let cfg = Config::parse("[sandbox]\npolicy = \"ephemeral\"\n").unwrap();
        assert_eq!(cfg.sandbox.policy, SandboxPolicy::Ephemeral);
        assert!(cfg.sandbox.keep.is_empty());
    }

    #[test]
    fn a_sandbox_section_naming_only_keep_still_gets_the_default_policy() {
        // `[sandbox]` on its own is valid and means persistent, so a section carrying only
        // `keep` — the setting anyone actually writes — must not be rejected for leaving out
        // the setting they had no reason to care about.
        let cfg = Config::parse("[sandbox]\nkeep = [\"node_modules\"]\n").unwrap();
        assert_eq!(cfg.sandbox.policy, SandboxPolicy::Persistent);
        assert_eq!(cfg.sandbox.keep, vec!["node_modules".to_owned()]);
    }

    #[test]
    fn hooks_are_read_in_lifecycle_order() {
        let cfg = Config::parse(
            r#"
            [hooks]
            post-build = "b"
            pre-sync = "s"
            deps = "d"
            "#,
        )
        .unwrap();
        let keys: Vec<_> = cfg
            .configured_hooks()
            .into_iter()
            .map(|(p, _)| p.key())
            .collect();
        assert_eq!(keys, vec!["pre_sync", "deps", "post_build"]);
    }

    #[test]
    fn a_blank_hook_counts_as_unset() {
        // An empty string is what `skot init` leaves behind, and what a user
        // clearing a hook leaves behind. It must not run.
        let cfg = Config::parse("[hooks]\npre-build = \"   \"\n").unwrap();
        assert_eq!(cfg.hook(HookPoint::PreBuild), None);
    }

    #[test]
    fn unknown_keys_are_rejected_with_the_offending_name() {
        let err = Config::parse("[hooks]\npre_buidl = \"typo\"\n")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("pre_buidl"),
            "error should name the key: {err}"
        );
    }

    #[test]
    fn unknown_top_level_sections_are_rejected() {
        let err = Config::parse("[artifacts]\npath = \"/usr/local/bin\"\n")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("artifacts"),
            "error should name the section: {err}"
        );
    }

    #[test]
    fn a_bad_enum_value_lists_the_valid_ones() {
        let err = Config::parse("[sandbox]\npolicy = \"sometimes\"\n")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("persistent") && err.contains("ephemeral"),
            "got: {err}"
        );
    }

    #[test]
    fn extra_hooks_carry_cwd_env_and_optionality() {
        let cfg = Config::parse(
            r#"
            [[hook]]
            name = "lint"
            cmd = "cargo clippy"
            cwd = "crates/core"
            env = { RUSTFLAGS = "-D warnings" }
            required = false
            "#,
        )
        .unwrap();
        let h = ExtraHook::find(&cfg.extra, "lint").unwrap();
        assert_eq!(h.cwd.as_deref(), Some("crates/core"));
        assert_eq!(
            h.env.get("RUSTFLAGS").map(String::as_str),
            Some("-D warnings")
        );
        assert!(!h.required);
    }

    #[test]
    fn extra_hooks_default_to_required() {
        let cfg = Config::parse("[[hook]]\nname = \"t\"\ncmd = \"true\"\n").unwrap();
        assert!(cfg.extra[0].required);
        assert!(ExtraHook::find(&cfg.extra, "absent").is_none());
    }

    #[test]
    fn the_template_is_itself_a_valid_config() {
        // If this fails, `skot init` writes a file that cannot be loaded.
        Config::parse(&Config::template()).expect("template must parse");
    }

    #[test]
    fn the_template_documents_every_hook_point() {
        let template = Config::template();
        for point in HookPoint::ALL {
            assert!(
                template.contains(point.toml_key()),
                "template never mentions the {} hook",
                point.toml_key()
            );
        }
    }

    #[test]
    fn the_display_name_and_the_config_key_are_different_and_both_used() {
        // If these ever collapsed, `skot hooks` would display a key that could not be
        // pasted into skot.toml. `deps` is the exception: it has no prefix, so there
        // is nothing for kebab-case to change.
        for point in HookPoint::ALL {
            if point == HookPoint::Deps {
                assert_eq!(point.key(), point.toml_key());
                continue;
            }
            assert_ne!(point.key(), point.toml_key());
            assert!(!point.toml_key().contains('_'), "{}", point.toml_key());
            assert!(!point.key().contains('-'), "{}", point.key());
        }
    }
}
