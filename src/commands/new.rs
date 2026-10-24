//! `patch new`: start the next patch, with a work area open on it.
//!
//! The authoring loop is one command per step: `new` gives you an empty patch registered at
//! its position in the series (`--at` picks the position, the end by default) and a worktree
//! of its own under `.skot/work/` holding everything below it, ready to edit. `save`
//! turns what you changed in there into that patch's contents. Nothing is committed anywhere:
//! the patch file *is* the record.
//!
//! Two things are worth stating, because both are easy to get wrong:
//!
//! - **The series is replayed before anything is written.** Resolving the base and checking
//!   that the patches below still apply to it happens first, so a failure leaves the series,
//!   the patches, and the work area exactly as they were: there is no half-created patch for
//!   someone to discover later. It also means the work area opens on a tree the series
//!   actually produces, not on whatever happened to be checked out.
//! - **It does not invent the patch's contents.** The file is written empty and registered, so
//!   that `save` has something to fill in and the series is complete before editing
//!   starts. An empty patch in the series is honest; a patch with a guessed subject and an
//!   empty diff is not.
//!
//! The shared run state stays out of it: `new` does not touch the sandbox or the applied set,
//! because those describe what has been *applied*, and starting a patch applies nothing. An
//! empty series is fine here: the first patch in a project is created against a bare upstream,
//! and `skot check` has nothing to complain about until the patch has content.

use anyhow::{Context, Result, bail};
use log::{debug, info};
use serde_json::json;

use crate::cli::NewArgs;
use crate::commands::{Ctx, Exit, authoring, emit_json};
use crate::hints::Hints;
use crate::invocation::{skot, skot_args};
use crate::layout::Layout;
use crate::series;
use crate::session;
use crate::state::State;

/// What `new` created, presented by [`run`].
#[derive(Debug)]
struct Created {
    name: String,
    path: std::path::PathBuf,
    /// The worktree the edits happen in.
    work_area: std::path::PathBuf,
    base: String,
    line: usize,
    subject: String,
}

/// Create an empty patch file, insert it into the series, and open a work area on it.
pub fn run(ctx: &Ctx, args: &NewArgs) -> Result<Exit> {
    let created = create(
        &ctx.layout,
        &args.name,
        args.base.as_deref(),
        args.at.as_deref(),
    )?;

    if ctx.cli.json {
        emit_json(&json!({
            "series_name": ctx.layout.series(),
            "patch": created.name,
            "path": created.path.display().to_string(),
            "work_area": created.work_area.display().to_string(),
            "base": created.base,
            "series_line": created.line,
            "subject": created.subject,
        }))?;
        return Ok(Exit::Success);
    }

    info!(target: "new", "created {}", created.name);
    debug!(target: "new", "{}", created.path.display());
    debug!(target: "new", "work area {}", created.work_area.display());
    debug!(
        target: "new",
        "registered at line {} of the series",
        created.line
    );

    // The work area holds the base by now, so there is only one place to edit and one next step.
    Hints::new("next")
        .line(format!("edit in {}", created.work_area.display()))
        .entry(
            skot_args("save", &created.name),
            "save your changes to the patch",
        )
        .print();
    Ok(Exit::Success)
}

/// The plain work behind [`run`]: no clap, no stdout, no verdict.
fn create(layout: &Layout, name: &str, base: Option<&str>, at: Option<&str>) -> Result<Created> {
    let (config, mut series) = layout.project()?;
    let slug = slug(name)?;

    // Held for the whole run: the series is the file two concurrent runs must not
    // interleave, and `session::materialise` assumes the caller already holds it rather
    // than taking a second one.
    let _lock = layout.lock()?;
    let state = State::load(layout)?;
    let base = match base {
        Some(base) => base.to_owned(),
        None => state
            .ref_name(layout)
            .map(str::to_owned)
            .or_else(|| layout.base_ref())
            .or_else(|| config.upstream.default_ref.clone())
            .or_else(|| super::mirror_head_ref(layout, "new"))
            .ok_or_else(|| {
                // `apply`'s version of this error says "run `skot patch apply <ref>`", which is the
                // wrong advice here: the user has not decided to apply anything, they have decided
                // to start a patch. The flag is the way to say which upstream they mean.
                anyhow::anyhow!(
                    "cannot tell which upstream to start the patch from.\n\
                 Pass one (`{}`), set `upstream.default-ref` in {}, \
                 or `{}` if the sandbox should be put in place first.",
                    skot_args("new", "--base main"),
                    layout.config_file().display(),
                    skot_args("apply", "<ref>")
                )
            })?,
    };

    let index = series::position_at(&series, at)?;
    let patch_name = series::default_patch_name(&slug);
    if layout.patches_dir().join(&patch_name).exists() {
        bail!(
            "{} already exists.\n\
             Pick another name, or `{}` if you think it is stale.",
            layout.patches_dir().join(&patch_name).display(),
            skot("clean")
        );
    }

    // The last thing that can fail for a reason the series is responsible for, and the last
    // such failure that costs nothing: resolve the base and replay the patches that will sit
    // under the new one. Doing it before the first write is what makes a bad base free, and
    // it is the same replay `session::plan` performs for every opener, so `new` cannot put a
    // session somewhere the series does not reach.
    let entries = series.entries(layout)?;
    let plan = session::plan(layout, &base, &entries[..index], None)?;

    std::fs::create_dir_all(layout.patches_dir())?;
    // The line goes in, the series is saved to match, and only then is the new file
    // written: a failure anywhere before the write leaves at worst a listed patch with no
    // file, which `skot patch list` calls out. Nothing else in the series moves, so there
    // is no record to update behind it: the applied set and the work areas still name the
    // patches they named before.
    let line = series.insert(&patch_name, index)?;
    series.save(layout)?;
    std::fs::write(layout.patches_dir().join(&patch_name), "")?;

    // Last of all the work area: a worktree with no manifest entry is recognised as a husk
    // and replaced by the next attempt, while an entry with no worktree would strand the
    // user in a dead directory, so nothing may be recorded that cannot be opened.
    let work_area = session::materialise(layout, &patch_name, &plan).with_context(|| {
        // Only failure left with the patch already written and registered, so the remedy has
        // to be about the orphan rather than about undoing anything.
        format!(
            "{} was created but its work area could not be opened; `{}` opens it.",
            patch_name,
            skot_args("edit", &patch_name)
        )
    })?;

    let subject = authoring::subject_from_name(&patch_name);
    Ok(Created {
        path: layout.patches_dir().join(&patch_name),
        work_area: layout.work_patch_dir(work_area.dir_name())?,
        name: patch_name,
        base,
        line,
        subject,
    })
}

/// Turn a human name into something safe to put in a filename.
///
/// The name ends up in two places that both care: a path under `patches/`, and a line of the
/// series. So `/`, `\`, and anything that could escape the directory are refused outright
/// rather than sanitised — a name the tool quietly rewrites is a name the user will not
/// recognise in `skot patch list`.
pub(super) fn slug(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("a patch needs a name");
    }
    if let Some(bad) = trimmed
        .chars()
        .find(|c| matches!(c, '/' | '\\' | ':' | '\0' | '\n' | '\r') || c.is_control())
    {
        bail!(
            "`{trimmed}` cannot be used as a patch name: it contains {bad:?}.\n\
             The name becomes a filename in {} and a line of the series, so it cannot \
             contain a path separator or a control character.",
            "patches/"
        );
    }
    if trimmed == "." || trimmed == ".." {
        bail!("`{trimmed}` cannot be used as a patch name");
    }

    // Whitespace becomes a dash. Patch names come from commit subjects and `skot patch new
    // <words>`, and people write those with spaces in them; a filename with a space has to be
    // quoted in every shell, every `series` line it is typed into, and every `cp` in a hook.
    // The dash form is what `git format-patch` would have produced anyway.
    let name = trimmed.split_whitespace().collect::<Vec<_>>().join("-");
    if name.is_empty() {
        bail!("`{trimmed}` has no characters that can go in a filename");
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command, PatchCommand};
    use crate::commands::testsupport;
    use clap::Parser as _;

    /// A project with a mirror holding one commit and `default-ref` set, because `new` puts
    /// the sandbox on a base before it writes anything — there is no upstream to work against
    /// in a bare directory, and pretending otherwise would test a different command.
    fn project() -> (tempfile::TempDir, crate::layout::Layout) {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = testsupport::project_with_sandbox(tmp.path());
        std::fs::write(
            layout.config_file(),
            "[upstream]\nurl = \"unused\"\ndefault-ref = \"main\"\n",
        )
        .unwrap();
        (tmp, layout)
    }

    #[test]
    fn new_creates_the_file_and_registers_it() {
        let (_tmp, layout) = project();
        create(&layout, "trim-widgets", None, None).unwrap();
        assert!(layout.patches_dir().join("trim-widgets.patch").is_file());
        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "trim-widgets.patch\n"
        );
    }

    #[test]
    fn new_at_inserts_the_line_and_moves_nothing_else() {
        // `--at` picks where the line goes. Nothing else in the series is touched: the
        // patches below keep the names they were created with, and an applied set that
        // names them keeps pointing at patches that still exist. The new patch is *not*
        // added to the set: `new` applies nothing.
        let (_tmp, layout) = project();
        create(&layout, "first", None, None).unwrap();
        create(&layout, "second", None, None).unwrap();
        create(&layout, "third", None, None).unwrap();

        let mut state = State::load(&layout).unwrap();
        state
            .set_applied(
                &layout,
                vec![
                    "first.patch".to_owned(),
                    "second.patch".to_owned(),
                    "third.patch".to_owned(),
                ],
            )
            .unwrap();

        let created = create(&layout, "middle", None, Some("2")).unwrap();

        assert_eq!(created.name, "middle.patch");
        assert_eq!(created.line, 2);
        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "first.patch\nmiddle.patch\nsecond.patch\nthird.patch\n"
        );
        assert!(layout.patches_dir().join("second.patch").is_file());
        assert!(layout.patches_dir().join("third.patch").is_file());
        let state = State::load(&layout).unwrap();
        assert_eq!(
            state.applied(&layout).to_vec(),
            ["first.patch", "second.patch", "third.patch"],
            "the applied set still names the same patches, and gains no new one"
        );
    }

    #[test]
    fn new_appends_after_patches_that_are_already_there() {
        let (_tmp, layout) = project();
        testsupport::write_series(&layout, &[("existing.patch", "")]);
        create(&layout, "next", None, None).unwrap();
        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "existing.patch\nnext.patch\n"
        );
    }

    #[test]
    fn new_opens_a_work_area_holding_the_series_and_leaves_shared_state_alone() {
        // The loop is `new` → edit → `save`, and the save diffs the work area against
        // the series below the patch. If `new` did not put the series in place, the user's
        // first edit would be exported as a patch against the wrong tree. What it must not do
        // is touch the shared run state: applying the series is not what starting a patch is,
        // and a patch counted as applied before anything replayed it would put the next
        // `pop` in charge of work that was never done.
        let (_tmp, layout) = project();
        testsupport::write_series(&layout, &[("existing.patch", testsupport::patch_one())]);

        let created = create(&layout, "next", None, None).unwrap();

        assert_eq!(
            std::fs::read_to_string(created.work_area.join("f.txt")).unwrap(),
            "first\n",
            "the work area should hold the series below the new patch"
        );
        assert!(
            created.work_area.join(".git").is_file(),
            "a linked worktree of the mirror, not a copy"
        );
        assert_eq!(
            std::fs::read_to_string(layout.sandbox_dir().join("f.txt")).unwrap(),
            "base\n",
            "the sandbox belongs to applying, not to authoring"
        );
        assert!(
            !layout.state_file().exists(),
            "new applies nothing, so it must not write the applied set"
        );
    }

    #[test]
    fn new_honours_an_explicit_base() {
        // `--base` is the only way to say which upstream the patch belongs on, and silently
        // ignoring it would produce a patch built on the wrong tree. An unresolvable ref must
        // fail loudly and leave the recorded base alone. After the fetch has had its
        // chance, the answer names the ref the upstream does not have.
        let (_tmp, layout) = project();
        let before = crate::state::State::load(&layout)
            .unwrap()
            .ref_name(&layout)
            .map(str::to_owned);
        let up = _tmp.path().join("upstream.git");
        let repo = git2::Repository::init_bare(&up).unwrap();
        let tree_oid = repo.treebuilder(None).unwrap().write().unwrap();
        let who = git2::Signature::now("t", "t@e").unwrap();
        {
            let tree = repo.find_tree(tree_oid).unwrap();
            repo.commit(Some("refs/heads/main"), &who, &who, "base", &tree, &[])
                .unwrap();
        }
        drop(repo);
        std::fs::write(
            layout.config_file(),
            format!("[upstream]\nurl = \"{}\"\n", up.display()),
        )
        .unwrap();

        let err = create(&layout, "next", Some("nope"), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("nope"), "the error should name the ref: {err}");

        // A base that cannot be resolved must not leave half the work done: the sandbox has
        // been reset by this point, so writing the patch anyway would register a patch whose
        // base nothing can reach.
        assert!(
            !layout.patches_dir().join("next.patch").exists(),
            "no patch should be created"
        );
        assert_eq!(
            crate::state::State::load(&layout)
                .unwrap()
                .ref_name(&layout)
                .map(str::to_owned),
            before,
            "the recorded base must be unchanged by a failed run"
        );
    }

    #[test]
    fn new_without_a_default_ref_says_how_to_choose_one() {
        // `apply`'s advice for this situation ("run `skot patch apply <ref>`") is the wrong
        // advice here, because the user has not decided to apply anything. The mirror's own
        // HEAD stands in only when there is a mirror; without one there is nothing left to
        // ask and the advice still arrives.
        let (_tmp, layout) = testsupport::project();

        let err = create(&layout, "next", None, None).unwrap_err().to_string();
        assert!(err.contains("--base"), "should point at the flag: {err}");
        assert!(
            err.contains("default-ref"),
            "should point at the config: {err}"
        );
    }

    #[test]
    fn new_falls_back_to_the_mirrors_own_head() {
        // No argument, no series base, no `upstream.default-ref`: the patch starts from
        // the branch the mirror itself points at instead of demanding a ref first.
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = testsupport::project_with_sandbox(tmp.path());

        create(&layout, "next", None, None).unwrap();

        assert!(layout.patches_dir().join("next.patch").is_file());
    }

    #[test]
    fn spaces_in_a_name_become_dashes() {
        // Patch names come from commit subjects, and people write those with spaces. A
        // filename with a space has to be quoted in every shell and in every `series` line
        // anyone types, and `add greeting.patch` is not something to hand a user.
        let (_tmp, layout) = project();
        create(&layout, "add greeting", None, None).unwrap();
        assert!(
            layout.patches_dir().join("add-greeting.patch").is_file(),
            "got {:?}",
            std::fs::read_dir(layout.patches_dir())
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn new_refuses_to_clobber_an_existing_patch() {
        let (_tmp, layout) = project();
        std::fs::write(layout.patches_dir().join("trim-widgets.patch"), "mine").unwrap();

        let err = create(&layout, "trim-widgets", None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists"), "got {err}");
        assert_eq!(
            std::fs::read_to_string(layout.patches_dir().join("trim-widgets.patch")).unwrap(),
            "mine",
            "the existing patch was overwritten"
        );
    }

    #[test]
    fn a_name_that_is_not_a_safe_filename_is_refused() {
        // The name goes into a path and into the series, so a separator or a newline has to
        // be rejected rather than producing a file outside patches/.
        for bad in ["../escape", "a/b", "..", "with\nnewline", ""] {
            let (_tmp, layout) = project();
            let err = create(&layout, bad, None, None).unwrap_err().to_string();
            assert!(!err.is_empty(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn a_series_that_will_not_apply_stops_new_before_it_writes_anything() {
        // `new` replays the series against the base before it writes anything. If that
        // replay fails, carrying on would create the patch on a base the series does not
        // hold, and the first `save` would then fold the *missing* work into the new
        // patch, because the base it diffs against is what the patches below produced, not
        // what is actually there. Failing here is much cheaper than debugging that later.
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = testsupport::project_with_sandbox(tmp.path());
        std::fs::write(
            layout.config_file(),
            "[upstream]\nurl = \"unused\"\ndefault-ref = \"main\"\n",
        )
        .unwrap();
        // A patch whose context does not match the mirror's single commit.
        testsupport::write_series(
            &layout,
            &[(
                "a.patch",
                "\
diff --git a/nowhere.txt b/nowhere.txt
index 1111111..2222222 100644
--- a/nowhere.txt
+++ b/nowhere.txt
@@ -1 +1 @@
-base
+edited
",
            )],
        );

        let err = create(&layout, "after", None, None).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("a.patch"),
            "the error should name the patch that would not apply: {text}"
        );
        assert!(
            !layout.patches_dir().join("after.patch").exists(),
            "no patch should be created when the base could not be prepared"
        );
        assert!(
            std::fs::read_to_string(layout.series_file())
                .unwrap()
                .lines()
                .filter(|l| !l.trim().is_empty())
                .count()
                == 1,
            "the series should be untouched"
        );
    }

    #[test]
    fn the_new_command_parses_with_a_name_and_an_optional_base() {
        let cli = Cli::try_parse_from(["skot", "patch", "new", "trim-widgets"]).unwrap();
        let Command::Patch(PatchCommand::New(args)) = cli.command else {
            panic!("expected new")
        };
        assert_eq!(args.name, "trim-widgets");
        assert!(args.base.is_none());
        assert!(Cli::try_parse_from(["skot", "patch", "new", "x", "--base", "v2"]).is_ok());
    }
}
