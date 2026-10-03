use anyhow::Result;

use crate::commands::{Ctx, Exit, emit_json};
use crate::report;

/// Show the hooks that are configured, in the order they would run.
pub fn run(ctx: &Ctx) -> Result<Exit> {
    let config = ctx.config()?;

    if ctx.cli.json {
        emit_json(&serde_json::json!({
            "lifecycle": config
                .configured_hooks()
                .into_iter()
                .map(|(point, cmd)| serde_json::json!({
                    "point": point.toml_key(),
                    "name": point.key(),
                    "cmd": cmd,
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

    let configured = config.configured_hooks();

    report::section("lifecycle hooks");
    if configured.is_empty() {
        report::note(&format!(
            "none. Add them under [hooks] in {}.",
            ctx.layout.config_file().display()
        ));
    } else {
        // The key shown is the one to write in skot.toml, so it can be copied.
        for (point, cmd) in configured {
            report::line(&format!("{:<12} {cmd}", point.toml_key()));
        }
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
