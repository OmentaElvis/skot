//! `skot patch copy <patch> --into <series>`, or `--from <series>` the other way.
//!
//! A patch belongs to the series that holds its file, and two series can hold patches
//! of the same name. Copying takes the file as it stands in the source series and
//! gives the target series a line for it, at the end or wherever `--at` puts it. What
//! it does on the target's base is a question for `check`, which is exactly why the
//! copy does not pretend to answer it.

use anyhow::{Context, Result, bail};
use log::{debug, info};

use crate::cli::PatchCopyArgs;
use crate::commands::{Ctx, Exit, emit_json};
use crate::series::Series;

pub fn run(ctx: &Ctx, args: &PatchCopyArgs) -> Result<Exit> {
    // The direction comes from the flag that was used: `--from` brings a patch into
    // the active series, `--into` takes one out to another. clap requires exactly one.
    let (source, target, source_name, target_name) =
        match (args.from.as_deref(), args.into.as_deref()) {
            (Some(from), None) => (
                ctx.layout.with_series(from)?,
                ctx.layout.clone(),
                from.to_owned(),
                ctx.layout.series().to_owned(),
            ),
            (None, Some(into)) => (
                ctx.layout.clone(),
                ctx.layout.with_series(into)?,
                ctx.layout.series().to_owned(),
                into.to_owned(),
            ),
            _ => bail!("copy takes --from or --into, not both and not neither"),
        };

    if !source.patches_dir().is_dir() {
        bail!(
            "there is no series `{source_name}`.\n`{}` lists what exists.",
            crate::invocation::skot("series")
        );
    }
    let source_series = Series::load(&source.series_file())
        .context(format!("reading {}", source.series_file().display()))?;

    // Resolved in the *source*: the name is as that series calls it, prefix and all.
    let entries = source_series
        .entries(&source)
        .context(format!("reading the patches of `{source_name}`"))?;
    let matches: Vec<_> = entries
        .iter()
        .filter(|entry| entry.name.starts_with(&args.patch))
        .collect();
    let entry = match matches.as_slice() {
        [entry] => *entry,
        [] => bail!(
            "the series `{source_name}` has no patch starting with `{}`.\n\
             `{}` lists its patches.",
            args.patch,
            crate::invocation::skot_args("patch", "list")
        ),
        many => {
            let listed = many
                .iter()
                .map(|entry| format!("`{}`", entry.name))
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "`{}` matches several patches in `{source_name}`: {listed}.\n\
                 Name one of them in full.",
                args.patch
            );
        }
    };

    // A duplicate name is refused rather than silently overwritten: two files with one
    // name is a series that cannot be read.
    let mut target_series = Series::load(&target.series_file())
        .context(format!("reading {}", target.series_file().display()))?;
    let target_file = target.patches_dir().join(&entry.name);
    if target_file.is_file() {
        bail!(
            "{target_name} already holds a patch called `{}`.\n\
             `{}` renames one of them out of the way, `{}` takes the file for good.",
            entry.name,
            crate::invocation::skot_args("move", "<patch>"),
            crate::invocation::skot_args("drop", "<patch>")
        );
    }

    std::fs::copy(&entry.path, &target_file)
        .context(format!("writing {}", target_file.display()))?;
    // Appends by default; `--at` is a position in the series, counting from 1 the way
    // `new --at` counts, so the two agree.
    let index = args.at.map(|at| at.saturating_sub(1)).unwrap_or(usize::MAX);
    target_series
        .insert(&entry.name, index)
        .context(format!("adding `{}` to the series", entry.name))?;
    target_series
        .save(&target)
        .context(format!("writing {}", target.series_file().display()))?;

    debug!(
        target: "copy",
        "copied `{}` from `{source_name}` into `{target_name}`",
        entry.name
    );
    if ctx.cli.json {
        emit_json(&serde_json::json!({
            "series_name": target_name,
            "from": source_name,
            "patch": entry.name,
        }))?;
    } else {
        info!(
            target: "copy",
            "copied {} from `{source_name}` into `{target_name}`",
            entry.name
        );
        info!(
            target: "copy",
            "`{}` says what it does on this series' base.",
            crate::invocation::skot("check")
        );
    }
    Ok(Exit::Success)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use crate::commands::testsupport::{project, write_series};
    use crate::commands::{Ctx, Exit};
    use crate::layout::Layout;
    use clap::Parser as _;

    fn ctx_for(layout: &Layout) -> Ctx {
        Ctx {
            layout: layout.clone(),
            cli: Cli::try_parse_from(["skot", "patch", "copy", "x", "--from", "exp"]).unwrap(),
            yes: false,
        }
    }

    fn copy(ctx: &Ctx, patch: &str, from: &str) -> Result<Exit> {
        run(
            ctx,
            &PatchCopyArgs {
                patch: patch.to_owned(),
                from: Some(from.to_owned()),
                into: None,
                at: None,
            },
        )
    }

    fn copy_into(ctx: &Ctx, patch: &str, into: &str) -> Result<Exit> {
        run(
            ctx,
            &PatchCopyArgs {
                patch: patch.to_owned(),
                from: None,
                into: Some(into.to_owned()),
                at: None,
            },
        )
    }

    #[test]
    fn a_patch_is_copied_from_the_named_series_and_appended_here() {
        let (_tmp, layout) = project();
        std::fs::create_dir_all(layout.series_root().join("exp")).unwrap();
        write_series(
            &layout.with_series("exp").unwrap(),
            &[("a.patch", "a\n"), ("b.patch", "b\n"), ("c.patch", "c\n")],
        );
        write_series(&layout, &[("a.patch", "a\n")]);

        // A prefix, resolved in the source series the way every patch command resolves.
        let exit = copy(&ctx_for(&layout), "c", "exp").unwrap();

        assert!(matches!(exit, Exit::Success));
        assert_eq!(
            std::fs::read_to_string(layout.patches_dir().join("c.patch")).unwrap(),
            "c\n"
        );
        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "a.patch\nc.patch\n",
            "the copy lands at the end of the series"
        );
    }

    #[test]
    fn into_copies_the_other_way_from_here_to_the_named_series() {
        let (_tmp, layout) = project();
        std::fs::create_dir_all(layout.series_root().join("exp")).unwrap();
        write_series(&layout, &[("a.patch", "a\n"), ("b.patch", "b\n")]);
        write_series(&layout.with_series("exp").unwrap(), &[("c.patch", "c\n")]);

        let exit = copy_into(&ctx_for(&layout), "b", "exp").unwrap();

        assert!(matches!(exit, Exit::Success));
        assert_eq!(
            std::fs::read_to_string(
                layout
                    .with_series("exp")
                    .unwrap()
                    .patches_dir()
                    .join("b.patch")
            )
            .unwrap(),
            "b\n"
        );
        assert_eq!(
            std::fs::read_to_string(layout.with_series("exp").unwrap().series_file()).unwrap(),
            "c.patch\nb.patch\n",
            "the source series is not touched"
        );
        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "a.patch\nb.patch\n",
        );
    }

    #[test]
    fn a_name_the_target_already_holds_is_refused_not_overwritten() {
        let (_tmp, layout) = project();
        std::fs::create_dir_all(layout.series_root().join("exp")).unwrap();
        write_series(&layout, &[("a.patch", "mine\n")]);
        write_series(
            &layout.with_series("exp").unwrap(),
            &[("a.patch", "theirs\n")],
        );

        let err = copy(&ctx_for(&layout), "a.patch", "exp").unwrap_err();

        let text = format!("{err:#}");
        assert!(text.contains("already holds"), "{text}");
        assert_eq!(
            std::fs::read_to_string(layout.patches_dir().join("a.patch")).unwrap(),
            "mine\n",
            "the existing file is untouched"
        );
    }

    #[test]
    fn an_ambiguous_prefix_lists_what_it_matched() {
        let (_tmp, layout) = project();
        std::fs::create_dir_all(layout.series_root().join("exp")).unwrap();
        write_series(
            &layout.with_series("exp").unwrap(),
            &[("a.patch", "a\n"), ("ab.patch", "b\n")],
        );

        let err = copy(&ctx_for(&layout), "a", "exp").unwrap_err();

        let text = format!("{err:#}");
        assert!(text.contains("several patches"), "{text}");
        assert!(
            text.contains("a.patch") && text.contains("ab.patch"),
            "{text}"
        );
    }

    #[test]
    fn exactly_one_direction_is_required() {
        assert!(Cli::try_parse_from(["skot", "patch", "copy", "x", "--into", "exp"]).is_ok());
        assert!(Cli::try_parse_from(["skot", "patch", "copy", "x", "--from", "exp"]).is_ok());
        assert!(
            Cli::try_parse_from(["skot", "patch", "copy", "x", "--from", "a", "--into", "b"])
                .is_err(),
            "--from and --into together is not a direction"
        );
        assert!(
            Cli::try_parse_from(["skot", "patch", "copy", "x"]).is_err(),
            "a copy without a direction is refused"
        );
    }
}
