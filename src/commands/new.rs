//! `new`: start the next patch, with everything already in the sandbox.
//!
//! The authoring loop is one command per step: `new` gives you a working tree with the
//! existing series applied and an empty patch registered at the end of the series, `refresh`
//! turns what you changed into that patch's contents. Nothing is committed anywhere — the
//! patch file *is* the record.
//!
//! Two things are worth stating, because both are easy to get wrong:
//!
//! - **The sandbox really is put in place.** A `new` that only wrote a file would leave the
//!   user editing whatever tree happened to be there, and `refresh` would then export whatever
//!   that tree contained. So `new` applies the series to the base it was given, which is also
//!   what makes `--base` mean something.
//! - **It does not invent the patch's contents.** The file is written empty and registered, so
//!   that `refresh` has something to fill in and the series is complete before editing starts.
//!   An empty patch in the series is honest; a patch with a guessed subject and an empty diff
//!   is not.
//!
//! An empty series is fine here: the first patch in a project is created against a bare
//! upstream, and `skot check` has nothing to complain about until the patch has content.

use anyhow::{Result, bail};
use log::{debug, info};
use serde_json::json;

use crate::cli::NewArgs;
use crate::commands::{Ctx, Exit, authoring, emit_json};
use crate::hints::Hints;
use crate::layout::Layout;
use crate::series;
use crate::state::State;

/// What `new` created, presented by [`run`].
#[derive(Debug)]
struct Created {
    name: String,
    path: std::path::PathBuf,
    base: String,
    line: usize,
    subject: String,
}

/// Create an empty patch file, append it to the series, and put the sandbox on the base.
pub fn run(ctx: &Ctx, args: &NewArgs) -> Result<Exit> {
    let created = create(&ctx.layout, &args.name, args.base.as_deref())?;

    if ctx.cli.json {
        emit_json(&json!({
            "patch": created.name,
            "path": created.path.display().to_string(),
            "base": created.base,
            "series_line": created.line,
            "subject": created.subject,
        }))?;
        return Ok(Exit::Success);
    }

    info!(target: "new", "created {}", created.name);
    debug!(target: "new", "{}", created.path.display());
    debug!(
        target: "new",
        "registered at line {} of the series",
        created.line
    );

    // The sandbox is on the base by now, so there is only one place to edit and one next step.
    Hints::new("next")
        .line(format!("edit in {}", ctx.layout.sandbox_dir().display()))
        .entry(
            format!("skot refresh {}", created.name),
            "re-export the patch from the sandbox",
        )
        .print();
    Ok(Exit::Success)
}

/// The plain work behind [`run`]: no clap, no stdout, no verdict.
fn create(layout: &Layout, name: &str, base: Option<&str>) -> Result<Created> {
    let (config, mut series) = layout.project()?;
    let slug = slug(name)?;

    let lock = layout.lock()?;
    let state = State::load(layout)?;
    let base = match base {
        Some(base) => base.to_owned(),
        None => state
            .ref_name
            .clone()
            .or_else(|| config.upstream.default_ref.clone())
            .ok_or_else(|| {
                // `apply`'s version of this error says "run `skot apply <ref>`", which is the
                // wrong advice here: the user has not decided to apply anything, they have decided
                // to start a patch. The flag is the way to say which upstream they mean.
                anyhow::anyhow!(
                    "cannot tell which upstream to start the patch from.\n\
                 Pass one (`skot new --base main`), set `upstream.default-ref` in {}, \
                 or `skot apply <ref>` if the sandbox should be put in place first.",
                    layout.config_file().display()
                )
            })?,
    };

    prepare_sandbox(layout, &base, &slug)?;

    let prefix = series.len() + 1;
    let patch_name = series::default_patch_name(&prefix.to_string(), &slug);
    if layout.patches_dir().join(&patch_name).exists() {
        bail!(
            "{} already exists.\n\
             Pick another name, or `skot clean` if you think it is stale.",
            layout.patches_dir().join(&patch_name).display()
        );
    }

    std::fs::create_dir_all(layout.patches_dir())?;
    std::fs::write(layout.patches_dir().join(&patch_name), "")?;
    let line = series.insert(&patch_name, series.len())?;
    series.save(layout)?;

    // Record the new patch as applied. It is empty, so the tree did not change, but "applied"
    // is what tells `refresh` which patch is being authored, and `push`/`pop` that the stack
    // is a prefix of the series. Without this the very next command refuses with "nothing is
    // applied", which is the opposite of useful: the user *did* just start a patch.
    let mut state = State::load(layout)?;
    let mut applied = state.applied.clone();
    applied.push(patch_name.clone());
    state.set_applied(layout, applied)?;
    drop(lock);

    let subject = authoring::subject_from_name(&patch_name);
    Ok(Created {
        path: layout.patches_dir().join(&patch_name),
        name: patch_name,
        base,
        line,
        subject,
    })
}

/// Put the sandbox on `base` with the series applied to it, recording what it did.
///
/// This is `apply` without the narration, and there is deliberately no second path: the
/// first patch in a project is created against a bare upstream with nothing to replay,
/// and `apply_locked` handles that by resetting the sandbox to `base` and applying zero
/// patches. One path means `new` cannot put the sandbox somewhere `apply` would not.
fn prepare_sandbox(layout: &Layout, base: &str, name: &str) -> Result<()> {
    let applied = crate::commands::apply::apply_locked(layout, Some(base))?;
    if !applied.ok {
        // Checked, not `?`'d: `apply_locked` reports a failed replay by *returning* a
        // verdict rather than by erroring, because the caller decides whether that
        // is fatal. Ignoring it here means `new` puts the new patch on a sandbox that
        // does not hold the series, and the first `refresh` then folds the missing work
        // into that new patch instead of failing.
        anyhow::bail!(
            "the existing series does not apply to {base}, so there is nowhere sensible to \
             start `{name}`.\n\
             {failure}\n\
             Fix or rebase that patch first. Nothing was written.",
            failure = applied.failure
        );
    }
    Ok(())
}

/// Turn a human name into something safe to put in a filename.
///
/// The name ends up in two places that both care: a path under `patches/`, and a line of the
/// series. So `/`, `\`, and anything that could escape the directory are refused outright
/// rather than sanitised — a name the tool quietly rewrites is a name the user will not
/// recognise in `skot list`.
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

    // Whitespace becomes a dash. Patch names come from commit subjects and `skot new
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
    use crate::cli::{Cli, Command};
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
        create(&layout, "trim-widgets", None).unwrap();
        assert!(
            layout
                .patches_dir()
                .join("0001-trim-widgets.patch")
                .is_file()
        );
        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "0001-trim-widgets.patch\n"
        );
    }

    #[test]
    fn new_numbers_its_patches_by_series_position() {
        // The number is what makes lexical order match application order, so a second patch
        // must not be called 0001 again.
        let (_tmp, layout) = project();
        create(&layout, "first", None).unwrap();
        create(&layout, "second", None).unwrap();

        let series = std::fs::read_to_string(layout.series_file()).unwrap();
        assert_eq!(series, "0001-first.patch\n0002-second.patch\n");
    }

    #[test]
    fn new_appends_after_patches_that_are_already_there() {
        let (_tmp, layout) = project();
        testsupport::write_series(&layout, &[("0001-existing.patch", "")]);
        create(&layout, "next", None).unwrap();
        assert_eq!(
            std::fs::read_to_string(layout.series_file()).unwrap(),
            "0001-existing.patch\n0002-next.patch\n"
        );
    }

    #[test]
    fn new_leaves_the_sandbox_holding_the_series() {
        // The loop is `new` → edit → `refresh`, and `refresh` diffs the sandbox against the
        // series below the patch. If `new` did not put the series in place, the user's first
        // edit would be exported as a patch against the wrong tree.
        let (_tmp, layout) = project();
        testsupport::write_series(
            &layout,
            &[("0001-existing.patch", testsupport::patch_one())],
        );

        create(&layout, "next", None).unwrap();

        assert_eq!(
            std::fs::read_to_string(layout.sandbox_dir().join("f.txt")).unwrap(),
            "first\n",
            "the sandbox should hold the series"
        );
        let state = crate::state::State::load(&layout).unwrap();
        assert_eq!(state.ref_name.as_deref(), Some("main"));
        assert_eq!(
            state.applied,
            vec![
                "0001-existing.patch".to_owned(),
                // The new patch counts as applied even though it is empty: it is the one being
                // authored, and this is what lets `refresh` know where to export to.
                "0002-next.patch".to_owned()
            ]
        );
    }

    #[test]
    fn new_honours_an_explicit_base() {
        // `--base` is the only way to say which upstream the patch belongs on, and silently
        // ignoring it would produce a patch built on the wrong tree. An unresolvable ref must
        // fail loudly and leave the recorded base alone. After the fetch has had its
        // chance, the answer names the ref the upstream does not have.
        let (_tmp, layout) = project();
        let before = crate::state::State::load(&layout).unwrap().ref_name;
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

        let err = create(&layout, "next", Some("nope"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("nope"), "the error should name the ref: {err}");

        // A base that cannot be resolved must not leave half the work done: the sandbox has
        // been reset by this point, so writing the patch anyway would register a patch whose
        // base nothing can reach.
        assert!(
            !layout.patches_dir().join("0001-next.patch").exists(),
            "no patch should be created"
        );
        assert_eq!(
            crate::state::State::load(&layout).unwrap().ref_name,
            before,
            "the recorded base must be unchanged by a failed run"
        );
    }

    #[test]
    fn new_without_a_default_ref_says_how_to_choose_one() {
        // `apply`'s advice for this situation — "run `skot apply <ref>`" — is the wrong
        // advice here, because the user has not decided to apply anything.
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = testsupport::project_with_sandbox(tmp.path());

        let err = create(&layout, "next", None).unwrap_err().to_string();
        assert!(err.contains("--base"), "should point at the flag: {err}");
        assert!(
            err.contains("default-ref"),
            "should point at the config: {err}"
        );
    }

    #[test]
    fn spaces_in_a_name_become_dashes() {
        // Patch names come from commit subjects, and people write those with spaces. A
        // filename with a space has to be quoted in every shell and in every `series` line
        // anyone types, and `0001-add greeting.patch` is not something to hand a user.
        let (_tmp, layout) = project();
        create(&layout, "add greeting", None).unwrap();
        assert!(
            layout
                .patches_dir()
                .join("0001-add-greeting.patch")
                .is_file(),
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
        std::fs::write(layout.patches_dir().join("0001-trim-widgets.patch"), "mine").unwrap();

        let err = create(&layout, "trim-widgets", None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists"), "got {err}");
        assert_eq!(
            std::fs::read_to_string(layout.patches_dir().join("0001-trim-widgets.patch")).unwrap(),
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
            let err = create(&layout, bad, None).unwrap_err().to_string();
            assert!(!err.is_empty(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn a_series_that_will_not_apply_stops_new_before_it_writes_anything() {
        // `new` puts the existing series in the sandbox so the new patch sits on top of it.
        // If that replay fails, carrying on would create the patch on a sandbox that does not
        // hold the series — and the first `refresh` would then fold the *missing* work into
        // the new patch, because the base it diffs against is the series prefix, not what is
        // actually there. Failing here is much cheaper than debugging that later.
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
                "0001-a.patch",
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

        let err = create(&layout, "after", None).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("0001-a.patch"),
            "the error should name the patch that would not apply: {text}"
        );
        assert!(
            !layout.patches_dir().join("0001-after.patch").exists(),
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
        let cli = Cli::try_parse_from(["skot", "new", "trim-widgets"]).unwrap();
        let Command::New(args) = cli.command else {
            panic!("expected new")
        };
        assert_eq!(args.name, "trim-widgets");
        assert!(args.base.is_none());
        assert!(Cli::try_parse_from(["skot", "new", "x", "--base", "v2"]).is_ok());
    }
}
