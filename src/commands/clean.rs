use anyhow::{Result, bail};
use log::warn;

use crate::cli::{CleanArgs, CleanLevel};
use crate::commands::{Ctx, Exit, emit_json};
use crate::invocation::skot_args;
use crate::layout::{self, Layout};
use crate::report;
use crate::session;

/// What a clean level would remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub level: CleanLevel,
    pub paths: Vec<(String, std::path::PathBuf)>,
    /// The patches whose work areas this plan drops, if any. Work areas with unsaved
    /// changes are an axis, not a level: they only appear when `--sessions` asked for
    /// them. Work areas that match their patch are derived state and join any plan.
    pub sessions: Vec<String>,
}

impl Plan {
    /// Describe what a level removes, without touching anything.
    ///
    /// The sandbox and the mirror are listed whenever they exist, because only a real
    /// build or a real sync ever creates them. The out directory gets a stricter test:
    /// `skot init` creates it empty, so counting it would leave every `clean out` with
    /// one entry that reclaims nothing, and a message that is always the same is a
    /// message people stop reading.
    ///
    /// Work areas are split by whether they hold unsaved changes. A work area that
    /// matches its patch holds nothing the patch file does not, so it is derived state
    /// like the sandbox: every plan reaps the saved ones, one directory at a time so an
    /// unsaved sibling is never taken with them. Unsaved work areas join only when
    /// `with_sessions` says so, and even then they sit after the sandbox (their worktrees
    /// are registered in the mirror and must be pruned while it still exists) and before
    /// the mirror itself.
    pub fn for_level(layout: &Layout, level: CleanLevel, with_sessions: bool) -> Self {
        let mut paths = Vec::new();
        let mut sessions = Vec::new();
        // The root, not the active series' tree: `clean sandbox` frees every tree in it,
        // and that includes whichever slot a mode flip has just left behind, shared or
        // per-series, without the plan having to know which one is current.
        let sandbox = layout.sandbox_root();
        if sandbox.exists() {
            paths.push(("the sandbox".to_owned(), sandbox));
        }
        let open = open_sessions(layout);
        if with_sessions && !open.is_empty() {
            // The whole work directory goes: the unsaved ones were asked for, and the
            // saved ones inside it are derived state that would have gone anyway.
            paths.push(("work areas".to_owned(), layout.work_dir()));
            sessions = open;
        } else {
            // Saved ones only, one directory at a time so an unsaved sibling is never
            // taken with them. This is also the `--sessions` plan when nothing is
            // unsaved: there is nothing protected left to ask about.
            for (patch, dir) in saved_work_areas(layout) {
                paths.push((format!("{patch} (saved)"), dir));
            }
        }
        let out = layout.out_dir();
        if level >= CleanLevel::Out && holds_content(&out) {
            paths.push(("the out directory".to_owned(), out));
        }
        let mirror = layout.mirror_dir();
        if level >= CleanLevel::Mirror && mirror.exists() {
            paths.push(("the mirror".to_owned(), mirror));
        }
        if level == CleanLevel::All {
            let lock = layout.lock_file();
            if lock.exists() {
                paths.push(("the lock file".to_owned(), lock));
            }
        }
        Self {
            level,
            paths,
            sessions,
        }
    }

    pub fn bytes(&self) -> u64 {
        self.paths.iter().map(|(_, p)| layout::dir_size(p)).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
}

/// Whether `path` has anything in it, treating a missing path and an empty directory
/// the same way.
fn holds_content(path: &std::path::Path) -> bool {
    if path.is_file() {
        return true;
    }
    std::fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_some())
}

/// The patches with a work area that holds unsaved changes, in a stable order.
///
/// Only these are worth a complaint: their edits exist nowhere else. A work area that
/// matches its patch is derived state, reaped like the sandbox. A comparison that fails
/// counts as unsaved: the safe direction is to protect it, not to delete it.
fn open_sessions(layout: &Layout) -> Vec<String> {
    session::Manifest::load(layout)
        .map(|m| {
            m.sessions
                .iter()
                .filter(|s| session::unsaved(layout, s).unwrap_or(true))
                .map(|s| s.patch.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// The work areas that hold nothing their patch does not: derived state, safe to reap.
/// Paired with the directory so each can be pruned individually.
fn saved_work_areas(layout: &Layout) -> Vec<(String, std::path::PathBuf)> {
    session::Manifest::load(layout)
        .map(|m| {
            m.sessions
                .iter()
                .filter(|s| matches!(session::unsaved(layout, s), Ok(false)))
                .filter_map(|s| s.dir_path(layout).ok().map(|dir| (s.patch.clone(), dir)))
                .collect()
        })
        .unwrap_or_default()
}

pub fn run(ctx: &Ctx, args: &CleanArgs) -> Result<Exit> {
    let open = open_sessions(&ctx.layout);

    // Removing the mirror orphans every work area with unsaved changes: their worktrees are
    // registered in it, so without it they are directories nothing can open, holding edits
    // that exist nowhere else. That is not a side effect a tidy is allowed to have
    // silently, so it is refused until those work areas are named as the thing to drop.
    // Work areas that match their patch are already in the plan as derived state.
    if !args.sessions && args.level >= CleanLevel::Mirror && !open.is_empty() {
        bail!(
            "{} would orphan {} work area(s) with unsaved changes: {}.\n\
             Their worktrees live in the mirror, so the edits would be unreachable.\n\
             `{}` drops them with the rest; `{}` saves one when you are done with it.",
            label(args.level),
            open.len(),
            open.join(", "),
            skot_args("clean", "--sessions"),
            skot_args("save", "<patch>"),
        );
    }

    let plan = Plan::for_level(&ctx.layout, args.level, args.sessions);

    if ctx.cli.json {
        emit_json(&serde_json::json!({
            "series_name": ctx.layout.series(),
            "level": format!("{:?}", plan.level).to_lowercase(),
            "would_remove": plan.paths.iter().map(|(label, p)| serde_json::json!({
                "what": label,
                "path": p.display().to_string(),
                "size": layout::human_bytes(layout::dir_size(p)),
            })).collect::<Vec<_>>(),
            "sessions": plan.sessions,
            "bytes": plan.bytes(),
            "dry_run": !ctx.yes,
        }))?;
        return Ok(Exit::Success);
    }

    report::section(&format!("clean: {}", label(args.level)));

    if plan.is_empty() {
        report::note("nothing to clean");
        if !open.is_empty() {
            report::note(&format!(
                "Keeping {} work area(s) with unsaved changes: {}. `{}` saves them.",
                open.len(),
                open.join(", "),
                skot_args("save", "<patch>")
            ));
        }
        return Ok(Exit::Success);
    }

    for (label, path) in &plan.paths {
        report::item(&format!(
            "{}  {:>8}  {}",
            report::bad(&format!("{label:<18}")),
            layout::human_bytes(layout::dir_size(path)),
            path.display()
        ));
    }
    report::note("skot never deletes patches/ or the series.");

    if !ctx.yes {
        warn!(target: "clean", "dry run. Re-run with --yes to do it.");
        return Ok(Exit::Success);
    }

    let lock = ctx.lock()?;
    let freed_before = plan.bytes();

    for (label, path) in &plan.paths {
        remove_one(&ctx.layout, label, path)?;
        report::line(&format!("{} {}", report::ok("removed"), path.display()));
    }

    // The records must not outlive the trees they describe. The whole sandbox root goes,
    // every series' tree in it included, so every record goes with it: one shared record,
    // or one per series. Nothing is cleared when the plan held only work areas: a work
    // area is not an apply, and the set it left behind is exactly what the next run
    // should still see.
    if args.level >= CleanLevel::Sandbox {
        crate::state::State::load(&ctx.layout)?.clear_sandboxes(&ctx.layout)?;
    }

    drop(lock);

    let freed = freed_before.saturating_sub(layout::dir_size(&ctx.layout.state_dir()));
    report::line(&report::ok(&format!(
        "freed about {}",
        layout::human_bytes(freed)
    )));
    Ok(Exit::Success)
}

/// Remove one planned path.
///
/// The sandbox is not a plain delete. It is a worktree *registered in the mirror*, so
/// removing the directory alone leaves the registration behind and the next `apply` fails
/// with "directory exists" inside the mirror's `worktrees/` directory — a libgit2 message
/// that never mentions the clean that caused it. The plan always lists the sandbox before
/// the mirror, so this runs while there is still a mirror to unregister from.
///
/// Work areas are the same shape: each worktree is registered in the mirror, so they are
/// ended one by one (pruning the registration) before the whole work directory goes, and
/// a single saved work area is ended by name rather than deleted as a bare directory.
fn remove_one(layout: &Layout, label: &str, path: &std::path::Path) -> Result<()> {
    if label == "the sandbox" {
        crate::git::sandbox::remove(layout)?;
        return Ok(());
    }
    if label == "work areas" {
        for s in session::Manifest::load(layout)?.sessions {
            session::end(layout, &s)?;
        }
        let _ = layout.guarded_remove(path)?;
        return Ok(());
    }
    if label.ends_with(" (saved)") {
        // By directory, not by patch name: two series can hold a patch of the same name,
        // and the plan paired this session with this path.
        if let Some(s) = session::Manifest::load(layout)?
            .sessions
            .into_iter()
            .find(|s| s.dir_path(layout).ok().as_deref() == Some(path))
        {
            session::end(layout, &s)?;
        }
        let _ = layout.guarded_remove(path)?;
        return Ok(());
    }
    let _ = layout.guarded_remove(path)?;
    Ok(())
}

fn label(level: CleanLevel) -> &'static str {
    match level {
        CleanLevel::Sandbox => "sandbox (the worktree; next build reinstalls dependencies)",
        CleanLevel::Out => "out (also the generated out directory)",
        CleanLevel::Mirror => "mirror (also the bare mirror; next run re-clones)",
        CleanLevel::All => {
            "all (everything under .skot but the series pointer; next run re-clones and reinstalls)"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    fn layout() -> (tempfile::TempDir, Layout) {
        let tmp = tempfile::tempdir().unwrap();
        let l = Layout::new(tmp.path());
        l.ensure_state_dirs().unwrap();
        (tmp, l)
    }

    #[test]
    fn the_default_level_removes_only_the_sandbox() {
        let (_tmp, l) = layout();
        std::fs::create_dir_all(l.sandbox_dir()).unwrap();
        std::fs::create_dir_all(l.mirror_dir()).unwrap();

        let plan = Plan::for_level(&l, CleanLevel::Sandbox, false);
        assert_eq!(plan.paths.len(), 1);
        // The root, so the shared slot and every per-series tree go together.
        assert_eq!(plan.paths[0].1, l.sandbox_root());
    }

    #[test]
    fn clean_sandbox_sees_a_leftover_shared_sandbox_after_a_mode_flip() {
        // The shared slot sits *under* the root at `@shared`, beside the per-series
        // trees. Planning the active series' own dir would list only that tree and
        // leave the slot from the other scope behind.
        let (_tmp, l) = layout();
        let per = l.with_per_series_sandbox(true);
        std::fs::create_dir_all(l.sandbox_dir()).unwrap();
        assert!(l.sandbox_dir().ends_with("@shared"));

        let plan = Plan::for_level(&per, CleanLevel::Sandbox, false);
        assert_eq!(plan.paths.len(), 1);
        assert_eq!(plan.paths[0].1, per.sandbox_root());
        assert!(plan.paths[0].1.join("@shared").exists());
    }

    #[test]
    fn each_level_is_a_superset_of_the_one_before_it() {
        let (_tmp, l) = layout();
        for dir in [l.sandbox_dir(), l.mirror_dir(), l.out_dir()] {
            std::fs::create_dir_all(&dir).unwrap();
        }
        let mut previous = 0;
        for level in [
            CleanLevel::Sandbox,
            CleanLevel::Out,
            CleanLevel::Mirror,
            CleanLevel::All,
        ] {
            let plan = Plan::for_level(&l, level, false);
            assert!(
                plan.paths.len() >= previous,
                "{level:?} removed less than the level before"
            );
            previous = plan.paths.len();
        }
    }

    #[test]
    fn a_plan_is_empty_when_there_is_nothing_to_clean() {
        let (_tmp, l) = layout();
        assert!(Plan::for_level(&l, CleanLevel::All, false).is_empty());
    }

    #[test]
    fn clean_never_targets_patches_or_the_series() {
        let (_tmp, l) = layout();
        std::fs::create_dir_all(l.sandbox_dir()).unwrap();
        std::fs::create_dir_all(l.mirror_dir()).unwrap();
        std::fs::create_dir_all(l.patches_dir()).unwrap();
        std::fs::write(l.series_file(), "a.patch\n").unwrap();

        for level in [
            CleanLevel::Sandbox,
            CleanLevel::Out,
            CleanLevel::Mirror,
            CleanLevel::All,
        ] {
            let plan = Plan::for_level(&l, level, true);
            for (_, path) in &plan.paths {
                assert!(
                    !path.starts_with(l.patches_dir()),
                    "{level:?} would delete patches"
                );
                assert_ne!(path, &l.series_file(), "{level:?} would delete the series");
            }
        }
    }

    #[test]
    fn clean_labels_explain_the_recovery_cost() {
        assert!(label(CleanLevel::Sandbox).contains("reinstalls"));
        assert!(label(CleanLevel::Mirror).contains("re-clones"));
    }

    /// A bare mirror with one commit, a sandbox worktree registered in it, and the state
    /// file: the minimum for `clean sandbox` to have something real to remove.
    fn layout_with_sandbox() -> (tempfile::TempDir, Layout) {
        let (tmp, l) = layout();
        std::fs::create_dir_all(l.mirror_dir()).unwrap();
        let repo = git2::Repository::init_bare(l.mirror_dir()).unwrap();
        let tree = {
            let mut b = repo.treebuilder(None).unwrap();
            b.insert("f.txt", repo.blob(b"base\n").unwrap(), 0o100644)
                .unwrap();
            b.write().unwrap()
        };
        let who = git2::Signature::now("t", "t@e").unwrap();
        let sha = repo
            .commit(
                Some("refs/heads/main"),
                &who,
                &who,
                "base",
                &repo.find_tree(tree).unwrap(),
                &[],
            )
            .unwrap();
        repo.set_head("refs/heads/main").unwrap();

        let mirror = crate::git::mirror::Mirror::open(&l).unwrap();
        crate::git::sandbox::create(&l, &mirror).unwrap();
        crate::git::sandbox::reset_to(&l, &sha.to_string(), &[]).unwrap();
        (tmp, l)
    }

    #[test]
    fn cleaning_the_sandbox_unregisters_it_so_the_next_apply_works() {
        // The regression: deleting the sandbox directory left the worktree registered in the
        // mirror, and the next apply died with a raw libgit2 "directory exists" from inside
        // the mirror — with nothing pointing at the clean that caused it.
        let (_tmp, l) = layout_with_sandbox();
        let plan = Plan::for_level(&l, CleanLevel::Sandbox, false);

        for (label, path) in &plan.paths {
            remove_one(&l, label, path).unwrap();
        }

        assert!(!l.sandbox_dir().exists(), "the sandbox should be gone");
        let repo = git2::Repository::open_bare(l.mirror_dir()).unwrap();
        assert!(
            repo.find_worktree("sandbox").is_err(),
            "the worktree registration outlived the clean"
        );

        // The real assertion: the sandbox can be created again, which is what `apply` does.
        let mirror = crate::git::mirror::Mirror::open(&l).unwrap();
        crate::git::sandbox::create(&l, &mirror).unwrap();
        assert!(l.sandbox_dir().exists());
    }

    #[test]
    fn cleaning_the_mirror_removes_the_sandbox_first() {
        // Order is load-bearing: pruning needs the mirror to still be there.
        let (_tmp, l) = layout_with_sandbox();
        let plan = Plan::for_level(&l, CleanLevel::Mirror, false);
        let sandbox = plan.paths.iter().position(|(n, _)| n == "the sandbox");
        let mirror = plan.paths.iter().position(|(n, _)| n == "the mirror");
        assert!(
            sandbox < mirror,
            "the sandbox must be removed before the mirror"
        );

        for (label, path) in &plan.paths {
            remove_one(&l, label, path).unwrap();
        }
        assert!(!l.mirror_dir().exists());
        assert!(!l.sandbox_dir().exists());
    }

    /// A project with a mirror, a two-patch series, and a work area on each holding
    /// unsaved changes: the state `clean` has to be careful around.
    fn layout_with_work_areas(root: &std::path::Path) -> Layout {
        let (l, _sha) = crate::commands::testsupport::mirror_with_one_commit(root);
        std::fs::write(
            l.config_file(),
            "[upstream]\nurl = \"unused\"\ndefault-ref = \"main\"\n",
        )
        .unwrap();
        crate::commands::testsupport::write_series(
            &l,
            &[
                ("a.patch", crate::commands::testsupport::patch_one()),
                ("b.patch", crate::commands::testsupport::patch_two()),
            ],
        );
        let entries = l.project().unwrap().1.entries(&l).unwrap();
        for entry in &entries {
            let below = entries
                .iter()
                .take_while(|e| e.line < entry.line)
                .cloned()
                .collect::<Vec<_>>();
            let plan = session::plan(&l, "main", &below, Some(entry)).unwrap();
            session::materialise(&l, &entry.name, &plan).unwrap();
            // Something the patch file does not have, so the work area counts as unsaved
            // and is protected rather than reaped as derived state.
            let dir = l.work_patch_dir(&entry.name).unwrap();
            std::fs::write(dir.join("unsaved.txt"), "work in progress\n").unwrap();
        }
        l
    }

    #[test]
    fn unsaved_work_areas_are_never_in_a_default_plan() {
        let tmp = tempfile::tempdir().unwrap();
        let l = layout_with_work_areas(tmp.path());

        for level in [
            CleanLevel::Sandbox,
            CleanLevel::Out,
            CleanLevel::Mirror,
            CleanLevel::All,
        ] {
            let plan = Plan::for_level(&l, level, false);
            assert!(
                !plan.paths.iter().any(|(n, _)| n == "work areas"),
                "{level:?} must not touch unsaved work areas on its own"
            );
            assert!(plan.sessions.is_empty());
        }
    }

    #[test]
    fn a_saved_work_area_is_derived_state_and_joins_every_plan() {
        // Nothing unsaved in it, so the patch file holds everything it does: reaped like
        // the sandbox, one directory at a time so an unsaved sibling is never taken too.
        let tmp = tempfile::tempdir().unwrap();
        let (l, _sha) = crate::commands::testsupport::mirror_with_one_commit(tmp.path());
        std::fs::write(
            l.config_file(),
            "[upstream]\nurl = \"unused\"\ndefault-ref = \"main\"\n",
        )
        .unwrap();
        crate::commands::testsupport::write_series(
            &l,
            &[("a.patch", crate::commands::testsupport::patch_one())],
        );
        let entries = l.project().unwrap().1.entries(&l).unwrap();
        let plan = session::plan(&l, "main", &[], Some(&entries[0])).unwrap();
        session::materialise(&l, "a.patch", &plan).unwrap();

        let plan = Plan::for_level(&l, CleanLevel::Sandbox, false);

        assert!(
            plan.paths.iter().any(|(n, _)| n == "a.patch (saved)"),
            "a saved work area is derived state: {plan:?}"
        );
        assert!(plan.sessions.is_empty(), "and is not an unsaved complaint");

        for (label, path) in &plan.paths {
            remove_one(&l, label, path).unwrap();
        }
        assert!(
            !l.work_patch_dir("a.patch").unwrap().exists(),
            "the work area is gone"
        );
        assert!(
            session::Manifest::load(&l).unwrap().sessions.is_empty(),
            "and so is its record"
        );
        assert!(l.mirror_dir().exists(), "the mirror is untouched");
    }

    #[test]
    fn unsaved_work_areas_join_the_plan_only_when_asked() {
        let tmp = tempfile::tempdir().unwrap();
        let l = layout_with_work_areas(tmp.path());

        let plan = Plan::for_level(&l, CleanLevel::Sandbox, true);

        assert_eq!(
            plan.sessions,
            ["a.patch", "b.patch"],
            "both work areas with unsaved changes are named"
        );
        assert!(plan.paths.iter().any(|(n, _)| n == "work areas"));
        let (_, path) = plan.paths.iter().find(|(n, _)| n == "work areas").unwrap();
        assert_eq!(path, &l.work_dir());
    }

    #[test]
    fn work_areas_go_after_the_sandbox_and_before_the_mirror() {
        // Both orderings are load-bearing: pruning a worktree needs the mirror, and the
        // sandbox is a worktree too, so it is unregistered first.
        let tmp = tempfile::tempdir().unwrap();
        let l = layout_with_work_areas(tmp.path());
        std::fs::create_dir_all(l.sandbox_dir()).unwrap();

        let plan = Plan::for_level(&l, CleanLevel::Mirror, true);
        let sandbox = plan.paths.iter().position(|(n, _)| n == "the sandbox");
        let sessions = plan.paths.iter().position(|(n, _)| n == "work areas");
        let mirror = plan.paths.iter().position(|(n, _)| n == "the mirror");

        assert!(sandbox.unwrap() < sessions.unwrap());
        assert!(sessions.unwrap() < mirror.unwrap());
    }

    #[test]
    fn cleaning_work_areas_prunes_their_worktrees_and_keeps_the_mirror() {
        let tmp = tempfile::tempdir().unwrap();
        let l = layout_with_work_areas(tmp.path());

        let plan = Plan::for_level(&l, CleanLevel::Sandbox, true);
        for (label, path) in &plan.paths {
            remove_one(&l, label, path).unwrap();
        }

        assert!(
            !l.work_dir().exists(),
            "the work directory goes with the work areas"
        );
        assert!(
            session::Manifest::load(&l).unwrap().sessions.is_empty(),
            "the manifest is emptied"
        );
        assert!(
            l.mirror_dir().exists(),
            "a sandbox-level clean never touches the mirror"
        );
        let repo = git2::Repository::open_bare(l.mirror_dir()).unwrap();
        for name in ["a.patch", "b.patch"] {
            assert!(
                repo.find_worktree(name).is_err(),
                "{name}'s registration outlived the clean"
            );
        }
    }

    #[test]
    fn removing_the_mirror_with_unsaved_changes_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let l = layout_with_work_areas(tmp.path());
        let ctx = Ctx {
            layout: l.clone(),
            cli: crate::cli::Cli::try_parse_from(["skot", "clean", "mirror"]).unwrap(),
            yes: true,
        };
        let args = CleanArgs {
            level: CleanLevel::Mirror,
            sessions: false,
        };

        let err = run(&ctx, &args).unwrap_err().to_string();

        assert!(err.contains("orphan"), "it should say why: {err}");
        assert!(err.contains("a.patch"), "it should name them: {err}");
        assert!(err.contains(&skot_args("clean", "--sessions")), "{err}");
        assert!(
            err.contains(&skot_args("save", "<patch>")),
            "it should say how to save them: {err}"
        );
        assert!(
            l.mirror_dir().exists(),
            "the refusal left everything in place"
        );
        assert!(
            l.work_dir().exists(),
            "the work areas are still there to be saved"
        );
    }

    #[test]
    fn all_is_refused_while_changes_are_unsaved_and_allowed_once_asked() {
        let refused_root = tempfile::tempdir().unwrap();
        let l = layout_with_work_areas(refused_root.path());
        let refused = Ctx {
            layout: l.clone(),
            cli: crate::cli::Cli::try_parse_from(["skot", "clean", "all"]).unwrap(),
            yes: true,
        };
        let err = run(
            &refused,
            &CleanArgs {
                level: CleanLevel::All,
                sessions: false,
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("--sessions"), "{err}");

        let asked_root = tempfile::tempdir().unwrap();
        let l = layout_with_work_areas(asked_root.path());
        let asked = Ctx {
            layout: l.clone(),
            cli: crate::cli::Cli::try_parse_from(["skot", "clean", "all", "--sessions"]).unwrap(),
            yes: true,
        };
        run(
            &asked,
            &CleanArgs {
                level: CleanLevel::All,
                sessions: true,
            },
        )
        .unwrap();

        assert!(!l.mirror_dir().exists());
        assert!(!l.work_dir().exists());
        assert!(!l.sandbox_dir().exists());
    }

    #[test]
    fn clean_never_takes_the_series_pointer() {
        // The pointer is the one thing under .skot that is not derived state: it is
        // the user's answer to "which series is active", and a clean that erased it
        // would leave the next command guessing between several lines of work.
        let (_tmp, l) = layout();
        std::fs::create_dir_all(l.series_root().join("exp")).unwrap();
        crate::state::State::load(&l)
            .unwrap()
            .set_active_series(&l, "exp")
            .unwrap();
        std::fs::create_dir_all(l.sandbox_root()).unwrap();
        std::fs::write(l.sandbox_root().join("scratch"), "x").unwrap();
        std::fs::write(l.out_dir().join("artifact"), "x").unwrap();

        let ctx = Ctx {
            layout: l.clone(),
            cli: crate::cli::Cli::try_parse_from(["skot", "clean", "all", "--sessions"]).unwrap(),
            yes: true,
        };
        run(
            &ctx,
            &CleanArgs {
                level: CleanLevel::All,
                sessions: true,
            },
        )
        .unwrap();

        assert!(
            l.state_file().is_file(),
            "the pointer survives a full clean"
        );
        assert_eq!(
            crate::state::State::load(&l).unwrap().active_series(),
            Some("exp")
        );
    }
}
