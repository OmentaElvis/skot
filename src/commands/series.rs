//! `skot series`: the list, and creating, switching and copying series.
//!
//! A series is a directory under `patches/`, so creating one is a directory and two
//! files, and switching is a pointer in `.skot/state.json`. What makes a switch more
//! than a pointer is the shared sandbox: `sandbox.on-switch` decides whether it follows
//! the switch now or waits for the next `build` or `apply`, and that is the only git
//! work this module does.

use std::fs;

use anyhow::{Context, Result, bail};
use log::{debug, error, info, warn};

use crate::cli::{
    SeriesArgs, SeriesCommand, SeriesCopyArgs, SeriesCreateArgs, SeriesDropArgs, SeriesRenameArgs,
    SeriesSwitchArgs, SeriesUndoArgs,
};
use crate::commands::apply::Applied;
use crate::commands::{Ctx, Exit, apply, emit_json};
use crate::config::OnSwitch;
use crate::hints::Hints;
use crate::invocation::{skot, skot_args};
use crate::layout::Layout;
use crate::prompt::Fallback;
use crate::report;
use crate::series::Series;
use crate::seriesdir;
use crate::session;
use crate::state::State;

pub fn run(ctx: &Ctx, args: &SeriesArgs) -> Result<Exit> {
    match &args.command {
        None => list(ctx),
        Some(SeriesCommand::Create(args)) => create(ctx, args),
        Some(SeriesCommand::Switch(args)) => switch(ctx, args),
        Some(SeriesCommand::Copy(args)) => copy(ctx, args),
        Some(SeriesCommand::Undo(args)) => undo(ctx, args),
        Some(SeriesCommand::Drop(args)) => drop_series(ctx, args),
        Some(SeriesCommand::Rename(args)) => rename(ctx, args),
    }
}

/// One row of the series list: its name, and how much of a series it holds.
struct Row {
    name: String,
    patches: usize,
    disabled: usize,
}

/// `skot series`.
fn list(ctx: &Ctx) -> Result<Exit> {
    // What marks the active row: the pointer, or the project default. Not the name
    // this run resolved, which under `--series` is a one-shot override and under a
    // lost pointer was no answer at all.
    let active: Option<String> = State::load(&ctx.layout)
        .ok()
        .and_then(|state| state.active_series().map(str::to_owned))
        .or_else(|| ctx.layout.config().ok().and_then(|config| config.series));

    let mut rows = Vec::new();
    for name in ctx.layout.series_names() {
        // The name came out of a directory listing, so it is a valid series name; the
        // layout for *that* series is what resolves its patch files correctly.
        let layout = ctx.layout.with_series(&name)?;
        let (patches, disabled) = match Series::load(&layout.series_file()) {
            Ok(series) => (series.len(), series.disabled().len()),
            Err(_) => (0, 0),
        };
        rows.push(Row {
            name,
            patches,
            disabled,
        });
    }

    if ctx.cli.json {
        emit_json(&serde_json::json!({
            "series": rows
                .iter()
                .map(|row| serde_json::json!({
                    "name": row.name,
                    "active": Some(row.name.as_str()) == active.as_deref(),
                    "patches": row.patches,
                    "disabled": row.disabled,
                }))
                .collect::<Vec<_>>(),
            "active": active,
        }))?;
        return Ok(Exit::Success);
    }

    if rows.is_empty() {
        report::note(&format!(
            "no series under {}; {} creates one.",
            ctx.layout.series_root().display(),
            skot_args("series", "create <name>")
        ));
        return Ok(Exit::Success);
    }

    report::section("series");
    for row in &rows {
        let mut summary = apply::count(row.patches, "patch", "patches");
        if row.disabled > 0 {
            summary.push_str(&format!(", {} disabled", row.disabled));
        }
        if Some(row.name.as_str()) == active.as_deref() {
            report::line(&format!("* {}  {}", report::ok(&row.name), summary));
        } else {
            report::line(&format!("  {}  {}", row.name, summary));
        }
    }
    Ok(Exit::Success)
}

/// `skot series create <name>`: create the series, then switch to it.
fn create(ctx: &Ctx, args: &SeriesCreateArgs) -> Result<Exit> {
    let target = ctx.layout.with_series(&args.name)?;
    if target.patches_dir().exists() {
        bail!(
            "the series `{}` already exists.\n`{}` switches to it.",
            args.name,
            skot_args("series", &format!("switch {}", args.name))
        );
    }
    refuse_case_clash(&ctx.layout, &args.name)?;
    // Validated before the first write: a refusal must not leave an empty series behind.
    let source = match &args.from {
        Some(from) => {
            if from == &args.name {
                bail!("a series cannot be seeded from itself.");
            }
            let source = ctx.layout.with_series(from)?;
            if !source.patches_dir().is_dir() {
                bail!(
                    "there is no series `{}`.\n`{}` lists what exists.",
                    from,
                    skot("series")
                );
            }
            Some(source)
        }
        None => None,
    };

    // Held across the switch: the rebuild below mutates the sandbox.
    let lock = ctx.lock()?;

    match source {
        Some(source) => {
            copy_series_files(&source, &target)?;
        }
        None => {
            // Written straight to the live path: two files is not a swap, and the
            // directory is either new or this would already have been refused.
            fs::create_dir_all(target.patches_dir())
                .context(format!("creating {}", target.patches_dir().display()))?;
            fs::write(
                target.series_file(),
                "# The patches to apply, in order. One filename per line.\n",
            )
            .context(format!("writing {}", target.series_file().display()))?;
            // The new series starts where the one you are leaving sits, so two series
            // split an experiment rather than two different bases. No base recorded yet
            // means the configured default decides, exactly as it did before.
            if ctx.layout.base_file().is_file() {
                fs::copy(ctx.layout.base_file(), target.base_file())
                    .context(format!("writing {}", target.base_file().display()))?;
            }
        }
    }

    let mut state = State::load(&ctx.layout)?;
    state.set_active_series(&ctx.layout, &args.name)?;
    debug!(target: "series", "switched the pointer to `{}`", args.name);
    let rebuild = after_switch(&target)?;
    drop(lock);

    let doc = serde_json::json!({
        "series_name": args.name,
        "created": true,
        "active": true,
        "from": args.from,
        "sandbox": rebuild.status,
        "details": rebuild.details(),
    });
    if ctx.cli.json {
        emit_json(&doc)?;
    } else {
        info!(
            target: "series",
            "created and switched to `{}`",
            args.name
        );
        rebuild.narrate(&args.name);
        Hints::new("next")
            .entry(skot_args("new", "<name>"), "start a patch on this series")
            .entry(skot("build"), "run your hooks from here")
            .print();
    }
    Ok(rebuild.exit())
}

/// Refuse a name that differs from an existing series only in case.
///
/// Two series `Exp` and `exp` are two directories on a case-insensitive checkout and
/// one on a case-sensitive one, so the same commands would mean different things
/// depending on where the project lives. Caught at the boundary, once.
pub(crate) fn refuse_case_clash(layout: &Layout, name: &str) -> Result<()> {
    if let Some(clash) = layout
        .series_names()
        .into_iter()
        .find(|other| other != name && other.eq_ignore_ascii_case(name))
    {
        bail!(
            "the series `{name}` differs only in case from `{clash}`.\n\
             skot refuses both spellings: on some file systems they are one directory, \
             and the same command would mean different things depending on where the \
             project lives.\n\
             Pick a name that differs by more than case."
        );
    }
    Ok(())
}

/// Build a copy of one series in scratch and rename it into place under a new name.
///
/// The list, the base and every patch: a copy is a snapshot of the series as it stands,
/// so anything the series holds goes with it. Built and checked in `.skot/tmp` first, so
/// a failure halfway through leaves the destination empty rather than half a series.
/// Returns the number of patch files.
fn copy_series_files(source: &Layout, target: &Layout) -> Result<usize> {
    let src = seriesdir::Built::read(source)?;
    let count = src.patches.len();
    let built = seriesdir::Built::build(
        target,
        target.series(),
        &src.series_text,
        src.base_text.as_deref(),
        &src.patches,
    )?;
    built.install(target)?;
    Ok(count)
}

/// `skot series switch <name>`: point everything at another series.
fn switch(ctx: &Ctx, args: &SeriesSwitchArgs) -> Result<Exit> {
    let target = ctx.layout.with_series(&args.name)?;
    if !target.patches_dir().is_dir() {
        bail!(
            "there is no series `{}`.\n`{}` lists what exists; `{}` creates it.",
            args.name,
            skot("series"),
            skot_args("series", "create <name>")
        );
    }

    let mut state = State::load(&ctx.layout)?;
    // The pointer decides "already there", not this run's resolution: with the
    // pointer lost, `series switch` is the command that records it again, and it must
    // not talk its way out of that.
    if state.active_series() == Some(args.name.as_str()) {
        // Already there. Rebuilding the sandbox would redo work for nothing, and a
        // no-op must not read as a switch that might have failed.
        if ctx.cli.json {
            emit_json(&serde_json::json!({
                "series_name": args.name,
                "created": false,
                "active": true,
                "sandbox": "unchanged",
            }))?;
        } else {
            info!(target: "series", "already on `{}`", args.name);
        }
        return Ok(Exit::Success);
    }

    // Held across the switch: the rebuild below mutates the sandbox.
    let lock = ctx.lock()?;
    state.set_active_series(&ctx.layout, &args.name)?;
    debug!(target: "series", "switched the pointer to `{}`", args.name);
    let rebuild = after_switch(&target)?;
    drop(lock);

    let doc = serde_json::json!({
        "series_name": args.name,
        "created": false,
        "active": true,
        "sandbox": rebuild.status,
        "details": rebuild.details(),
    });
    if ctx.cli.json {
        emit_json(&doc)?;
    } else {
        info!(target: "series", "switched to `{}`", args.name);
        rebuild.narrate(&args.name);
        Hints::new("next")
            .entry(skot("build"), "run your hooks from here")
            .print();
    }
    Ok(rebuild.exit())
}

/// `skot series copy <name>`: a checkpoint of another series under a new name.
///
/// Copying is not switching: the active series, the pointer and the sandbox all stay
/// where they are, so a checkpoint can be taken mid-experiment without disturbing it.
fn copy(ctx: &Ctx, args: &SeriesCopyArgs) -> Result<Exit> {
    let source = match &args.from {
        Some(from) => {
            if from == &args.name {
                bail!("a series cannot be copied onto itself.");
            }
            let source = ctx.layout.with_series(from)?;
            if !source.patches_dir().is_dir() {
                bail!(
                    "there is no series `{}`.\n`{}` lists what exists.",
                    from,
                    skot("series")
                );
            }
            source
        }
        None => ctx.layout.clone(),
    };
    let from = source.series().to_owned();
    let target = ctx.layout.with_series(&args.name)?;
    if target.patches_dir().exists() {
        bail!(
            "the series `{}` already exists.\n`{}` switches to it.",
            args.name,
            skot_args("series", &format!("switch {}", args.name))
        );
    }
    refuse_case_clash(&ctx.layout, &args.name)?;

    let patches = copy_series_files(&source, &target)?;
    debug!(
        target: "series",
        "copied `{from}` to `{}` ({patches} patch files)",
        args.name
    );

    if ctx.cli.json {
        emit_json(&serde_json::json!({
            "series_name": args.name,
            "from": from,
            "patches": patches,
            "active": ctx.layout.series(),
        }))?;
    } else {
        info!(
            target: "series",
            "copied `{from}` to `{}`; `{}` stays active",
            args.name,
            ctx.layout.series()
        );
        Hints::new("next")
            .entry(
                skot_args("series", &format!("switch {}", args.name)),
                "switch to the copy when you want it",
            )
            .print();
    }
    Ok(Exit::Success)
}

/// `skot series undo`: trade the active series with the version rebase kept.
///
/// The two directories exchange places, so a second undo puts back what the first took
/// away. Nothing is copied: the undo buffer is one entry deep by construction, and the
/// trade is what keeps it that way. The sandbox follows whichever version ends up
/// active, the same way a switch does.
fn undo(ctx: &Ctx, args: &SeriesUndoArgs) -> Result<Exit> {
    let name = ctx.layout.series().to_owned();
    if !ctx.layout.undo_entry().is_dir() {
        bail!(
            "there is nothing to undo for `{name}`.\n\
             Undo is the version the last successful rebase replaced, and this series \
             has none."
        );
    }
    if !confirm_undo(ctx, &name, args.yes)? {
        info!(target: "series", "`{name}` stands as it is");
        return Ok(Exit::Failure);
    }

    let lock = ctx.lock()?;
    seriesdir::recover(&ctx.layout)?;
    seriesdir::undo_swap(&ctx.layout)?;
    // Whatever version is now active, its sandbox is rebuilt from it, the same way a
    // switch rebuilds: the tree the other version left is derived from the series that
    // is no longer there.
    let rebuild = after_switch(&ctx.layout)?;
    drop(lock);

    let patches = Series::load(&ctx.layout.series_file()).map_or(0, |s| s.len());
    debug!(target: "series", "undid the last rebase of `{name}` ({patches} patches)");
    if ctx.cli.json {
        emit_json(&serde_json::json!({
            "series_name": name,
            "undone": true,
            "patches": patches,
            "sandbox": rebuild.status,
            "details": rebuild.details(),
        }))?;
    } else {
        info!(
            target: "series",
            "restored the previous `{name}` ({}); the version it replaced is now the one \
             undo would put back",
            apply::count(patches, "patch", "patches")
        );
        rebuild.narrate(&name);
    }
    Ok(rebuild.exit())
}

/// The question before a swap: a `--yes` caller has already answered it, and a run
/// that cannot ask is refused with the flag that says yes for it.
fn confirm_undo(ctx: &Ctx, name: &str, yes: bool) -> Result<bool> {
    let question = format!("Put the previous version of `{name}` back?");
    let help = format!(
        "The two versions trade places, so undoing again reverses this. `{}` says yes \
         in advance.",
        skot_args("series", "undo --yes")
    );
    Ok(ctx.asker().with_yes(yes).confirm(
        &question,
        Some(&help),
        Fallback::Flag(skot_args("series", "undo --yes")),
    )?)
}

/// `skot series drop <name>`: a series, its patches and its base, for good.
///
/// The active series is refused rather than switched away from first: dropping the
/// series you are on is either a mistake or a two-step decision, and the two steps say
/// so out loud. Open work areas are ended when they hold nothing their patch lacks,
/// and refused when they do, because the edits exist nowhere else.
fn drop_series(ctx: &Ctx, args: &SeriesDropArgs) -> Result<Exit> {
    let target = ctx.layout.with_series(&args.name)?;
    if !target.patches_dir().is_dir() {
        bail!(
            "there is no series `{}`.\n`{}` lists what exists.",
            args.name,
            skot("series")
        );
    }
    // The pointer decides "active", the same way `series switch` reads it: with the
    // pointer lost, this run's resolution is a guess and a guess cannot decide a delete.
    let active = State::load(&ctx.layout)
        .ok()
        .and_then(|state| state.active_series().map(str::to_owned))
        .unwrap_or_else(|| ctx.layout.series().to_owned());
    if active == args.name {
        bail!(
            "`{}` is the active series, so skot will not remove it.\n\
             `{}` to another series first, then drop this one.",
            args.name,
            skot_args("series", "switch <name>")
        );
    }

    let manifest = session::Manifest::load(&ctx.layout)?;
    let open: Vec<session::Session> = manifest
        .sessions
        .iter()
        .filter(|s| s.series_name() == args.name)
        .cloned()
        .collect();
    // `unsaved` needs a mirror to compare against; with none, no work area can exist.
    if !open.is_empty() && ctx.layout.mirror_dir().exists() {
        let mut held = Vec::new();
        for session in &open {
            if session::unsaved(&ctx.layout, session)? {
                held.push(session.patch.clone());
            }
        }
        if !held.is_empty() {
            bail!(
                "the series `{}` holds work its patches do not: {}.\n\
                 Save it (`{}`) or throw it away (`{}`) per patch, then drop the series.",
                args.name,
                held.join(", "),
                skot_args("save", "<patch>"),
                skot_args("clean", "--sessions")
            );
        }
    }

    let patches = crate::series::Series::load(&target.series_file()).map_or(0, |s| s.len());
    if !confirm_drop(ctx, &args.name, patches)? {
        info!(target: "series", "the series `{}` stands", args.name);
        return Ok(Exit::Failure);
    }

    let lock = ctx.lock()?;
    // Clean work areas are retired first: their patches are about to disappear, and a
    // worktree with no patch is a directory only someone who knows to look would find.
    for session in &open {
        session::end(&ctx.layout, session)?;
    }
    let mut state = State::load(&ctx.layout)?;
    state.sandboxes.remove(&args.name);
    if ctx.layout.per_series_sandbox() {
        remove_sandbox_tree(&ctx.layout.with_series(&args.name)?)?;
    }
    state.save(&ctx.layout)?;
    if !target.guarded_remove(&target.patches_dir())? {
        bail!(
            "nothing was removed: {} does not exist.",
            target.patches_dir().display()
        );
    }
    crate::blobs::sweep(&ctx.layout).context("sweeping binary content no patch references")?;
    drop(lock);

    follow_committed_default(ctx, &args.name, None);
    debug!(target: "series", "dropped `{}` ({patches} patches)", args.name);
    if ctx.cli.json {
        emit_json(&serde_json::json!({
            "series_name": args.name,
            "dropped": true,
            "patches": patches,
            "work_areas_retired": open.len(),
        }))?;
    } else {
        info!(
            target: "series",
            "removed `{}` ({}); it is gone from disk, and no undo follows",
            args.name,
            apply::count(patches, "patch", "patches")
        );
    }
    Ok(Exit::Success)
}

/// The question, before anything is deleted: a `--yes` caller has already answered it,
/// and a run that cannot ask is refused with the flag that says yes for it, the same
/// way `patch drop` does it. Under `--json` that refusal is a usage error rather than
/// a document, since a document cannot answer a question.
fn confirm_drop(ctx: &Ctx, name: &str, patches: usize) -> Result<bool> {
    let question = format!(
        "Remove the series `{name}` and its {}?",
        apply::count(patches, "patch", "patches")
    );
    let help = format!(
        "This deletes every patch file it holds. `{}` says yes in advance.",
        skot_args("series", &format!("drop {name} --yes"))
    );
    Ok(ctx.asker().confirm(
        &question,
        Some(&help),
        Fallback::Flag(skot_args("series", &format!("drop {name} --yes"))),
    )?)
}

/// Remove a per-series sandbox tree, which is derived state the next apply rebuilds.
fn remove_sandbox_tree(layout: &Layout) -> Result<()> {
    let dir = layout.sandbox_dir();
    if !dir.exists() {
        return Ok(());
    }
    // The check `guarded_remove` cannot make: sandbox trees live under `.skot`, one
    // level outside what `guarded_remove` allows, and are rebuilt rather than kept.
    if !dir.starts_with(layout.sandbox_root()) {
        bail!("refusing to remove {}: not a sandbox tree.", dir.display());
    }
    fs::remove_dir_all(&dir).context(format!("removing {}", dir.display()))
}

/// Keep the committed default in `skot.toml` from naming a series that is gone. The
/// field is the project's checked-in choice, and `rename` or `drop` just made the name
/// it holds wrong. One line is rewritten, or commented out when the series is dropped,
/// and nothing else in the file moves. The command behind it has already happened by
/// the time this runs, so anything this cannot do is a log line, never a failure.
fn follow_committed_default(ctx: &Ctx, from: &str, to: Option<&str>) {
    let named = ctx
        .config()
        .is_ok_and(|config| config.series.as_deref() == Some(from));
    if !named {
        return;
    }
    let path = ctx.layout.config_file();
    let Ok(text) = fs::read_to_string(&path) else {
        return;
    };
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let mut edited = false;
    for line in &mut lines {
        // The key lives at the top level: everything after the first table header
        // belongs to that table.
        if line.trim_start().starts_with('[') {
            break;
        }
        let series_key = line
            .trim_start()
            .strip_prefix("series")
            .is_some_and(|rest| rest.trim_start().starts_with('='));
        if series_key {
            *line = match to {
                Some(name) => format!("series = {}", toml::Value::String(name.to_owned())),
                None => format!("# {line}"),
            };
            edited = true;
            break;
        }
    }
    if !edited {
        return;
    }
    let mut out = lines.join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    if let Err(err) = fs::write(&path, &out) {
        warn!(target: "series", "could not update {}: {err}", path.display());
        return;
    }
    match to {
        Some(name) => info!(target: "series", "the committed default series is now `{name}`"),
        None => info!(
            target: "series",
            "the committed default named `{from}`; its line is commented out"
        ),
    }
}

/// `skot series rename <name>`: the active series under a new name.
///
/// Everything that names the series follows: the pointer, the series directory, open
/// work areas and their worktree registrations in the mirror. The series' sandbox tree
/// is derived state and is rebuilt by the next apply under the new name.
fn rename(ctx: &Ctx, args: &SeriesRenameArgs) -> Result<Exit> {
    let from = ctx.layout.series().to_owned();
    let target = ctx.layout.with_series(&args.name)?;
    if from == args.name {
        info!(target: "series", "the series is already called `{from}`");
        return Ok(Exit::Success);
    }
    if target.patches_dir().exists() {
        bail!(
            "the series `{}` already exists.\n`{}` switches to it; to replace it, \
             `{}` first.",
            args.name,
            skot_args("series", &format!("switch {}", args.name)),
            skot_args("series", &format!("drop {}", args.name))
        );
    }
    refuse_case_clash(&ctx.layout, &args.name)?;

    let manifest = session::Manifest::load(&ctx.layout)?;
    let open: Vec<session::Session> = manifest
        .sessions
        .iter()
        .filter(|s| s.series_name() == from)
        .cloned()
        .collect();
    // `unsaved` needs a mirror to compare against; with none, no work area can exist.
    if !open.is_empty() && ctx.layout.mirror_dir().exists() {
        let mut held = Vec::new();
        for session in &open {
            if session::unsaved(&ctx.layout, session)? {
                held.push(session.patch.clone());
            }
        }
        if !held.is_empty() {
            bail!(
                "the series `{from}` holds work its patches do not: {}.\n\
                 Save it (`{}`) or throw it away (`{}`) per patch, then rename.",
                held.join(", "),
                skot_args("save", "<patch>"),
                skot_args("clean", "--sessions")
            );
        }
    }

    let lock = ctx.lock()?;
    fs::rename(ctx.layout.patches_dir(), target.patches_dir()).context(format!(
        "renaming {} to {}",
        ctx.layout.patches_dir().display(),
        target.patches_dir().display()
    ))?;
    let mut state = State::load(&ctx.layout)?;
    state.set_active_series(&ctx.layout, &args.name)?;
    // The sandbox tree is derived from the series name and is rebuilt by the next
    // apply; the record goes so nothing pretends the old tree still stands.
    state.sandboxes.remove(&from);
    if ctx.layout.per_series_sandbox() {
        remove_sandbox_tree(&ctx.layout)?;
    }
    state.save(&ctx.layout)?;

    // Work areas and their worktree registrations follow the rename. A registration
    // cannot be renamed through libgit2, so the two files git keeps for a linked
    // worktree are rewritten the way `git worktree move` does it.
    if !open.is_empty() {
        let mirror = crate::git::mirror::Mirror::open(&ctx.layout)?;
        let mut moved = open.clone();
        for session in &mut moved {
            let old_dir = session.dir_path(&ctx.layout)?;
            let new_dir = ctx
                .layout
                .work_patch_dir_for(&args.name, session.dir_name())?;
            if let Some(parent) = new_dir.parent() {
                fs::create_dir_all(parent).context(format!("creating {}", parent.display()))?;
            }
            fs::rename(&old_dir, &new_dir).context(format!(
                "moving {} to {}",
                old_dir.display(),
                new_dir.display()
            ))?;
            let old_reg = crate::git::sandbox::session_worktree_name(&from, session.dir_name());
            let new_reg =
                crate::git::sandbox::session_worktree_name(&args.name, session.dir_name());
            rewrite_registration(&mirror, &old_reg, &new_reg, &new_dir)?;
            session.series = args.name.clone();
        }
        let mut manifest = manifest;
        manifest.sessions.retain(|s| s.series_name() != from);
        for session in moved {
            manifest.insert(session);
        }
        manifest.save(&ctx.layout)?;
    }
    drop(lock);

    follow_committed_default(ctx, &from, Some(&args.name));
    debug!(
        target: "series",
        "renamed `{from}` to `{}` ({} work areas)",
        args.name,
        open.len()
    );
    if ctx.cli.json {
        emit_json(&serde_json::json!({
            "series_name": args.name,
            "renamed_from": from,
            "work_areas": open.len(),
            "sandbox": "rebuilt on next apply",
        }))?;
    } else {
        info!(
            target: "series",
            "renamed `{from}` to `{}`; the next apply rebuilds its sandbox",
            args.name
        );
        Hints::new("next")
            .entry(skot("build"), "rebuild the sandbox under the new name")
            .print();
    }
    Ok(Exit::Success)
}

/// Move a linked worktree's registration the way `git worktree move` does: rename the
/// admin directory, then point the two `gitdir` files at each other's new homes.
fn rewrite_registration(
    mirror: &crate::git::mirror::Mirror,
    old_name: &str,
    new_name: &str,
    new_dir: &std::path::Path,
) -> Result<()> {
    let admin_root = mirror.repo().path().join("worktrees");
    let old_reg = admin_root.join(old_name);
    let new_reg = admin_root.join(new_name);
    if !old_reg.is_dir() {
        // No registration under the old name is not a reason to fail the rename: the
        // directory moved, and the next edit can re-register under the new name.
        debug!(
            target: "series",
            "no worktree registration under `{old_name}` to move"
        );
        return Ok(());
    }
    fs::rename(&old_reg, &new_reg).context(format!(
        "renaming the worktree registration `{old_name}` to `{new_name}`"
    ))?;
    // Absolute, the way libgit2 prettifies them when it creates a worktree: both files
    // hold paths that are read back without a working directory to resolve against.
    let new_reg =
        fs::canonicalize(&new_reg).with_context(|| format!("resolving {}", new_reg.display()))?;
    let new_dir =
        fs::canonicalize(new_dir).with_context(|| format!("resolving {}", new_dir.display()))?;
    // The admin side points at the worktree's `.git` file; the worktree side points at
    // the admin directory itself. Both shapes are what libgit2 writes when it creates a
    // worktree, so what it renames it can also reopen.
    fs::write(
        new_reg.join("gitdir"),
        format!("{}/.git\n", new_dir.display()),
    )
    .context(format!("writing {}", new_reg.join("gitdir").display()))?;
    fs::write(
        new_dir.join(".git"),
        format!("gitdir: {}\n", new_reg.display()),
    )
    .context(format!("writing {}", new_dir.join(".git").display()))?;
    Ok(())
}

/// What a switch did to the shared sandbox, per `[sandbox] on-switch`.
struct Rebuild {
    /// The word `--json` gets: `rebuilt`, `kept`, `per-series`, `no-mirror` or `failed`.
    status: &'static str,
    /// The apply behind a `rebuilt` or `failed`, for the document and the narration.
    applied: Option<Applied>,
}

impl Rebuild {
    /// The document half: the ref the sandbox ended up on, when a rebuild ran.
    fn details(&self) -> serde_json::Value {
        match &self.applied {
            Some(applied) => serde_json::json!({
                "ok": applied.ok,
                "ref": applied.ref_name,
                "sha": applied.sha,
                "applied": applied
                    .report
                    .patches
                    .iter()
                    .map(|p| p.name.clone())
                    .collect::<Vec<_>>(),
                "failure": applied.failure,
            }),
            None => serde_json::Value::Null,
        }
    }

    /// The human half: only failures speak, since the switch itself was already said.
    fn narrate(&self, series: &str) {
        let Some(applied) = &self.applied else { return };
        if applied.ok {
            info!(
                target: "series",
                "the sandbox now holds `{series}` ({}), on {} ({})",
                apply::count(applied.report.patch_count, "patch", "patches"),
                applied.ref_name,
                applied.sha,
            );
            return;
        }
        error!(
            target: "series",
            "`{series}` does not apply to {}",
            applied.report.base
        );
        for line in applied.report.summary().lines() {
            debug!(target: "series", "{line}");
        }
        info!(
            target: "series",
            "The switch stands; the sandbox was reset and holds upstream only."
        );
        info!(
            target: "series",
            "`{}` shows the conflicts; rebasing a patch and `{}` fixes one.",
            skot("check"),
            skot_args("save", "<patch>")
        );
    }

    /// A rebuild that failed is the only outcome the switch has to answer for.
    fn exit(&self) -> Exit {
        if self.status == "failed" {
            Exit::Failure
        } else {
            Exit::Success
        }
    }
}

/// Put the sandbox where the switch says it belongs.
///
/// Only a *shared* sandbox has anywhere to go: in per-series mode each series owns its
/// own tree, and the switched-to series' tree is already its own, present or not.
fn after_switch(layout: &Layout) -> Result<Rebuild> {
    let config = layout.config()?;
    if layout.per_series_sandbox() {
        return Ok(Rebuild {
            status: "per-series",
            applied: None,
        });
    }
    match config.sandbox.on_switch {
        OnSwitch::Keep => {
            info!(
                target: "series",
                "the sandbox stays where it is; the next build or apply moves it"
            );
            Ok(Rebuild {
                status: "kept",
                applied: None,
            })
        }
        OnSwitch::Rebuild => {
            if !layout.mirror_dir().exists() {
                // Nothing to apply against yet: a project that has never synced has no
                // sandbox to rebuild, and starting one would mean a fetch nobody asked for.
                info!(
                    target: "series",
                    "no mirror to apply against yet; `{}` then `{}`",
                    skot("sync"),
                    skot("apply")
                );
                return Ok(Rebuild {
                    status: "no-mirror",
                    applied: None,
                });
            }
            let applied = apply::apply_locked(layout, None)?;
            let status = if applied.ok { "rebuilt" } else { "failed" };
            Ok(Rebuild {
                status,
                applied: Some(applied),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{
        Cli, SeriesCommand, SeriesCopyArgs, SeriesCreateArgs, SeriesDropArgs, SeriesRenameArgs,
        SeriesSwitchArgs,
    };
    use crate::commands::testsupport::write_series;
    use crate::layout::DEFAULT_SERIES;
    use clap::Parser as _;

    fn ctx_for(layout: &Layout) -> Ctx {
        Ctx {
            layout: layout.clone(),
            cli: Cli::try_parse_from(["skot", "series"]).unwrap(),
            yes: false,
        }
    }

    fn call(ctx: &Ctx, command: Option<SeriesCommand>) -> Result<Exit> {
        run(ctx, &SeriesArgs { command })
    }

    fn create(ctx: &Ctx, name: &str) -> Result<Exit> {
        call(
            ctx,
            Some(SeriesCommand::Create(SeriesCreateArgs {
                name: name.to_owned(),
                from: None,
            })),
        )
    }

    fn switch(ctx: &Ctx, name: &str) -> Result<Exit> {
        call(
            ctx,
            Some(SeriesCommand::Switch(SeriesSwitchArgs {
                name: name.to_owned(),
            })),
        )
    }

    #[test]
    fn a_bare_series_lists_what_exists_and_marks_the_active_one() {
        let (_tmp, layout) = crate::commands::testsupport::project();
        fs::create_dir_all(layout.series_root().join("alpha")).unwrap();
        fs::create_dir_all(layout.series_root().join("exp")).unwrap();
        write_series(&layout, &[("a.patch", "a\n")]);

        let ctx = ctx_for(&layout);
        let exit = call(&ctx, None).unwrap();
        assert!(matches!(exit, Exit::Success));
        // The rows themselves are narration; the directories being listed is the shape.
        assert!(layout.series_root().join("alpha").is_dir());
        assert!(layout.series_root().join("exp").is_dir());
    }

    #[test]
    fn creating_a_series_switches_the_pointer_and_keeps_the_base() {
        let (_tmp, layout) = crate::commands::testsupport::project();
        fs::write(layout.base_file(), "v2\n").unwrap();
        let ctx = ctx_for(&layout);

        create(&ctx, "exp").unwrap();

        assert!(layout.with_series("exp").unwrap().patches_dir().is_dir());
        assert_eq!(
            fs::read_to_string(layout.with_series("exp").unwrap().base_file()).unwrap(),
            "v2\n"
        );
        let state = State::load(&layout).unwrap();
        assert_eq!(state.active_series(), Some("exp"));
    }

    #[test]
    fn creating_an_existing_series_points_at_switching_instead() {
        let (_tmp, layout) = crate::commands::testsupport::project();
        fs::create_dir_all(layout.series_root().join("exp")).unwrap();
        let ctx = ctx_for(&layout);

        let err = create(&ctx, "exp").unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("already exists"), "got {text}");
        assert!(text.contains("switch exp"), "got {text}");
    }

    /// The context the next invocation would get: the pointer decides the series.
    fn ctx_now(layout: &Layout) -> Ctx {
        let series = State::load(layout)
            .ok()
            .and_then(|state| state.active_series().map(str::to_owned))
            .unwrap_or_else(|| DEFAULT_SERIES.to_owned());
        ctx_for(&layout.with_series(&series).unwrap())
    }

    #[test]
    fn switching_moves_the_pointer_and_a_missing_series_is_refused() {
        let (_tmp, layout) = crate::commands::testsupport::project();
        create(&ctx_for(&layout), "exp").unwrap();

        // Back to the default, then over to exp, then again for the no-op.
        switch(&ctx_now(&layout), DEFAULT_SERIES).unwrap();
        assert_eq!(
            State::load(&layout).unwrap().active_series(),
            Some(DEFAULT_SERIES)
        );
        switch(&ctx_now(&layout), "exp").unwrap();
        assert_eq!(State::load(&layout).unwrap().active_series(), Some("exp"));
        assert!(matches!(
            switch(&ctx_now(&layout), "exp").unwrap(),
            Exit::Success
        ));

        let err = switch(&ctx_now(&layout), "nowhere").unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("no series `nowhere`"), "got {text}");
        assert!(text.contains(&skot("series")), "got {text}");
    }

    #[test]
    fn switching_records_the_pointer_even_under_a_one_shot_flag() {
        // A lost pointer is fixed by `series switch`, so the command must record even
        // when `--series` already named the target for this run: the no-op branch
        // answers to the pointer, not to the resolution above it.
        let (_tmp, layout) = crate::commands::testsupport::project();
        fs::create_dir_all(layout.series_root().join("exp")).unwrap();
        let ctx = Ctx {
            layout: layout.with_series("exp").unwrap(),
            cli: Cli::try_parse_from(["skot", "--series", "exp", "series", "switch", "exp"])
                .unwrap(),
            yes: false,
        };

        switch(&ctx, "exp").unwrap();

        assert_eq!(
            State::load(&layout).unwrap().active_series(),
            Some("exp"),
            "the pointer must be recorded, not assumed"
        );
    }

    #[test]
    fn copying_leaves_the_active_series_alone() {
        let (_tmp, layout) = crate::commands::testsupport::project();
        write_series(
            &layout,
            &[("a.patch", crate::commands::testsupport::patch_one())],
        );
        let ctx = ctx_for(&layout);

        let exit = call(
            &ctx,
            Some(SeriesCommand::Copy(SeriesCopyArgs {
                name: "checkpoint".to_owned(),
                from: None,
            })),
        )
        .unwrap();
        assert!(matches!(exit, Exit::Success));

        let copy = layout.with_series("checkpoint").unwrap();
        assert_eq!(fs::read_to_string(copy.series_file()).unwrap(), "a.patch\n");
        assert!(copy.patches_dir().join("a.patch").is_file());
        assert_eq!(layout.series(), DEFAULT_SERIES, "copying is not switching");
        assert!(
            State::load(&layout).unwrap().active_series().is_none(),
            "copying does not write the pointer"
        );
    }

    #[test]
    fn copying_and_seeding_read_the_named_series_not_the_active_one() {
        let (_tmp, layout) = crate::commands::testsupport::project();
        // The active series is empty; `exp` is the one with patches in it.
        create(&ctx_for(&layout), "exp").unwrap();
        write_series(
            &layout.with_series("exp").unwrap(),
            &[
                ("a.patch", crate::commands::testsupport::patch_one()),
                ("b.patch", crate::commands::testsupport::patch_two()),
            ],
        );
        let ctx = ctx_for(&layout);

        call(
            &ctx,
            Some(SeriesCommand::Copy(SeriesCopyArgs {
                name: "checkpoint".to_owned(),
                from: Some("exp".to_owned()),
            })),
        )
        .unwrap();
        call(
            &ctx,
            Some(SeriesCommand::Create(SeriesCreateArgs {
                name: "seeded".to_owned(),
                from: Some("exp".to_owned()),
            })),
        )
        .unwrap();

        for name in ["checkpoint", "seeded"] {
            let layout = layout.with_series(name).unwrap();
            let text = fs::read_to_string(layout.series_file()).unwrap();
            assert!(
                text.contains("a.patch") && text.contains("b.patch"),
                "{name} got {text:?}"
            );
            assert!(layout.patches_dir().join("b.patch").is_file());
        }
        assert_eq!(
            State::load(&layout).unwrap().active_series(),
            Some("seeded"),
            "create switches; copy does not"
        );
    }

    #[test]
    fn dropping_removes_the_series_but_never_the_active_one() {
        let (_tmp, layout) = crate::commands::testsupport::project();
        create(&ctx_for(&layout), "exp").unwrap();
        write_series(&layout.with_series("exp").unwrap(), &[("a.patch", "a\n")]);
        // Back on the default series: the pointer, not this run's layout, is what
        // decides "active" for a drop.
        switch(
            &ctx_for(&layout.with_series("exp").unwrap()),
            DEFAULT_SERIES,
        )
        .unwrap();
        let ctx = Ctx {
            layout: layout.with_series(DEFAULT_SERIES).unwrap(),
            cli: Cli::try_parse_from(["skot", "--yes", "series", "drop", "exp"]).unwrap(),
            yes: true,
        };

        // The active series is refused, pointer first: this run's own resolution is a
        // guess when the pointer is lost, and a guess cannot decide a delete.
        let err = call(
            &ctx,
            Some(SeriesCommand::Drop(SeriesDropArgs {
                name: DEFAULT_SERIES.to_owned(),
            })),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("active series"), "{err:#}");

        let exit = call(
            &ctx,
            Some(SeriesCommand::Drop(SeriesDropArgs {
                name: "exp".to_owned(),
            })),
        )
        .unwrap();
        assert!(matches!(exit, Exit::Success));
        assert!(!layout.with_series("exp").unwrap().patches_dir().exists());
        assert!(
            layout
                .with_series(DEFAULT_SERIES)
                .unwrap()
                .patches_dir()
                .is_dir(),
            "the active series is untouched"
        );
    }

    #[test]
    fn dropping_comments_out_a_committed_default_that_named_it() {
        // The committed default must never name a series that is gone. Dropping the
        // series it holds is that situation, so the line goes, not the rest of the file.
        let (_tmp, layout) = crate::commands::testsupport::project();
        create(&ctx_for(&layout), "exp").unwrap();
        write_series(&layout.with_series("exp").unwrap(), &[("a.patch", "a\n")]);
        fs::write(
            layout.config_file(),
            "# keep this comment\nseries = \"exp\"\n[upstream]\nurl = \"unused\"\n",
        )
        .unwrap();
        // Back on the default: `exp` is droppable, and the committed default still
        // names it, which is the situation the line-edit exists for. Switching is a
        // local act and leaves that line alone.
        switch(
            &ctx_for(&layout.with_series("exp").unwrap()),
            DEFAULT_SERIES,
        )
        .unwrap();
        let ctx = Ctx {
            layout: layout.with_series(DEFAULT_SERIES).unwrap(),
            cli: Cli::try_parse_from(["skot", "series", "drop", "exp", "--yes"]).unwrap(),
            yes: true,
        };

        call(
            &ctx,
            Some(SeriesCommand::Drop(SeriesDropArgs {
                name: "exp".to_owned(),
            })),
        )
        .unwrap();

        let text = fs::read_to_string(layout.config_file()).unwrap();
        assert!(text.contains("# series = \"exp\""), "{text}");
        assert!(text.contains("# keep this comment"), "{text}");
        assert_eq!(
            crate::config::Config::parse(&text).unwrap().series,
            None,
            "no series named in the file any more"
        );
    }

    #[test]
    fn dropping_without_yes_in_a_prompt_free_environment_is_told_how() {
        let (_tmp, layout) = crate::commands::testsupport::project();
        create(&ctx_for(&layout), "exp").unwrap();
        write_series(&layout.with_series("exp").unwrap(), &[("a.patch", "a\n")]);
        // Back on the default, so `exp` is droppable and only the question stands
        // between it and the delete.
        switch(
            &ctx_for(&layout.with_series("exp").unwrap()),
            DEFAULT_SERIES,
        )
        .unwrap();
        let ctx = Ctx {
            layout: layout.with_series(DEFAULT_SERIES).unwrap(),
            cli: Cli::try_parse_from(["skot", "series", "drop", "exp"]).unwrap(),
            yes: false,
        };
        // No terminal under the test harness, which is the case the message exists for:
        // `confirm_drop` refuses with the remedy rather than opening a prompt no one
        // answers. Opening a prompt instead would only refuse on its own when stdin is
        // already not a terminal, so `cargo test` from a shell would block.
        let err = call(
            &ctx,
            Some(SeriesCommand::Drop(SeriesDropArgs {
                name: "exp".to_owned(),
            })),
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("cannot ask"), "{err}");
        assert!(err.contains("--yes"), "{err}");
        assert!(
            layout.with_series("exp").unwrap().patches_dir().is_dir(),
            "nothing is deleted when the question cannot be asked"
        );
    }

    #[test]
    fn renaming_moves_the_pointer_the_directory_and_anything_named_after_it() {
        let (_tmp, layout) = crate::commands::testsupport::project();
        create(&ctx_for(&layout), "exp").unwrap();
        write_series(&layout.with_series("exp").unwrap(), &[("a.patch", "a\n")]);
        fs::write(layout.with_series("exp").unwrap().base_file(), "main\n").unwrap();
        let ctx = ctx_for(&layout.with_series("exp").unwrap());

        call(
            &ctx,
            Some(SeriesCommand::Rename(SeriesRenameArgs {
                name: "renamed".to_owned(),
            })),
        )
        .unwrap();

        assert!(!layout.with_series("exp").unwrap().patches_dir().exists());
        let target = layout.with_series("renamed").unwrap();
        assert!(target.patches_dir().join("a.patch").is_file());
        assert_eq!(fs::read_to_string(target.base_file()).unwrap(), "main\n");
        assert_eq!(
            State::load(&layout).unwrap().active_series(),
            Some("renamed")
        );
    }

    #[test]
    fn renaming_follows_the_committed_default_series_line() {
        // The `series` field is the project's checked-in default. Renaming the series it
        // names must not leave the line naming something that is gone.
        let (_tmp, layout) = crate::commands::testsupport::project();
        create(&ctx_for(&layout), "exp").unwrap();
        fs::write(
            layout.config_file(),
            "# keep this comment\nseries = \"exp\"\n[upstream]\nurl = \"unused\"\n",
        )
        .unwrap();
        let ctx = ctx_for(&layout.with_series("exp").unwrap());

        call(
            &ctx,
            Some(SeriesCommand::Rename(SeriesRenameArgs {
                name: "renamed".to_owned(),
            })),
        )
        .unwrap();

        let text = fs::read_to_string(layout.config_file()).unwrap();
        assert!(text.contains("series = \"renamed\""), "{text}");
        assert!(text.contains("# keep this comment"), "{text}");
    }

    #[test]
    fn renaming_refuses_a_name_that_already_exists() {
        let (_tmp, layout) = crate::commands::testsupport::project();
        create(&ctx_for(&layout), "exp").unwrap();
        // Made by hand, not by `create`, so the pointer stays on `exp`.
        fs::create_dir_all(layout.series_root().join("other")).unwrap();
        let ctx = ctx_for(&layout.with_series("exp").unwrap());

        let err = call(
            &ctx,
            Some(SeriesCommand::Rename(SeriesRenameArgs {
                name: "other".to_owned(),
            })),
        )
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("already exists"), "{text}");
        assert!(layout.with_series("other").unwrap().patches_dir().is_dir());
        assert_eq!(
            State::load(&layout).unwrap().active_series(),
            Some("exp"),
            "a refused rename leaves the pointer alone"
        );
    }

    #[test]
    fn renaming_carries_open_work_areas_and_their_registrations() {
        // A work area's registration name embeds the series, and git has no operation
        // that renames one in place: the rename rewrites the two gitdir files the way
        // `git worktree move` does, and this is the proof the area still opens.
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = crate::commands::testsupport::mirror_with_one_commit(tmp.path());
        fs::write(
            layout.config_file(),
            "[upstream]\nurl = \"unused\"\ndefault-ref = \"main\"\n",
        )
        .unwrap();
        write_series(
            &layout,
            &[("a.patch", crate::commands::testsupport::patch_one())],
        );
        create(&ctx_for(&layout), "exp").unwrap();
        write_series(
            &layout.with_series("exp").unwrap(),
            &[("a.patch", crate::commands::testsupport::patch_one())],
        );
        let exp = layout.with_series("exp").unwrap();
        let entries = exp.project().unwrap().1.entries(&exp).unwrap();
        let plan = crate::session::plan(&exp, "main", &[], Some(&entries[0])).unwrap();
        crate::session::materialise(&exp, "a.patch", &plan).unwrap();

        call(
            &ctx_for(&exp),
            Some(SeriesCommand::Rename(SeriesRenameArgs {
                name: "renamed".to_owned(),
            })),
        )
        .unwrap();

        let manifest = session::Manifest::load(&layout).unwrap();
        let session = &manifest.sessions[0];
        assert_eq!(session.series_name(), "renamed");
        assert!(
            session.dir_path(&layout).unwrap().exists(),
            "the work area directory followed the rename"
        );
        let mirror = crate::git::mirror::Mirror::open(&layout).unwrap();
        let new_reg = crate::git::sandbox::session_worktree_name("renamed", session.dir_name());
        let old_reg = crate::git::sandbox::session_worktree_name("exp", session.dir_name());
        assert!(
            mirror.repo().find_worktree(&new_reg).is_ok(),
            "the registration was rewritten under the new name"
        );
        assert!(mirror.repo().find_worktree(&old_reg).is_err());
        assert!(
            !session::unsaved(&layout, session).unwrap(),
            "the work area still opens and still matches its patch"
        );
    }

    #[test]
    fn a_series_name_that_differs_only_in_case_is_refused_at_the_boundary() {
        let (_tmp, layout) = crate::commands::testsupport::project();
        create(&ctx_for(&layout), "exp").unwrap();
        let ctx = ctx_for(&layout.with_series("exp").unwrap());

        let err = create(&ctx, "Exp").unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("differs only in case"), "{text}");
        assert!(text.contains("`exp`"), "{text}");
        assert!(!layout.with_series("Exp").unwrap().patches_dir().exists());
    }

    #[test]
    fn all_and_current_cannot_be_series_names() {
        let (_tmp, layout) = crate::commands::testsupport::project();
        let ctx = ctx_for(&layout);
        for name in ["all", "current", "ALL"] {
            let err = create(&ctx, name).unwrap_err();
            assert!(
                format!("{err:#}").contains("`all` and `current`"),
                "{err:#}"
            );
        }
    }

    #[test]
    fn a_switch_defers_until_there_is_a_mirror_and_can_keep_the_sandbox() {
        let (_tmp, layout) = crate::commands::testsupport::project();
        let ctx = ctx_for(&layout);
        create(&ctx, "exp").unwrap();

        // No mirror yet: the rebuild has nowhere to apply against.
        switch(&ctx, DEFAULT_SERIES).unwrap();
        assert!(State::load(&layout).unwrap().sandbox(&layout).is_none());

        // With the sandbox policy asking to keep, a switch never rebuilds either.
        fs::write(
            layout.config_file(),
            "[upstream]\nurl = \"unused\"\n[sandbox]\non-switch = \"keep\"\n",
        )
        .unwrap();
        switch(&ctx, "exp").unwrap();
        assert!(State::load(&layout).unwrap().sandbox(&layout).is_none());
    }

    /// A series with `held` as its patch text, plus an undo entry holding `kept`.
    fn series_with_undo(layout: &Layout, held: &str, kept: &str) {
        write_series(layout, &[("a.patch", held)]);
        let undo = layout.undo_entry();
        fs::create_dir_all(&undo).unwrap();
        fs::write(undo.join("a.patch"), kept).unwrap();
        fs::write(undo.join("series"), "a.patch\n").unwrap();
    }

    #[test]
    fn undo_trades_the_series_with_the_version_rebase_kept() {
        let (_tmp, layout) = crate::commands::testsupport::project();
        series_with_undo(&layout, "rebased\n", "original\n");
        let ctx = ctx_for(&layout);

        let exit = undo(&ctx, &SeriesUndoArgs { yes: true }).unwrap();
        assert!(matches!(exit, Exit::Success));

        // What was live is now the undo entry, and vice versa: the trade, not a copy.
        assert_eq!(
            fs::read_to_string(layout.patches_dir().join("a.patch")).unwrap(),
            "original\n"
        );
        assert_eq!(
            fs::read_to_string(layout.undo_entry().join("a.patch")).unwrap(),
            "rebased\n"
        );

        // A second undo reverses the first: the two versions keep swapping.
        undo(&ctx, &SeriesUndoArgs { yes: true }).unwrap();
        assert_eq!(
            fs::read_to_string(layout.patches_dir().join("a.patch")).unwrap(),
            "rebased\n"
        );
        assert_eq!(
            fs::read_to_string(layout.undo_entry().join("a.patch")).unwrap(),
            "original\n"
        );
    }

    #[test]
    fn undo_with_nothing_to_put_back_says_so() {
        let (_tmp, layout) = crate::commands::testsupport::project();
        write_series(&layout, &[("a.patch", "a\n")]);
        let ctx = ctx_for(&layout);

        let err = undo(&ctx, &SeriesUndoArgs { yes: true }).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("nothing to undo"), "got {text}");
        assert!(
            layout.patches_dir().join("a.patch").is_file(),
            "nothing is touched when there is no undo entry"
        );
    }

    #[test]
    fn undo_without_yes_in_a_prompt_free_environment_is_told_how() {
        let (_tmp, layout) = crate::commands::testsupport::project();
        series_with_undo(&layout, "rebased\n", "original\n");
        let ctx = ctx_for(&layout);

        let err = undo(&ctx, &SeriesUndoArgs { yes: false })
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot ask"), "{err}");
        assert!(err.contains("--yes"), "{err}");
        assert_eq!(
            fs::read_to_string(layout.patches_dir().join("a.patch")).unwrap(),
            "rebased\n",
            "nothing swaps when the question cannot be asked"
        );
    }

    #[test]
    fn a_copy_is_complete_or_not_there_at_all() {
        // The copy builds in scratch and is renamed in, so a destination is either the
        // whole series or no series: never the two files that were reached before an
        // error stopped the loop.
        let (_tmp, layout) = crate::commands::testsupport::project();
        write_series(
            &layout,
            &[
                ("a.patch", crate::commands::testsupport::patch_one()),
                ("b.patch", crate::commands::testsupport::patch_two()),
            ],
        );
        let ctx = ctx_for(&layout);

        call(
            &ctx,
            Some(SeriesCommand::Copy(SeriesCopyArgs {
                name: "snapshot".to_owned(),
                from: None,
            })),
        )
        .unwrap();

        let copy = layout.with_series("snapshot").unwrap();
        assert_eq!(
            fs::read_to_string(copy.series_file()).unwrap(),
            "a.patch\nb.patch\n"
        );
        assert!(copy.patches_dir().join("a.patch").is_file());
        assert!(copy.patches_dir().join("b.patch").is_file());
        // Scratch was renamed away, not left beside the result.
        assert!(
            !layout.tmp_dir().join("snapshot.new").exists(),
            "the built copy must be renamed out of tmp"
        );
    }
}
