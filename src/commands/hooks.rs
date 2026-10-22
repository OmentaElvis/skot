use anyhow::Result;

use crate::commands::{Ctx, Exit, emit_json};
use crate::phase::HookPoint;
use crate::report;

/// Show every lifecycle slot with its command, and the named hooks, in the order they
/// would run. A slot with nothing in it is shown as such, so "not configured" cannot be
/// mistaken for "not a slot".
pub fn run(ctx: &Ctx) -> Result<Exit> {
    let config = ctx.config()?;

    if ctx.cli.json {
        emit_json(&serde_json::json!({
            "lifecycle": HookPoint::ALL
                .into_iter()
                .map(|point| serde_json::json!({
                    "point": point.toml_key(),
                    "name": point.key(),
                    "cmd": config.hook(point),
                }))
                .collect::<Vec<_>>(),
            "named": config.extra.iter().map(|h| serde_json::json!({
                "name": h.name,
                "cmd": h.cmd,
                "cwd": h.cwd,
                "required": h.required,
            })).collect::<Vec<_>>(),
        }))?;
        return Ok(Exit::Success);
    }

    report::section("lifecycle hooks");
    // The key shown is the one to write in skot.toml, so it can be copied.
    for point in HookPoint::ALL {
        match config.hook(point) {
            Some(cmd) => report::line(&format!("{:<12} {cmd}", point.toml_key())),
            None => report::line(&format!(
                "{:<12} {}",
                point.toml_key(),
                report::absent("(not configured)")
            )),
        }
    }
    if config.configured_hooks().is_empty() {
        report::note(&format!(
            "none configured. Add them under [hooks] in {}.",
            ctx.layout.config_file().display()
        ));
    }

    report::section("named hooks");
    if config.extra.is_empty() {
        report::note("none. Add them as [[hook]] entries.");
    } else {
        for hook in &config.extra {
            let flag = if hook.required { "" } else { "  (optional)" };
            report::line(&format!("{:<12} {}{flag}", hook.name, hook.cmd));
        }
    }
    Ok(Exit::Success)
}
