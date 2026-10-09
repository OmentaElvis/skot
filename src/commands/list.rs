use anyhow::Result;
use log::warn;

use crate::cli::ListArgs;
use crate::commands::{Ctx, Exit, emit_json};
use crate::report;

pub fn run(ctx: &Ctx, args: &ListArgs) -> Result<Exit> {
    let (_config, series) = ctx.project()?;

    if series.is_empty() {
        if crate::ui::is_json() {
            emit_json(&serde_json::json!({ "patches": [] }))?;
            return Ok(Exit::Success);
        }
        warn!(
            target: "list",
            "the series at {} lists no patches",
            series.path().display()
        );
        report::note("Add one with: skot new <name>");
        return Ok(Exit::Success);
    }

    let entries = series.entries_lenient(&ctx.layout);
    let missing: Vec<&str> = entries
        .iter()
        .filter(|e| !e.path.is_file())
        .map(|e| e.name.as_str())
        .collect();
    let applied = crate::state::State::load(&ctx.layout)
        .map(|s| s.applied)
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
                }))
                .collect::<Vec<_>>(),
            "missing": missing,
            "applied": applied,
        }))?;
        return Ok(if missing.is_empty() {
            Exit::Success
        } else {
            Exit::Failure
        });
    }

    report::section(&format!("series ({} patches)", entries.len()));
    for (i, entry) in entries.iter().enumerate() {
        // `[!]` (not on disk) outranks `[x]`: the pair `[x]`/`[ ]` is what makes a hole in
        // the applied set visible at a glance.
        let mark = if !entry.path.is_file() {
            report::bad("[!]").to_string()
        } else if applied.contains(&entry.name) {
            "[x]".to_string()
        } else {
            "[ ]".to_string()
        };
        report::item(&format!("{:>3}  {mark} {}", i + 1, entry.name));
        if args.paths && entry.path.is_file() {
            report::item(&format!(
                "      {}",
                report::muted(&entry.path.display().to_string())
            ));
        }
    }

    if !missing.is_empty() {
        warn!(target: "list", "{} patch file(s) listed but not on disk:", missing.len());
        for name in &missing {
            report::line(&report::bad(name));
        }
        return Ok(Exit::Failure);
    }
    Ok(Exit::Success)
}
