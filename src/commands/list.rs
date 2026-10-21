use anyhow::Result;
use log::warn;

use crate::cli::ListArgs;
use crate::commands::{Ctx, Exit, emit_json};
use crate::invocation::skot_args;
use crate::report;
use crate::series::Entry;
use crate::session;

pub fn run(ctx: &Ctx, args: &ListArgs) -> Result<Exit> {
    let (_config, series) = ctx.project()?;
    let entries = series.entries_lenient(&ctx.layout);
    let disabled = series.disabled_lines();
    let unsaved = unsaved_work_areas(&ctx.layout);

    if entries.is_empty() && disabled.is_empty() {
        if crate::ui::is_json() {
            emit_json(&serde_json::json!({ "patches": [], "disabled": [] }))?;
            return Ok(Exit::Success);
        }
        warn!(
            target: "list",
            "the series at {} lists no patches",
            series.path().display()
        );
        report::note(&format!("Add one with: {}", skot_args("new", "<name>")));
        return Ok(Exit::Success);
    }

    let missing: Vec<&str> = entries
        .iter()
        .filter(|e| !e.path.is_file())
        .map(|e| e.name.as_str())
        .collect();
    let applied = crate::state::State::load(&ctx.layout)
        .map(|s| s.applied(&ctx.layout).to_vec())
        .unwrap_or_default();

    if ctx.cli.json {
        emit_json(&serde_json::json!({
            "series": series.path().display().to_string(),
            "patches": entries
                .iter()
                .map(|e| serde_json::json!({
                    "index": entries.iter().position(|x| x.name == e.name),
                    "name": e.name,
                    "line": e.line,
                    "path": e.path.display().to_string(),
                    "present": e.path.is_file(),
                    "applied": applied.contains(&e.name),
                    "unsaved": unsaved.contains(&e.name),
                }))
                .collect::<Vec<_>>(),
            "disabled": disabled
                .iter()
                .map(|(line, name)| serde_json::json!({ "name": name, "line": line }))
                .collect::<Vec<_>>(),
            "unsaved": unsaved,
            "missing": missing,
            "applied": applied,
        }))?;
        return Ok(if missing.is_empty() {
            Exit::Success
        } else {
            Exit::Failure
        });
    }

    let heading = if disabled.is_empty() {
        format!("series ({} patches)", entries.len())
    } else {
        format!(
            "series ({} patches, {} disabled)",
            entries.len(),
            disabled.len()
        )
    };
    report::section(&heading);

    // Enabled and disabled rows in one walk, in file order: the disabled line sits
    // where the file puts it, so the listing is the series file made readable.
    type Disabled<'a> = (usize, String);
    let mut rows: Vec<(usize, Option<&Entry>, Option<&Disabled<'_>>)> =
        entries.iter().map(|e| (e.line, Some(e), None)).collect();
    rows.extend(disabled.iter().map(|d| (d.0, None, Some(d))));
    rows.sort_by_key(|(line, _, _)| *line);
    let had_rows = !rows.is_empty();

    let mut position = 0usize;
    for (_, entry, off) in rows {
        if let Some(entry) = entry {
            position += 1;
            // `[!]` (not on disk) outranks `[x]`: the pair `[x]`/`[ ]` is what makes a hole
            // in the applied set visible at a glance. Only a patch the sandbox is holding
            // gets the green name; the rest are plain.
            let applied_now = entry.path.is_file() && applied.contains(&entry.name);
            let mark = if !entry.path.is_file() {
                report::bad("[!]").to_string()
            } else if applied_now {
                "[x]".to_string()
            } else {
                "[ ]".to_string()
            };
            let name = if applied_now {
                report::ok(&entry.name).to_string()
            } else {
                entry.name.clone()
            };
            // A work area with unsaved changes is work the patch file does not hold yet:
            // the name says so rather than making the author remember where they left it.
            let unsaved_suffix = if unsaved.contains(&entry.name) {
                report::muted(" (unsaved)").to_string()
            } else {
                String::new()
            };
            report::item(&format!("{position:>3}  {mark} {name}{unsaved_suffix}"));
            if args.paths && entry.path.is_file() {
                report::item(&format!(
                    "      {}",
                    report::muted(&entry.path.display().to_string())
                ));
            }
        } else if let Some((_, name)) = off {
            report::item(&report::muted(&format!("       # {name}")));
        }
    }

    if !missing.is_empty() {
        warn!(
            target: "list",
            "{} patch file(s) listed but not on disk:",
            missing.len()
        );
        for name in &missing {
            report::line(&report::bad(name));
        }
        return Ok(Exit::Failure);
    }
    if had_rows {
        report::section("key");
        report::field("[x]", "applied to the sandbox");
        report::field("[ ]", "not applied");
        report::field("#", "disabled");
        report::field("(unsaved)", "unsaved work-area changes");
    }
    Ok(Exit::Success)
}

/// The names of the patches whose work area holds changes the patch file does not, in
/// whatever order the manifest holds.
///
/// Read-only and lockless: a missing manifest or a work area that will not open means
/// nothing is unsaved, which is all a marker needs to know.
pub(crate) fn unsaved_work_areas(layout: &crate::layout::Layout) -> Vec<String> {
    session::Manifest::load(layout)
        .map(|m| {
            m.sessions
                .iter()
                .filter(|s| s.series_name() == layout.series())
                .filter(|s| session::unsaved(layout, s).unwrap_or(false))
                .map(|s| s.patch.clone())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Layout;

    #[test]
    fn unsaved_work_areas_are_the_manifest_sessions_holding_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path());

        assert!(
            unsaved_work_areas(&layout).is_empty(),
            "a missing manifest means nothing is unsaved"
        );

        session::Manifest {
            sessions: vec![session::Session {
                series: crate::layout::DEFAULT_SERIES.to_owned(),
                patch: "b.patch".to_owned(),
                dir: "b.patch".to_owned(),
                ref_name: "main".to_owned(),
                base_sha: "abc".to_owned(),
                base_tree: "def".to_owned(),
            }],
        }
        .save(&layout)
        .unwrap();

        // No work area on disk, so nothing is unsaved: the patch file holds everything.
        assert!(unsaved_work_areas(&layout).is_empty());
    }
}
