//! `patch show`: print a patch, organised.
//!
//! Three shapes, one content. A terminal gets a report head (what the patch is,
//! where it sits, whether the sandbox holds it, what it changes) above the patch
//! itself, with every structured part of the file coloured by its role: mail
//! headers, the diffstat, file headers, hunks, the two signs. `--json` gets
//! structured facts plus the patch text, and anything else gets the patch file's
//! exact bytes, so `skot patch show p > p.patch` round-trips and `| less` just
//! works.

use std::io::Write;

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::cli::ShowArgs;
use crate::commands::{Ctx, Exit, authoring};
use crate::invocation::skot;
use crate::session;
use crate::state::State;
use crate::ui::{Styled, stdout_is_terminal};

/// One patch, and the facts the report head and the JSON document both report.
struct Chosen {
    name: String,
    path: std::path::PathBuf,
    series: String,
    /// 1-based among the enabled patches; `None` for a disabled one.
    position: Option<usize>,
    total: usize,
    enabled: bool,
    applied: bool,
    work_area: Option<std::path::PathBuf>,
    unsaved: bool,
}

/// One file's counts inside a patch: the `diff --git` name, and what its hunks held.
#[derive(Debug, PartialEq, Eq)]
struct FileStat {
    path: String,
    added: usize,
    removed: usize,
}

pub fn run(ctx: &Ctx, args: &ShowArgs) -> Result<Exit> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    emit(ctx, args, &mut out, stdout_is_terminal())
}

/// The work function, and the whole contract: everything goes through `out`, and
/// `tty` is the caller's statement about the terminal, so tests check the piped
/// byte-exact path without pretending to be interactive.
fn emit(ctx: &Ctx, args: &ShowArgs, out: &mut dyn Write, tty: bool) -> Result<Exit> {
    let (_, series) = ctx.project()?;
    let entries = series.entries_lenient(&ctx.layout);
    let disabled = series.disabled_lines();
    let applied = State::load(&ctx.layout)
        .map(|s| s.applied(&ctx.layout).to_vec())
        .unwrap_or_default();

    let chosen = choose(ctx, args, &entries, &disabled, &applied)?;

    let mut documents = Vec::new();
    for (at, picked) in chosen.iter().enumerate() {
        if !picked.path.is_file() {
            bail!("patch listed but not on disk: {}", picked.name);
        }
        let bytes = std::fs::read(&picked.path)
            .with_context(|| format!("reading {}", picked.path.display()))?;
        let text = String::from_utf8(bytes)
            .with_context(|| format!("{} is not valid UTF-8", picked.name))?;
        let stats = file_stats(&text);

        if ctx.cli.json {
            documents.push(json!({
                "name": picked.name,
                // The series is the document's own business, not each patch's; the
                // position is 1-based the way every other command counts.
                "position": picked.position,
                "enabled": picked.enabled,
                "applied": picked.applied,
                "work_area": picked.work_area.as_ref().map(|p| p.display().to_string()),
                "unsaved": picked.unsaved,
                "files": stats
                    .iter()
                    .map(|s| json!({ "path": s.path, "added": s.added, "removed": s.removed }))
                    .collect::<Vec<_>>(),
                "added": stats.iter().map(|s| s.added).sum::<usize>(),
                "removed": stats.iter().map(|s| s.removed).sum::<usize>(),
                "text": text,
            }));
            continue;
        }

        if !tty {
            if args.stat {
                writeln!(out, "{}  {}", picked.name, summary(&stats))?;
                for file in &stats {
                    writeln!(out, " {} +{} -{}", file.path, file.added, file.removed)?;
                }
            } else {
                out.write_all(text.as_bytes())?;
            }
            continue;
        }

        if at > 0 {
            writeln!(out)?;
        }
        preview(picked, &stats, &text, args.stat, out)?;
    }

    if ctx.cli.json {
        writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&json!({
                // The same word the other inspect commands use for the series name, so
                // a script reads one meaning out of every document it holds.
                "series_name": ctx.layout.series(),
                "patches": documents,
            }))?
        )?;
    }
    Ok(Exit::Success)
}

/// The patch each argument means, or -- with no arguments -- the one open work area.
fn choose(
    ctx: &Ctx,
    args: &ShowArgs,
    entries: &[crate::series::Entry],
    disabled: &[(usize, String)],
    applied: &[String],
) -> Result<Vec<Chosen>> {
    if !args.patches.is_empty() {
        let all: Vec<&str> = entries
            .iter()
            .map(|e| e.name.as_str())
            .chain(disabled.iter().map(|(_, n)| n.as_str()))
            .collect();
        let enabled: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        return args
            .patches
            .iter()
            .map(|want| {
                let name = authoring::resolve_patch(&all, &enabled, want)?.name(&enabled);
                one(ctx, &name, entries, disabled, applied)
            })
            .collect();
    }
    let open: Vec<String> = session::Manifest::load(&ctx.layout)
        .map(|m| {
            m.sessions
                .iter()
                .filter(|s| s.series_name() == ctx.layout.series())
                .map(|s| s.patch.clone())
                .collect()
        })
        .unwrap_or_default();
    match open.as_slice() {
        [] => bail!(
            "no work area is open: name the patch to show (see {})",
            skot("list")
        ),
        [only] => one(ctx, only, entries, disabled, applied).map(|c| vec![c]),
        many => bail!(
            "several work areas are open: {}.\nName the patch to show.",
            many.join(", ")
        ),
    }
}

/// The patch's file, and everything the report head says about it.
fn one(
    ctx: &Ctx,
    name: &str,
    entries: &[crate::series::Entry],
    disabled: &[(usize, String)],
    applied: &[String],
) -> Result<Chosen> {
    let work = session::Manifest::load(&ctx.layout).ok().and_then(|m| {
        m.sessions
            .into_iter()
            .find(|s| s.series_name() == ctx.layout.series() && s.patch == name)
    });
    let work_area = work.as_ref().and_then(|s| s.dir_path(&ctx.layout).ok());
    let unsaved = work
        .as_ref()
        .is_some_and(|s| session::unsaved(&ctx.layout, s).unwrap_or(false));

    if let Some(position) = entries.iter().position(|e| e.name == name) {
        let entry = &entries[position];
        return Ok(Chosen {
            name: entry.name.clone(),
            path: entry.path.clone(),
            series: ctx.layout.series().to_owned(),
            position: Some(position + 1),
            total: entries.len(),
            enabled: true,
            applied: applied.contains(&entry.name),
            work_area,
            unsaved,
        });
    }
    if let Some((_, _)) = disabled.iter().find(|(_, n)| n == name) {
        return Ok(Chosen {
            name: name.to_owned(),
            path: ctx.layout.patches_dir().join(name),
            series: ctx.layout.series().to_owned(),
            position: None,
            total: entries.len(),
            enabled: false,
            applied: false,
            work_area,
            unsaved,
        });
    }
    bail!(
        "`{}` is not in the series: {}.\nSee {}.",
        name,
        entries
            .iter()
            .map(|e| e.name.as_str())
            .chain(disabled.iter().map(|(_, n)| n.as_str()))
            .collect::<Vec<_>>()
            .join(", "),
        skot("list")
    )
}

/// Per-file counts, in patch order: the b-side name of each `diff --git`, and the
/// body lines behind it. Header lines (`+++`/`---`, `@@`, the mail framing) are not
/// counts, however much they look like them.
fn file_stats(text: &str) -> Vec<FileStat> {
    let mut files: Vec<FileStat> = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            // `diff --git a/x b/x`: the b-side name is the file even when the delta
            // deletes it and `+++` says `/dev/null`. Read through the parser that knows
            // the quoted form, because a path with a space is written that way.
            let path = crate::git::binary::diff_b_path(line).unwrap_or_else(|_| {
                let last = rest.rsplit(' ').next().unwrap_or(rest);
                last.strip_prefix("b/").unwrap_or(last).to_owned()
            });
            files.push(FileStat {
                path,
                added: 0,
                removed: 0,
            });
            continue;
        }
        let Some(current) = files.last_mut() else {
            continue;
        };
        if line.starts_with('+') && !line.starts_with("+++") {
            current.added += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            current.removed += 1;
        }
    }
    files
}

/// The report head and, unless `--stat`, the patch below it, on a terminal.
fn preview(
    chosen: &Chosen,
    stats: &[FileStat],
    text: &str,
    stat: bool,
    out: &mut dyn Write,
) -> Result<()> {
    head(chosen, stats, text, out)?;
    if !stat && !text.is_empty() {
        writeln!(out)?;
        body(text, out)?;
    }
    Ok(())
}

/// What the patch is and where it sits, in words rather than marks: the answer to
/// "what am I looking at" before the file's own bytes begin.
fn head(chosen: &Chosen, stats: &[FileStat], text: &str, out: &mut dyn Write) -> Result<()> {
    let dim = Styled::new().dim();
    let bold = Styled::new().bold();
    let green = Styled::new().green();
    let yellow = Styled::new().yellow();

    let title = if chosen.applied {
        green.render(&chosen.name).to_string()
    } else {
        bold.render(&chosen.name).to_string()
    };
    writeln!(out, "{title}")?;

    let place = match chosen.position {
        Some(position) => format!("{} ({} of {})", chosen.series, position, chosen.total),
        None => format!("{} (disabled)", chosen.series),
    };
    let state = if !chosen.enabled {
        "disabled".to_owned()
    } else if chosen.applied {
        "enabled, applied to the sandbox".to_owned()
    } else {
        "enabled, not applied".to_owned()
    };
    row("series", &place, out)?;
    row("state", &state, out)?;
    if let Some(dir) = &chosen.work_area {
        let note = if chosen.unsaved {
            format!(" {}", yellow.render("(unsaved)"))
        } else {
            String::new()
        };
        row("work area", &format!("{}{note}", dir.display()), out)?;
    }
    if text.is_empty() {
        row("files", &dim.render("empty patch").to_string(), out)?;
    } else {
        row("files", &summary_spans(stats), out)?;
        for file in stats {
            let mut counts = Vec::new();
            if file.added > 0 {
                counts.push(green.render(format!("+{}", file.added)).to_string());
            }
            if file.removed > 0 {
                counts.push(red().render(format!("-{}", file.removed)).to_string());
            }
            if counts.is_empty() {
                counts.push(dim.render("(no line changes)").to_string());
            }
            writeln!(out, "    {} {}", file.path, counts.join(" "))?;
        }
    }
    Ok(())
}

/// One aligned report row: dim key, then the value in whatever colour it earned.
fn row(key: &str, value: &str, out: &mut dyn Write) -> Result<()> {
    writeln!(
        out,
        "  {} {value}",
        Styled::new().dim().render(format!("{key:<10}"))
    )?;
    Ok(())
}

fn red() -> Styled {
    Styled::new().red()
}

/// The counts line, with each side in its own colour so the shape of the change
/// reads before the words do.
fn summary_spans(stats: &[FileStat]) -> String {
    let plural = |n: usize, one: &'static str, many: &'static str| {
        if n == 1 { one } else { many }
    };
    let green = Styled::new().green();
    let red = red();
    let added: usize = stats.iter().map(|s| s.added).sum();
    let removed: usize = stats.iter().map(|s| s.removed).sum();
    let mut parts = vec![format!(
        "{} {} changed",
        stats.len(),
        plural(stats.len(), "file", "files")
    )];
    if added > 0 {
        parts.push(
            green
                .render(format!("{added} insertion{}(+)", plural(added, "", "s")))
                .to_string(),
        );
    }
    if removed > 0 {
        parts.push(
            red.render(format!("{removed} deletion{}(-)", plural(removed, "", "s")))
                .to_string(),
        );
    }
    let sep = Styled::new().dim().render(", ").to_string();
    parts.join(&sep)
}

/// The patch body, walked in the zones a patch actually has: the mail header, the
/// diffstat behind its `---`, then the diff itself. Every zone has its own colours,
/// so the structure is visible without reading a word.
fn body(text: &str, out: &mut dyn Write) -> Result<()> {
    let dim = Styled::new().dim();
    let bold = Styled::new().bold();
    let cyan = Styled::new().cyan();
    let blue = Styled::new().bold().blue();
    let green = Styled::new().green();
    let red = red();
    let yellow = Styled::new().yellow();

    #[derive(PartialEq)]
    enum Zone {
        Mail,
        Stat,
        Diff,
    }
    let mut zone = Zone::Mail;
    // The header block ends at the first blank line, per the mail format a patch
    // is written in. Everything after that blank is the commit message, where a
    // colon is just punctuation.
    let mut in_headers = true;
    let (mut old, mut new) = (0usize, 0usize);
    let mut in_hunk = false;

    for line in text.lines() {
        if line.starts_with("diff --git") {
            zone = Zone::Diff;
            in_hunk = false;
            writeln!(out, "{}", cyan.render(line))?;
            continue;
        }
        match zone {
            Zone::Mail => {
                if line.trim_end() == "---" {
                    zone = Zone::Stat;
                    writeln!(out, "{}", dim.render(line))?;
                } else if let Some((key, value)) = mail_field(line, in_headers) {
                    // The label is the structure, and Subject is the patch's own
                    // title, so it gets the weight.
                    let label = cyan.render(format!("{key}:")).to_string();
                    if key == "Subject" {
                        writeln!(out, "{label} {}", bold.render(value))?;
                    } else {
                        writeln!(out, "{label} {value}")?;
                    }
                } else if line.is_empty() {
                    in_headers = false;
                    writeln!(out)?;
                } else if in_headers {
                    // The `From <sha> Mon Sep...` envelope, a folded continuation,
                    // or a header this renderer does not model.
                    writeln!(out, "{}", dim.render(line))?;
                } else {
                    // The commit message: prose, whatever it is punctuated with.
                    writeln!(out, "{line}")?;
                }
            }
            Zone::Stat => {
                if let Some((path, rest)) = line.trim_start().split_once(" | ") {
                    writeln!(
                        out,
                        "  {} {} {}",
                        bold.render(path),
                        dim.render("|"),
                        bar_spans(rest)
                    )?;
                } else if line.trim_start().contains(" file changed")
                    || line.trim_start().contains(" files changed")
                {
                    writeln!(out, "  {}", summary_text(line.trim_start()))?;
                } else {
                    writeln!(out, "{}", dim.render(line))?;
                }
            }
            Zone::Diff => {
                if line.starts_with("@@") {
                    if let Some((o, n)) = hunk_starts(line) {
                        old = o;
                        new = n;
                        in_hunk = true;
                    }
                    writeln!(out, "{}", blue.render(line))?;
                    continue;
                }
                if !in_hunk {
                    // The mode and index lines, and the two file paths.
                    if line.starts_with("--- ") || line.starts_with("+++ ") {
                        writeln!(out, "{}", yellow.render(line))?;
                    } else {
                        writeln!(out, "{}", dim.render(line))?;
                    }
                    continue;
                }
                if let Some(rest) = line.strip_prefix('+') {
                    let gutter = format!("{:>4} {:>4} \u{2502} ", "", new);
                    new += 1;
                    writeln!(
                        out,
                        "{}{}",
                        dim.render(gutter),
                        green.render(format!("+{rest}"))
                    )?;
                } else if let Some(rest) = line.strip_prefix('-') {
                    let gutter = format!("{:>4} {:>4} \u{2502} ", old, "");
                    old += 1;
                    writeln!(
                        out,
                        "{}{}",
                        dim.render(gutter),
                        red.render(format!("-{rest}"))
                    )?;
                } else if line.starts_with('\\') {
                    writeln!(out, "{}", yellow.render(line))?;
                } else {
                    let gutter = format!("{:>4} {:>4} \u{2502} ", old, new);
                    old += 1;
                    new += 1;
                    writeln!(out, "{}{line}", dim.render(gutter))?;
                }
            }
        }
    }
    Ok(())
}

/// A diffstat bar, `12 ++++----`: the count dim, then the runs in the two colours.
fn bar_spans(rest: &str) -> String {
    let dim = Styled::new().dim();
    let green = Styled::new().green();
    let red = red();
    let (count, bar) = match rest.split_once(' ') {
        Some((count, bar)) => (format!("{count} "), bar.to_owned()),
        None => (String::new(), rest.to_owned()),
    };
    let mut spans = dim.render(count).to_string();
    let mut run = String::new();
    let mut kind = '\0';
    for c in bar.chars() {
        let same = c == kind;
        if !same && !run.is_empty() {
            spans.push_str(&render_run(kind, &run, &dim, &green, &red));
            run.clear();
        }
        kind = c;
        run.push(c);
    }
    if !run.is_empty() {
        spans.push_str(&render_run(kind, &run, &dim, &green, &red));
    }
    spans
}

fn render_run(kind: char, run: &str, dim: &Styled, green: &Styled, red: &Styled) -> String {
    match kind {
        '+' => green.render(run).to_string(),
        '-' => red.render(run).to_string(),
        _ => dim.render(run).to_string(),
    }
}

/// The diffstat's own totals line, coloured the same way skot's head counts are.
fn summary_text(text: &str) -> String {
    let green = Styled::new().green();
    let red = red();
    text.split(", ")
        .map(|part| {
            if part.contains("insertion") {
                green.render(part).to_string()
            } else if part.contains("deletion") {
                red.render(part).to_string()
            } else {
                part.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(&Styled::new().dim().render(", ").to_string())
}

/// A mail header field, and only while the header block lasts: the first blank
/// line ends that block, and a name with a space in it was never a header.
fn mail_field(line: &str, in_headers: bool) -> Option<(&str, &str)> {
    if !in_headers {
        return None;
    }
    let (key, value) = line.split_once(": ")?;
    if key.is_empty()
        || !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    Some((key, value))
}

/// The numbers a `@@ -a,b +c,d @@` line starts the hunk at.
fn hunk_starts(line: &str) -> Option<(usize, usize)> {
    let mut parts = line.split_whitespace().skip(1);
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let old = old.split(',').next()?.parse().ok()?;
    let new = new.split(',').next()?.parse().ok()?;
    Some((old, new))
}

/// `1 file changed, 2 insertions(+), 3 deletions(-)`, omitting the sides that are
/// zero. The plain form: pipes and logs get this, terminals get the coloured one.
fn summary(stats: &[FileStat]) -> String {
    let plural = |n: usize, one: &'static str, many: &'static str| {
        if n == 1 { one } else { many }
    };
    let added: usize = stats.iter().map(|s| s.added).sum();
    let removed: usize = stats.iter().map(|s| s.removed).sum();
    let mut parts = Vec::new();
    if added > 0 {
        parts.push(format!("{added} insertion{}(+)", plural(added, "", "s")));
    }
    if removed > 0 {
        parts.push(format!("{removed} deletion{}(-)", plural(removed, "", "s")));
    }
    let what = format!("{} {}", stats.len(), plural(stats.len(), "file", "files"));
    if parts.is_empty() {
        format!("{what} changed")
    } else {
        format!("{what} changed, {}", parts.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use crate::commands::testsupport;
    use crate::series::Series;
    use clap::Parser as _;

    const PATCH: &str = "diff --git a/f.txt b/f.txt\nindex 1111111..2222222 100644\n--- a/f.txt\n+++ b/f.txt\n@@ -1,1 +1,1 @@\n-base\n+first\n";

    fn two_file_patch() -> &'static str {
        "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n one\n-two\n+twain\n+two point five\nthree\ndiff --git a/b.txt b/b.txt\n--- a/b.txt\n+++ b/b.txt\n@@ -1,0 +1,1 @@\n+hello\n"
    }

    fn ctx_on(layout: crate::layout::Layout, json: bool) -> Ctx {
        let flag: &[&str] = if json {
            &["skot", "--json", "patch", "show"]
        } else {
            &["skot", "patch", "show"]
        };
        Ctx {
            cli: Cli::try_parse_from(flag).unwrap(),
            layout,
            yes: false,
        }
    }

    fn args(names: &[&str]) -> ShowArgs {
        ShowArgs {
            patches: names.iter().map(|n| n.to_string()).collect(),
            stat: false,
        }
    }

    fn series_of(layout: &crate::layout::Layout) -> Series {
        let (_config, series) = layout.project().unwrap();
        series
    }

    #[test]
    fn stats_count_the_body_lines_of_each_file() {
        let stats = file_stats(two_file_patch());
        assert_eq!(
            stats,
            vec![
                FileStat {
                    path: "a.txt".to_owned(),
                    added: 2,
                    removed: 1
                },
                FileStat {
                    path: "b.txt".to_owned(),
                    added: 1,
                    removed: 0
                },
            ]
        );
    }

    #[test]
    fn stats_read_a_quoted_path_whole() {
        let quoted = "diff --git \"a/my logo.bin\" \"b/my logo.bin\"\nGIT binary patch\n";
        assert_eq!(
            file_stats(quoted),
            vec![FileStat {
                path: "my logo.bin".to_owned(),
                added: 0,
                removed: 0
            }]
        );
    }

    #[test]
    fn stats_name_the_b_side_even_when_a_file_is_deleted() {
        let deleted = "diff --git a/gone.txt b/gone.txt\n--- a/gone.txt\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-bye\n";
        assert_eq!(
            file_stats(deleted),
            vec![FileStat {
                path: "gone.txt".to_owned(),
                added: 0,
                removed: 1
            }]
        );
    }

    #[test]
    fn hunk_headers_set_both_sides_of_the_gutter() {
        assert_eq!(hunk_starts("@@ -1,2 +1,3 @@ ctx"), Some((1, 1)));
        assert_eq!(hunk_starts("@@ -0,0 +1,2 @@ new file"), Some((0, 1)));
        assert_eq!(hunk_starts("@@ -7 +7,0 @@ deleted"), Some((7, 7)));
        assert_eq!(hunk_starts("not a hunk"), None);
    }

    #[test]
    fn a_colon_in_the_commit_message_is_not_a_header_field() {
        // The header block ends at the first blank line; after it, `Fixes: #12` is
        // prose in the commit message, not a field.
        assert_eq!(
            mail_field("Subject: [PATCH] a", true),
            Some(("Subject", "[PATCH] a"))
        );
        assert_eq!(
            mail_field("X-Skot-Import: refs/pull/1", true),
            Some(("X-Skot-Import", "refs/pull/1"))
        );
        assert_eq!(mail_field("Fixes: #12", false), None);
        assert_eq!(mail_field("Note: mind the gap", false), None);
        assert_eq!(mail_field("a note: with a space in the name", true), None);
        assert_eq!(mail_field("From 1792528017 Mon Sep 17", true), None);
    }

    #[test]
    fn piped_output_is_the_patch_bytes_exactly() {
        let (_tmp, layout) = testsupport::project();
        std::fs::write(layout.patches_dir().join("a.patch"), PATCH).unwrap();
        std::fs::write(layout.series_file(), "a.patch\n").unwrap();

        let mut out = Vec::new();
        let exit = emit(&ctx_on(layout, false), &args(&["a.patch"]), &mut out, false).unwrap();
        assert_eq!(exit, Exit::Success);
        assert_eq!(String::from_utf8(out).unwrap(), PATCH);
    }

    #[test]
    fn json_carries_the_text_the_counts_and_the_state() {
        let (_tmp, layout) = testsupport::project();
        std::fs::write(layout.patches_dir().join("a.patch"), PATCH).unwrap();
        std::fs::write(layout.series_file(), "a.patch\n").unwrap();

        let mut out = Vec::new();
        emit(&ctx_on(layout, true), &args(&["a.patch"]), &mut out, false).unwrap();
        let doc: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let patch = &doc["patches"][0];
        assert_eq!(patch["name"], "a.patch");
        assert_eq!(doc["series_name"], "default");
        assert_eq!(patch["text"], PATCH);
        assert_eq!(patch["added"], 1);
        assert_eq!(patch["removed"], 1);
        assert_eq!(patch["files"][0]["path"], "f.txt");
        assert_eq!(patch["enabled"], true);
        assert_eq!(patch["applied"], false);
        assert_eq!(patch["position"], 1);
    }

    #[test]
    fn the_open_work_area_is_the_target_when_none_is_named() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, _sha) = testsupport::mirror_with_one_commit(tmp.path());
        std::fs::write(root.patches_dir().join("a.patch"), PATCH).unwrap();
        std::fs::write(root.series_file(), "a.patch\n").unwrap();
        let entries = series_of(&root).entries_lenient(&root);
        let plan = session::plan(&root, "main", &[], Some(&entries[0])).unwrap();
        session::materialise(&root, "a.patch", &plan).unwrap();

        let mut out = Vec::new();
        emit(&ctx_on(root, false), &args(&[]), &mut out, false).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), PATCH);
    }

    #[test]
    fn several_open_work_areas_are_named_in_the_refusal() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, _sha) = testsupport::mirror_with_one_commit(tmp.path());
        for name in ["a.patch", "b.patch"] {
            std::fs::write(root.patches_dir().join(name), PATCH).unwrap();
        }
        std::fs::write(root.series_file(), "a.patch\nb.patch\n").unwrap();
        let entries = series_of(&root).entries_lenient(&root);
        for entry in &entries {
            let plan = session::plan(&root, "main", &entries[..0], Some(entry)).unwrap();
            session::materialise(&root, &entry.name, &plan).unwrap();
        }

        let err = emit(&ctx_on(root, false), &args(&[]), &mut Vec::new(), false).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("a.patch"), "{message}");
        assert!(message.contains("b.patch"), "{message}");
    }

    #[test]
    fn no_open_work_area_and_no_name_is_refused() {
        let (_tmp, layout) = testsupport::project();
        std::fs::write(layout.patches_dir().join("a.patch"), PATCH).unwrap();
        std::fs::write(layout.series_file(), "a.patch\n").unwrap();

        let err = emit(&ctx_on(layout, false), &args(&[]), &mut Vec::new(), false).unwrap_err();
        assert!(format!("{err:#}").contains("name the patch"));
    }

    #[test]
    fn a_disabled_patch_can_be_shown() {
        let (_tmp, layout) = testsupport::project();
        std::fs::write(layout.patches_dir().join("a.patch"), PATCH).unwrap();
        std::fs::write(layout.series_file(), "#!a.patch\n").unwrap();

        let mut out = Vec::new();
        let exit = emit(&ctx_on(layout, false), &args(&["a.patch"]), &mut out, false).unwrap();
        assert_eq!(exit, Exit::Success);
        assert_eq!(String::from_utf8(out).unwrap(), PATCH);
    }
}
