use anyhow::Result;
use log::warn;

use crate::cli::StatusArgs;
use crate::commands::{Ctx, Exit, emit_json};
use crate::git;
use crate::invocation::{skot, skot_args};
use crate::layout::{self, Layout};
use crate::report;
use crate::state::State;

pub fn run(ctx: &Ctx, _args: &StatusArgs) -> Result<Exit> {
    let layout = &ctx.layout;
    let state = State::load(layout)?;
    let series = crate::series::Series::load(&layout.series_file()).ok();
    let config = ctx.config().ok();

    if ctx.cli.json {
        emit_json(&serde_json::json!({
            "series_name": layout.series(),
            "root": layout.root().display().to_string(),
            "mirror": path_state(&layout.mirror_dir()),
            "sandbox": path_state(&layout.sandbox_dir()),
            "patch_count": series.as_ref().map_or(0, |s| s.len()),
            "missing_patches": series.as_ref().map_or_else(Vec::new, |s| {
                s.entries_lenient(layout)
                    .into_iter()
                    .filter(|e| !e.path.is_file())
                    .map(|e| e.name)
                    .collect::<Vec<_>>()
            }),
            "ref": state.ref_name(layout),
            "sha": state.sha(layout),
            "applied": state.applied(layout),
            "in_flight": state.in_flight.map(|p| p.name().to_owned()),
        }))?;
        return Ok(Exit::Success);
    }

    // An interrupted run is the single most important thing to say, so it comes first.
    if let Some(message) = state.interrupted() {
        warn!(target: "status", "{message}");
        report::line("Re-run the same command; skot resets the sandbox before it starts.");
        report::line(&format!(
            "`{}` discards the sandbox entirely if you would rather start over.",
            skot("clean")
        ));
    }

    layout::print_inventory(layout);
    print_layout(layout);
    print_series(layout, series.as_ref(), state.applied(layout));
    print_mirror(layout);
    print_sandbox(layout, series.as_ref(), &state);
    print_config(config.as_ref(), layout);

    Ok(Exit::Success)
}

fn path_state(path: &std::path::Path) -> serde_json::Value {
    if path.exists() {
        serde_json::json!({ "exists": true, "size": layout::human_bytes(layout::dir_size(path)) })
    } else {
        serde_json::json!({ "exists": false })
    }
}

fn print_layout(layout: &Layout) {
    report::section("paths");
    report::field("root", &layout.root().display().to_string());
    report::field("config", &layout.config_file().display().to_string());
    report::field("series", &layout.series_file().display().to_string());
    report::field("patches", &layout.patches_dir().display().to_string());
    report::field("state", &layout.state_dir().display().to_string());
}

fn print_series(layout: &Layout, series: Option<&crate::series::Series>, applied: &[String]) {
    let Some(series) = series else {
        report::section("series");
        report::note(&format!(
            "{} Create one with `{}`.",
            report::absent("absent."),
            skot("init")
        ));
        return;
    };
    let entries = series.entries_lenient(layout);
    // The name and base, so the answer to "what am I on, and where does it sit" is on
    // the same screen as the patches. The base is the series' own record, not the
    // sandbox's ref: those can differ, and both answers matter.
    report::section(&format!("series {} ({})", layout.series(), entries.len()));
    report::field(
        "base",
        layout.base_ref().as_deref().unwrap_or(
            // Not an error: a fresh project has no base until something records one.
            "(not recorded: building records it)",
        ),
    );
    for (i, entry) in entries.iter().enumerate() {
        // `!` (not on disk) outranks `[x]`: a patch cannot be on if its file is gone. The
        // `[x]`/`[ ]` pair is what makes a hole visible: the count in the sandbox section
        // says how many are on, this says which.
        let mark = if !entry.path.is_file() {
            report::bad("[!]").to_string()
        } else if applied.contains(&entry.name) {
            "[x]".to_string()
        } else {
            "[ ]".to_string()
        };
        report::item(&format!("{:>3}  {mark} {}", i + 1, entry.name));
    }
}

fn print_mirror(layout: &Layout) {
    report::section("mirror");
    let path = layout.mirror_dir();
    if !path.join("HEAD").is_file() {
        report::note(&format!(
            "{} `{}` creates it.",
            report::absent("absent."),
            skot("sync")
        ));
        return;
    }
    report::field(
        "path",
        &format!(
            "{}  {}",
            path.display(),
            layout::human_bytes(layout::dir_size(&path))
        ),
    );
    match git::mirror::Mirror::open(layout) {
        Ok(mirror) => match mirror.refs() {
            Ok(refs) => {
                report::field(
                    "refs",
                    &format!("{} branches, {} tags", refs.branches.len(), refs.tags.len()),
                );
                if let Some(latest) = mirror.version_tags().ok().and_then(|t| t.last().cloned()) {
                    report::field("latest tag", &latest);
                }
            }
            Err(e) => report::field("refs", &report::bad(&format!("unreadable: {e}"))),
        },
        Err(e) => report::field("path", &report::bad(&format!("unreadable: {e}"))),
    }
}

fn print_sandbox(layout: &Layout, series: Option<&crate::series::Series>, state: &State) {
    report::section("sandbox");
    let path = layout.sandbox_dir();
    if !path.exists() {
        report::note(&format!(
            "{} `{}` creates it.",
            report::absent("absent."),
            skot("apply")
        ));
        return;
    }
    report::field(
        "path",
        &format!(
            "{}  {}",
            path.display(),
            layout::human_bytes(git::sandbox::size(layout))
        ),
    );

    // A working sandbox is *detached*, by design: a reset must not move a branch in the
    // mirror. So "no ref" is the normal healthy state and not a problem to report. What
    // matters is the commit it is on, and whether the series is currently in it.
    match git::sandbox::open(layout) {
        Ok(repo) => match git::sandbox::head_sha(&repo) {
            Ok(sha) => report::field("head", &report::ok(&format!("detached at {sha}"))),
            Err(e) => report::field("head", &report::bad(&format!("unreadable: {e}"))),
        },
        Err(_) => report::note(&format!(
            "{} `{}` then `{}`.",
            report::bad("not a skot worktree."),
            skot_args("clean", "sandbox"),
            skot("apply")
        )),
    }

    let count = state.applied(layout).len();
    let total = series.map_or(0, |s| s.len());
    report::field(
        "applied",
        &match count {
            0 => report::absent("none"),
            n if total > 0 => report::ok(&format!("{n} of {total} patches")),
            n => report::ok(&format!("{n} patch(es)")),
        },
    );
    // A hole is a deliberate state, but it is invisible from the count alone, so the
    // patches that are off are named right here rather than only in the series listing.
    if count > 0
        && let Some(series) = series
    {
        let entries = series.entries_lenient(layout);
        let disabled: Vec<&str> = entries
            .iter()
            .filter(|e| !state.applied(layout).contains(&e.name))
            .map(|e| e.name.as_str())
            .collect();
        if !disabled.is_empty() {
            report::note(&format!("disabled: {}", disabled.join(", ")));
        }
    }
}

fn print_config(config: Option<&crate::config::Config>, layout: &Layout) {
    report::section("config");
    let Some(config) = config else {
        report::note(&format!("absent at {}", layout.config_file().display()));
        return;
    };
    match &config.upstream.url {
        Some(url) => report::field("upstream", url),
        None => report::field("upstream", &report::absent("not set")),
    }
    report::field("policy", &config.sandbox.policy.to_string());
    // No keep globs is the normal shape for a project with nothing to protect, not a
    // problem to warn about: an untracked cache already survives a reset on its own.
    if !config.sandbox.keep.is_empty() {
        report::field("keep", &config.sandbox.keep.join(" "));
    }
    let hooks = config.configured_hooks();
    if hooks.is_empty() {
        report::note("no hooks configured, so a build does nothing but apply the series");
    } else {
        for (point, _) in hooks {
            report::field("hook", point.toml_key());
        }
    }
}
