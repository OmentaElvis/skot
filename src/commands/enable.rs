//! `patch enable`: put a patch back in the applying set.
//!
//! The series file is the record: `enable` uncomments the patch's line, so the choice
//! is trackable in git and lasts. The applied set is untouched; `skot patch apply`
//! (or a push) realizes the set in the sandbox.

use anyhow::{Context, Result, bail};
use log::{debug, info};
use serde_json::json;

use crate::cli::EnableArgs;
use crate::commands::authoring;
use crate::commands::{Ctx, Exit, emit_json};
use crate::hints::Hints;
use crate::invocation::{skot, skot_args};
use crate::layout::Layout;

/// What an enable did, presented by [`run`].
struct Changed {
    /// The patches whose lines were uncommented, in series order.
    names: Vec<String>,
    /// The enabled set after the change, in series order.
    enabled: Vec<String>,
    /// What is still disabled after the change.
    disabled: Vec<String>,
}

/// Uncomment patches' lines: enabled from now on, persistently.
///
/// Success is [`Exit::Success`] when anything changed. Nothing changing (an empty
/// selection, a patch already enabled) is [`Exit::Failure`], the house rule for a
/// command that did not do the thing asked for.
pub fn run(ctx: &Ctx, args: &EnableArgs) -> Result<Exit> {
    // The question is asked before the lock, the way apply's used to be: a prompt
    // sitting on the lock is one the user can walk away from mid-answer.
    let (_config, series) = ctx.layout.project()?;
    let enabled: Vec<String> = series.names().into_iter().map(String::from).collect();
    let disabled = series.disabled(&ctx.layout);

    let wanted: Vec<String> = if args.all {
        if disabled.is_empty() {
            info!(target: "enable", "every patch is already enabled; nothing changed");
            return Ok(Exit::Failure);
        }
        disabled.clone()
    } else if !args.patches.is_empty() {
        let names: Vec<&str> = enabled.iter().map(String::as_str).collect();
        let mut universe: Vec<&str> = names.clone();
        universe.extend(disabled.iter().map(String::as_str));
        let mut targets = Vec::new();
        for want in &args.patches {
            if !enabled.iter().any(|n| n == want)
                && !disabled.iter().any(|n| n == want)
                && series.has_disabled_line(want)
            {
                bail!(
                    "`{want}` is disabled, but its file is gone, so there is nothing \
                     to put back.\n`{}` takes the line out.",
                    skot_args("drop", want)
                );
            }
            targets.push(authoring::resolve_patch(&universe, &names, want)?.name(&names));
        }
        targets
    } else if !ctx.cli.json && crate::ui::stdin_is_terminal() {
        pick(&disabled)?
    } else {
        bail!(
            "cannot ask which patch to enable here.\nName it: `{}`.",
            skot_args("enable", "<patch>")
        )
    };
    if wanted.is_empty() {
        info!(target: "enable", "nothing selected; nothing changed");
        return Ok(Exit::Failure);
    }

    // Already-enabled names are a no-op, not a failure of the rest: a batch enables
    // what is left and says so.
    let mut todo: Vec<String> = Vec::new();
    let mut already: Vec<String> = Vec::new();
    for name in wanted {
        if enabled.contains(&name) {
            if !already.contains(&name) {
                already.push(name);
            }
        } else if !todo.contains(&name) {
            todo.push(name);
        }
    }
    if todo.is_empty() {
        info!(
            target: "enable",
            "{} already enabled; nothing changed",
            already.join(", ")
        );
        return Ok(Exit::Failure);
    }

    let changed = enable(&ctx.layout, &todo)?;
    for name in &already {
        debug!(target: "enable", "  {name} was already enabled; left alone");
    }

    if ctx.cli.json {
        emit_json(&json!({
            "ok": true,
            "enabled_now": changed.names,
            "enabled": changed.enabled,
            "disabled": changed.disabled,
        }))?;
        return Ok(Exit::Success);
    }

    info!(
        target: "enable",
        "enabled {}",
        super::push::count(changed.names.len(), "patch", "patches")
    );
    for name in &changed.names {
        debug!(target: "enable", "  {name}");
    }
    Hints::new("next")
        .entry(skot("apply"), "realize the set in the sandbox")
        .entry(skot("list"), "the series as it stands")
        .print();
    Ok(Exit::Success)
}

/// Ask an interactive session which patches to enable. Only disabled ones are offered:
/// an enabled patch would only produce the no-op.
fn pick(disabled: &[String]) -> Result<Vec<String>> {
    if disabled.is_empty() {
        bail!(
            "every patch in the series is enabled.\n`{}` skips one.",
            skot("disable")
        );
    }
    inquire::MultiSelect::new("Which patches to enable", disabled.to_vec())
        .with_help_message("Space toggles; Enter enables the selection. Esc cancels.")
        .prompt()
        .context("asking which patches to enable")
}

/// The plain work: lock, uncomment, record.
fn enable(layout: &Layout, wanted: &[String]) -> Result<Changed> {
    let lock = layout.lock()?;
    let (_config, mut series) = layout.project()?;

    let mut names = Vec::new();
    for want in wanted {
        if !series.enable(layout, want) {
            bail!(
                "`{want}` could not be enabled: its line or its file changed underfoot.\n`{}` \
                 lists what is disabled.",
                skot("list")
            );
        }
        names.push(want.clone());
    }
    series.save(layout)?;
    let enabled = series.names().into_iter().map(String::from).collect();
    let disabled = series.disabled(layout);
    drop(lock);

    Ok(Changed {
        names,
        enabled,
        disabled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use crate::commands::testsupport;
    use clap::Parser as _;

    fn two_enabled_one_disabled() -> (tempfile::TempDir, Layout) {
        let (tmp, layout) = testsupport::project();
        for name in ["0001-a.patch", "0002-b.patch", "0003-c.patch"] {
            std::fs::write(layout.patches_dir().join(name), "x\n").unwrap();
        }
        std::fs::write(
            layout.series_file(),
            "0001-a.patch\n# 0002-b.patch\n0003-c.patch\n",
        )
        .unwrap();
        (tmp, layout)
    }

    #[test]
    fn enabling_uncomments_the_line_and_the_set_follows() {
        let (tmp, layout) = two_enabled_one_disabled();
        let cli = Cli::try_parse_from(["skot", "patch", "enable", "0002-b.patch"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let exit = run(
            &ctx,
            &EnableArgs {
                patches: vec!["0002-b.patch".to_owned()],
                all: false,
            },
        )
        .unwrap();

        assert_eq!(exit, Exit::Success);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("series")).unwrap(),
            "0001-a.patch\n0002-b.patch\n0003-c.patch\n"
        );
    }

    #[test]
    fn enabling_an_already_enabled_patch_is_a_no_op() {
        let (tmp, layout) = two_enabled_one_disabled();
        let cli = Cli::try_parse_from(["skot", "patch", "enable", "0001-a.patch"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let exit = run(
            &ctx,
            &EnableArgs {
                patches: vec!["0001-a.patch".to_owned()],
                all: false,
            },
        )
        .unwrap();

        assert_eq!(exit, Exit::Failure);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("series")).unwrap(),
            "0001-a.patch\n# 0002-b.patch\n0003-c.patch\n",
            "the file is untouched"
        );
    }

    #[test]
    fn several_patches_can_be_enabled_at_once_by_prefix() {
        let (tmp, layout) = two_enabled_one_disabled();
        // Two disabled this time, so the batch has something to do for both names.
        std::fs::write(
            layout.series_file(),
            "0001-a.patch\n# 0002-b.patch\n# 0003-c.patch\n",
        )
        .unwrap();
        let cli = Cli::try_parse_from(["skot", "patch", "enable", "0002", "0003"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let exit = run(
            &ctx,
            &EnableArgs {
                patches: vec!["0002".to_owned(), "0003".to_owned()],
                all: false,
            },
        )
        .unwrap();

        assert_eq!(exit, Exit::Success);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("series")).unwrap(),
            "0001-a.patch\n0002-b.patch\n0003-c.patch\n"
        );
    }

    #[test]
    fn all_takes_every_disabled_patch() {
        let (tmp, layout) = two_enabled_one_disabled();
        std::fs::write(
            layout.series_file(),
            "0001-a.patch\n# 0002-b.patch\n# 0003-c.patch\n",
        )
        .unwrap();
        let cli = Cli::try_parse_from(["skot", "patch", "enable", "--all"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let exit = run(
            &ctx,
            &EnableArgs {
                patches: Vec::new(),
                all: true,
            },
        )
        .unwrap();

        assert_eq!(exit, Exit::Success);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("series")).unwrap(),
            "0001-a.patch\n0002-b.patch\n0003-c.patch\n"
        );
    }

    #[test]
    fn all_with_nothing_disabled_is_a_no_op() {
        let (tmp, layout) = two_enabled_one_disabled();
        std::fs::write(
            layout.series_file(),
            "0001-a.patch\n0002-b.patch\n0003-c.patch\n",
        )
        .unwrap();
        let cli = Cli::try_parse_from(["skot", "patch", "enable", "--all"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let exit = run(
            &ctx,
            &EnableArgs {
                patches: Vec::new(),
                all: true,
            },
        )
        .unwrap();

        assert_eq!(exit, Exit::Failure);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("series")).unwrap(),
            "0001-a.patch\n0002-b.patch\n0003-c.patch\n"
        );
    }

    #[test]
    fn a_batch_with_one_already_enabled_enables_the_rest() {
        let (tmp, layout) = two_enabled_one_disabled();
        std::fs::write(
            layout.series_file(),
            "0001-a.patch\n# 0002-b.patch\n# 0003-c.patch\n",
        )
        .unwrap();
        let cli = Cli::try_parse_from(["skot", "patch", "enable", "0001-a.patch", "0002"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let exit = run(
            &ctx,
            &EnableArgs {
                patches: vec!["0001-a.patch".to_owned(), "0002".to_owned()],
                all: false,
            },
        )
        .unwrap();

        assert_eq!(exit, Exit::Success, "the patch that could go, went");
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("series")).unwrap(),
            "0001-a.patch\n0002-b.patch\n# 0003-c.patch\n"
        );
    }

    #[test]
    fn a_script_that_names_nothing_is_told_how() {
        let (tmp, layout) = two_enabled_one_disabled();
        let cli = Cli::try_parse_from(["skot", "patch", "enable"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let err = run(
            &ctx,
            &EnableArgs {
                patches: Vec::new(),
                all: false,
            },
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("cannot ask"), "{err}");
        assert!(err.contains(&skot_args("enable", "<patch>")), "{err}");
        drop(tmp);
    }

    #[test]
    fn an_unknown_patch_names_what_is_disabled() {
        let (tmp, layout) = two_enabled_one_disabled();
        let cli = Cli::try_parse_from(["skot", "patch", "enable", "nope.patch"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        let err = run(
            &ctx,
            &EnableArgs {
                patches: vec!["nope.patch".to_owned()],
                all: false,
            },
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("is not in the series"), "{err}");
        assert!(err.contains("0002-b.patch"), "{err}");
        drop(tmp);
    }

    #[test]
    fn json_is_one_document_with_the_sets_after() {
        let (tmp, layout) = two_enabled_one_disabled();
        let cli =
            Cli::try_parse_from(["skot", "patch", "enable", "0002-b.patch", "--json"]).unwrap();
        let ctx = Ctx {
            layout,
            cli,
            yes: false,
        };

        run(
            &ctx,
            &EnableArgs {
                patches: vec!["0002-b.patch".to_owned()],
                all: false,
            },
        )
        .unwrap();

        // The document itself is stdout's business, checked end to end in tests/cli.rs;
        // here the work is just that the sets after the change are what save wrote.
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("series")).unwrap(),
            "0001-a.patch\n0002-b.patch\n0003-c.patch\n"
        );
    }
}
