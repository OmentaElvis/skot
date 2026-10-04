use anyhow::{Context, Result};
use log::error;

use crate::cli::CheckArgs;
use crate::commands::{Ctx, Exit, emit_json};
use crate::git;
use crate::hints::Hints;
use crate::report;

/// Check the series against a ref, or against the sandbox when there is no ref.
///
/// Both cases run the same way: apply the series to a commit's tree with libgit2, in the
/// mirror's object database. Nothing is checked out, so this is safe against a ref the
/// sandbox has never been on.
pub fn run(ctx: &Ctx, args: &CheckArgs) -> Result<Exit> {
    let (config, series) = ctx.project()?;
    let entries = series.require_resolvable(&ctx.layout)?;

    let wanted = args
        .ref_name
        .clone()
        .or_else(|| config.upstream.default_ref.clone())
        .or_else(|| git::sandbox::current_ref(&ctx.layout));

    let report = match wanted {
        Some(name) => {
            let mirror = git::mirror::Mirror::open(&ctx.layout).ok();
            let Some(mirror) = mirror else {
                anyhow::bail!(
                    "no mirror at {}, so `{name}` cannot be resolved.\n\
                     Run `skot sync` to fetch it.",
                    ctx.layout.mirror_dir().display()
                );
            };
            let sha = mirror
                .resolve(&name)
                .context(format!("cannot resolve `{name}` against the mirror"))?;
            crate::check::check_series(mirror.repo(), &sha, &entries)
                .context(format!("checking the series against {name} ({sha})"))?
        }
        None => {
            if !git::sandbox::is_repository(&ctx.layout) {
                anyhow::bail!(
                    "nothing to check against.\n\
                     There is no mirror at {} and no sandbox yet.\n\
                     Run `skot sync <ref>` to create one.",
                    ctx.layout.mirror_dir().display()
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
        emit_json(&report)?;
        return Ok(if report.ok {
            Exit::Success
        } else {
            Exit::Failure
        });
    }

    report::section("check");
    report::field("against", &report.base);

    // Stopping at the first failure is the default, because the usual reason to run
    // `check` is that one patch broke and fixing it is the next task. `--all` is for
    // triage after a rebase, where you want the whole damage at once.
    if report.ok {
        for patch in &report.patches {
            print_patch(patch);
        }
        report::line(&report::ok(&format!(
            "all {} apply",
            count(report.patch_count, "patch", "patches")
        )));
        Hints::new("next")
            .entry("skot build", "the whole run: fetch, apply, build, verify")
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
        error!("{subject} not apply: {} of {total}", failed.len());
        return Ok(Exit::Failure);
    }

    for patch in &failed {
        print_patch(patch);
    }
    let first = &failed[0];
    error!("`{}` does not apply here.", first.name);
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

    #[test]
    fn a_count_and_its_verb_agree() {
        assert_eq!(count(1, "patch", "patches"), "1 patch");
        assert_eq!(count(2, "patch", "patches"), "2 patches");
        assert_eq!(count(0, "patch", "patches"), "0 patches", "zero is plural");
    }
}
