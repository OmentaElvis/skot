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
                }))
                .collect::<Vec<_>>(),
            "missing": missing,
        }))?;
        return Ok(if missing.is_empty() {
            Exit::Success
        } else {
            Exit::Failure
        });
    }

    report::section(&format!("series ({} patches)", entries.len()));
    for (i, entry) in entries.iter().enumerate() {
        let mark = if entry.path.is_file() { "  " } else { "! " };
        report::item(&format!("{mark}{:>3}  {}", i + 1, entry.name));
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
