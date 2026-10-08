use anyhow::Result;
use log::warn;

use crate::cli::{CleanArgs, CleanLevel};
use crate::commands::{Ctx, Exit, emit_json};
use crate::layout::{self, Layout};
use crate::report;

/// What a clean level would remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub level: CleanLevel,
    pub paths: Vec<(String, std::path::PathBuf)>,
}

impl Plan {
    /// Describe what a level removes, without touching anything.
    ///
    /// The sandbox and the mirror are listed whenever they exist, because only a real
    /// build or a real sync ever creates them. The out directory gets a stricter test:
    /// `skot init` creates it empty, so counting it would leave every `clean --all`
    /// with one entry that reclaims nothing, and a message that is always the same is a
    /// message people stop reading.
    pub fn for_level(layout: &Layout, level: CleanLevel) -> Self {
        let mut paths = Vec::new();
        let sandbox = layout.sandbox_dir();
        if sandbox.exists() {
            paths.push(("the sandbox".to_owned(), sandbox));
        }
        let out = layout.out_dir();
        if level >= CleanLevel::Sandbox && holds_content(&out) {
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
        Self { level, paths }
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

pub fn run(ctx: &Ctx, args: &CleanArgs) -> Result<Exit> {
    let plan = Plan::for_level(&ctx.layout, args.level);

    if ctx.cli.json {
        emit_json(&serde_json::json!({
            "level": format!("{:?}", plan.level).to_lowercase(),
            "would_remove": plan.paths.iter().map(|(label, p)| serde_json::json!({
                "what": label,
                "path": p.display().to_string(),
                "size": layout::human_bytes(layout::dir_size(p)),
            })).collect::<Vec<_>>(),
            "bytes": plan.bytes(),
            "dry_run": !ctx.yes,
        }))?;
        return Ok(Exit::Success);
    }

    report::section(&format!("clean: {}", label(args.level)));

    if plan.is_empty() {
        report::note("nothing to clean");
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

    // The record must not outlive the tree it describes. `ref_name` and `sha` are left
    // alone: they are the last known apply target, which is what `unapply` needs if a
    // mirror happens to still be there, and the sandbox is gone either way.
    crate::state::State::load(&ctx.layout)?.clear_applied(&ctx.layout)?;

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
fn remove_one(layout: &Layout, label: &str, path: &std::path::Path) -> Result<()> {
    if label == "the sandbox" {
        crate::git::sandbox::remove(layout)?;
        return Ok(());
    }
    let _ = layout.guarded_remove(path)?;
    Ok(())
}

fn label(level: CleanLevel) -> &'static str {
    match level {
        CleanLevel::Build => "build (the sandbox; next build reinstalls dependencies)",
        CleanLevel::Sandbox => "sandbox (also the out directory)",
        CleanLevel::Mirror => "mirror (also the bare mirror; next run re-clones)",
        CleanLevel::All => "all (everything under .skot; next run re-clones and reinstalls)",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let plan = Plan::for_level(&l, CleanLevel::Build);
        assert_eq!(plan.paths.len(), 1);
        assert_eq!(plan.paths[0].1, l.sandbox_dir());
    }

    #[test]
    fn each_level_is_a_superset_of_the_one_before_it() {
        let (_tmp, l) = layout();
        for dir in [l.sandbox_dir(), l.mirror_dir(), l.out_dir()] {
            std::fs::create_dir_all(&dir).unwrap();
        }
        let mut previous = 0;
        for level in [
            CleanLevel::Build,
            CleanLevel::Sandbox,
            CleanLevel::Mirror,
            CleanLevel::All,
        ] {
            let plan = Plan::for_level(&l, level);
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
        assert!(Plan::for_level(&l, CleanLevel::All).is_empty());
    }

    #[test]
    fn clean_never_targets_patches_or_the_series() {
        let (_tmp, l) = layout();
        std::fs::create_dir_all(l.sandbox_dir()).unwrap();
        std::fs::create_dir_all(l.mirror_dir()).unwrap();
        std::fs::create_dir_all(l.patches_dir()).unwrap();
        std::fs::write(l.series_file(), "0001-a.patch\n").unwrap();

        for level in [
            CleanLevel::Build,
            CleanLevel::Sandbox,
            CleanLevel::Mirror,
            CleanLevel::All,
        ] {
            let plan = Plan::for_level(&l, level);
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
        assert!(label(CleanLevel::Build).contains("reinstalls"));
        assert!(label(CleanLevel::Mirror).contains("re-clones"));
    }

    /// A bare mirror with one commit, a sandbox worktree registered in it, and the state
    /// file — the minimum for `clean build` to have something real to remove.
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
        let plan = Plan::for_level(&l, CleanLevel::Build);

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
        let plan = Plan::for_level(&l, CleanLevel::Mirror);
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
}
