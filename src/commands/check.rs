use anyhow::{Context, Result};
use log::error;

use crate::cli::CheckArgs;
use crate::commands::{Ctx, Exit, authoring, emit_json};
use crate::git;
use crate::hints::Hints;
use crate::report;
use crate::state::State;

/// Check the series (or with `--applied`, the recorded set) against a ref, or against
/// the sandbox when there is no ref.
///
/// Both cases run the same way: apply the entries to a commit's tree with libgit2, in
/// the mirror's object database. Nothing is checked out, so this is safe against a ref
/// the sandbox has never been on. `--applied` narrows the replay to the set `build`
/// would land, which is what makes it the question to ask after toggling patches:
/// the full series is the default because "the whole thing still applies" is the
/// strongest statement, but a set with holes in it has its own answer.
pub fn run(ctx: &Ctx, args: &CheckArgs) -> Result<Exit> {
    let (config, series) = ctx.project()?;
    let all = series.require_resolvable(&ctx.layout)?;
    let entries = if args.applied {
        let state = State::load(&ctx.layout)?;
        authoring::recorded_stack(&ctx.layout, &series, &state)?.entries
    } else {
        all
    };
    let set = if args.applied { "applied" } else { "series" };

    let wanted = args
        .ref_name
        .clone()
        .or_else(|| config.upstream.default_ref.clone())
        .or_else(|| git::sandbox::current_ref(&ctx.layout));

    let report = match wanted {
        Some(name) => {
            // The same choke point as `apply`: make sure the mirror can serve this ref
            // (fetching only if it cannot), then read it. `check` does not need a
            // sandbox (the entries are applied to the commit's tree in the mirror), so a
            // ref the mirror lacks is fetched, not deferred to a `skot sync` round trip.
            let (mirror, sha) = super::sync::resolve_in_mirror(&ctx.layout, &name)?;
            crate::check::check_series(mirror.repo(), &sha, &entries).context(format!(
                "checking {} against {name} ({sha})",
                if args.applied {
                    "the applied set"
                } else {
                    "the series"
                }
            ))?
        }
        None => {
            if !git::sandbox::is_repository(&ctx.layout) {
                anyhow::bail!(
                    "nothing to check against.\n\
                     No ref was given, `upstream.default-ref` is unset, and there is no \
                     sandbox yet.\n\
                     Pass one (`skot check main`), set `upstream.default-ref` in {}, or \
                     run `skot build` first.",
                    ctx.layout.config_file().display()
                );
            }
            let repo = git::sandbox::open(&ctx.layout)?;
            let sha = git::sandbox::head_sha(&repo).context(format!(
                "reading the sandbox HEAD at {}",
                ctx.layout.sandbox_dir().display()
            ))?;
            crate::check::check_series(&repo, &sha, &entries)?
        }
    };

    if ctx.cli.json {
        let mut doc = serde_json::to_value(&report)?;
        if let Some(object) = doc.as_object_mut() {
            object.insert("set".to_owned(), serde_json::json!(set));
        }
        emit_json(&doc)?;
        return Ok(if report.ok {
            Exit::Success
        } else {
            Exit::Failure
        });
    }

    report::section("check");
    report::field("against", &report.base);
    if args.applied {
        report::field("set", "the applied set (what `build` lands)");
    }

    // Stopping at the first failure is the default, because the usual reason to run
    // `check` is that one patch broke and fixing it is the next task. `--all` is for
    // triage after a rebase, where you want the whole damage at once.
    if report.ok {
        for patch in &report.patches {
            print_patch(patch);
        }
        if report.patch_count == 0 {
            report::line(&report::ok("no patches; nothing to check"));
        } else {
            report::line(&report::ok(&format!(
                "all {} apply",
                count(report.patch_count, "patch", "patches")
            )));
        }
        Hints::new("next")
            .entry("skot build", "the whole run: fetch, apply, build")
            .print();
        return Ok(Exit::Success);
    }

    let failed: Vec<_> = report.patches.iter().filter(|p| !p.ok).collect();

    if args.all {
        for patch in &report.patches {
            print_patch(patch);
        }
        let total = count(report.patch_count, "patch", "patches");
        let subject = count(failed.len(), "patch does", "patches do");
        error!(target: "check", "{subject} not apply: {} of {total}", failed.len());
        return Ok(Exit::Failure);
    }

    for patch in &failed {
        print_patch(patch);
    }
    let first = &failed[0];
    error!(target: "check", "`{}` does not apply here.", first.name);
    if failed.len() > 1 {
        report::note(&format!(
            "{} later {} also failed. Use --all to see them.",
            failed.len() - 1,
            if failed.len() == 2 {
                "patch"
            } else {
                "patches"
            }
        ));
    }
    report::note("Rebase it against this ref, then `skot refresh` to re-export it.");
    Ok(Exit::Failure)
}

/// A count with a verb that agrees with it.
///
/// `"1 of 2 patches do not apply"` is the kind of wrongness a user reads past once and
/// then stops trusting the rest of the output.
fn count(n: usize, singular: &str, plural: &str) -> String {
    format!("{n} {}", if n == 1 { singular } else { plural })
}

/// Print one patch's result as part of the report on stdout.
///
/// Under `--json` this never runs: the document *is* the report there, so the two renderings
/// of one result must not both reach the wire.
fn print_patch(patch: &crate::check::PatchReport) {
    if patch.ok {
        report::line(&format!("  {}  {}", report::ok("ok  "), patch.name));
    } else {
        report::line(&format!("  {}  {}", report::bad("FAIL"), patch.name));
    }
    if let Some(err) = &patch.error {
        report::line(&format!("      {}", report::bad(err)));
    }
    for file in patch.files.iter().filter(|f| !f.ok) {
        if let Some(detail) = &file.detail {
            for line in detail.lines() {
                report::line(&format!("        {}", report::muted(line)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use crate::commands::testsupport;
    use crate::layout::Layout;
    use clap::Parser as _;

    fn args(applied: bool) -> CheckArgs {
        CheckArgs {
            ref_name: None,
            applied,
            all: false,
            first_only: false,
        }
    }

    fn ctx_for(layout: &Layout) -> Ctx {
        let cli = Cli::try_parse_from(["skot", "check"]).unwrap();
        Ctx {
            layout: layout.clone(),
            cli,
            yes: false,
        }
    }

    #[test]
    fn a_count_and_its_verb_agree() {
        assert_eq!(count(1, "patch", "patches"), "1 patch");
        assert_eq!(count(2, "patch", "patches"), "2 patches");
        assert_eq!(count(0, "patch", "patches"), "0 patches", "zero is plural");
    }

    #[test]
    fn applied_narrows_the_replay_to_what_build_would_land() {
        // The second patch needs context only the first one creates, so the whole series
        // fails on this base while the applied set, which holds only the first, lands.
        let tmp = tempfile::tempdir().unwrap();
        let layout = testsupport::landed_set(
            tmp.path(),
            &[
                ("0001-a.patch", testsupport::patch_adds_g()),
                ("0002-b.patch", testsupport::patch_two()),
            ],
            &["0001-a.patch"],
        )
        .0;

        let full = run(&ctx_for(&layout), &args(false)).unwrap();
        assert_eq!(full, Exit::Failure, "the whole series does not land");

        let applied = run(&ctx_for(&layout), &args(true)).unwrap();
        assert_eq!(applied, Exit::Success, "the staged set does");
    }

    #[test]
    fn an_empty_applied_set_checks_nothing_and_succeeds() {
        // Nothing staged is a legal state (a fresh project, or everything popped), and
        // the answer is vacuously yes rather than an error.
        let tmp = tempfile::tempdir().unwrap();
        let layout = testsupport::landed_set(
            tmp.path(),
            &[("0001-a.patch", testsupport::patch_one())],
            &[],
        )
        .0;

        let exit = run(&ctx_for(&layout), &args(true)).unwrap();

        assert_eq!(exit, Exit::Success);
    }

    #[test]
    fn a_recorded_name_the_series_no_longer_lists_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = testsupport::landed_set(
            tmp.path(),
            &[
                ("0001-a.patch", testsupport::patch_adds_g()),
                ("0002-b.patch", testsupport::patch_two()),
            ],
            &["0001-a.patch"],
        )
        .0;
        // Hand-edit the series out from under the recorded set, the drift `validate_set`
        // exists to catch.
        std::fs::write(layout.series_file(), "0002-b.patch\n").unwrap();

        let err = run(&ctx_for(&layout), &args(true)).unwrap_err().to_string();

        assert!(err.contains("does not list"), "{err}");
        assert!(err.contains("unapply"), "{err}");
    }
}
