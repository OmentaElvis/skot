//! The sandbox: the working tree skot patches, builds in, and hands to hooks.
//!
//! The sandbox is a **linked worktree of the mirror**. That means the mirror is a bare
//! repository holding every object, and the sandbox is a checkout that shares that object
//! store through `.git/worktrees/<name>` rather than duplicating it.
//!
//! Why this shape rather than a second clone:
//!
//! 1. **Disk.** The sandbox stores no objects of its own. It costs the working tree plus a
//!    few kilobytes of worktree metadata, where a clone would cost the whole history again.
//! 2. **Control.** Resetting the sandbox is a checkout inside the mirror, so it is cheap and
//!    needs no second copy to keep in step.
//!
//! libgit2 creates and opens linked worktrees directly (`Repository::worktree`,
//! `Repository::open_from_worktree`), so none of this is hand-written plumbing.
//!
//! The same shape hosts one more thing: a patch's work area. A work area is a linked
//! worktree of the same mirror, registered under the patch's own name rather than under
//! `sandbox`, and it shares the add, reset and prune helpers here so the two cannot drift
//! apart. What differs is only what each is called and what it is cleaned with.
//!
//! The consequence worth stating plainly: a linked worktree holds the object store open, so
//! the mirror must never be pruned or replaced while a sandbox exists. `skot clean` knows
//! this and removes the sandbox first.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::git::Sha;
use crate::git::mirror::Mirror;
use crate::layout::Layout;

/// The name the shared sandbox's worktree is registered under in the mirror.
const WORKTREE_NAME: &str = "sandbox";

/// Percent-encode everything git would refuse in a worktree name.
///
/// A worktree registration name has to survive as a ref-adjacent identifier and as a
/// directory under the mirror's `worktrees/`. Spaces and `/` fail outright, and `~` is
/// refused too, so anything outside `[A-Za-z0-9._-]` becomes `%XX`. The output may
/// contain `%` (git allows it; the probe in the design notes proved the whole scheme
/// against libgit2 directly).
pub fn pct(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for byte in name.bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'_' | b'-' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// The registration name for a session worktree: unique across series.
///
/// Two series can hold patches of the same name, and their work areas are different
/// directories that must be different registrations, so the series goes into the name.
pub fn session_worktree_name(series: &str, dir: &str) -> String {
    format!("{}@{}", pct(series), pct(dir))
}

/// The registration name for the sandbox this layout uses.
///
/// Shared mode registers the one tree as `sandbox`, the name it has always had;
/// per-series mode puts the series in the name so every series' tree can coexist in
/// one mirror. The names differ so the two can never both claim the same registration,
/// which is what lets a mode flip land on top of the other mode's leftover instead of
/// failing. Note the shared *path* is `@shared` but the shared *name* is not
/// `sandbox@shared`: a series really could be called `shared`, and it would then own
/// that registration.
pub fn sandbox_worktree_name(layout: &Layout) -> String {
    if layout.per_series_sandbox() {
        format!("{}@{}", WORKTREE_NAME, pct(layout.series()))
    } else {
        WORKTREE_NAME.to_owned()
    }
}

/// Whether a registration name is a sandbox rather than a session worktree.
fn is_sandbox_name(name: &str) -> bool {
    name == WORKTREE_NAME || name.starts_with(&format!("{WORKTREE_NAME}@"))
}

/// Whether the sandbox is a worktree skot created.
///
/// The `.git` marker, not the directory: a leftover directory that is not a worktree is
/// exactly the wreckage `create` must refuse to pretend is a sandbox, and the message
/// `add` gives for it is the one about an interrupted run.
pub fn is_repository(layout: &Layout) -> bool {
    layout.sandbox_dir().join(".git").exists()
}

/// Create the sandbox if it is not already there, pointing it at `mirror`.
///
/// Idempotent: an existing sandbox is left alone, so this is safe to call before every
/// build. That matters because `persistent` is the default sandbox policy, and the
/// expensive part of a build is dependency state we must not throw away by accident.
pub fn create(layout: &Layout, mirror: &Mirror) -> Result<()> {
    if is_repository(layout) {
        return Ok(());
    }
    // No mode-flip guard: the shared slot and each series' tree are siblings under the
    // sandbox root, so one never sits inside the other and a leftover from the other
    // mode cannot block this create. `clean sandbox` reaps a slot the current mode no
    // longer uses.
    add(
        mirror,
        &sandbox_worktree_name(layout),
        &layout.sandbox_dir(),
        &crate::invocation::skot_args("clean", "sandbox"),
    )
}

/// Register a linked worktree of `mirror` at `dir` under `name`.
///
/// The shared entry point for the sandbox and for per-patch sessions: both are detached
/// checkouts of the mirror that store no objects of their own. `remedy` is the command the
/// leftover-directory message points at, and it differs by caller because the thing that
/// removes it differs.
pub fn add(mirror: &Mirror, name: &str, dir: &Path, remedy: &str) -> Result<()> {
    if !mirror.is_initialised() {
        bail!(
            "no mirror at {}, so there is nothing for `{name}` to check out.\n\
             Run `{}` first.",
            mirror.path().display(),
            crate::invocation::skot("sync")
        );
    }

    // libgit2 creates the worktree directory itself and refuses if it is already there, so
    // an empty leftover is cleared first. A *non-empty* leftover is a different situation and
    // gets its own message rather than a bare "directory exists": the directory holding files
    // with no worktree registered means a previous run was interrupted between creating the
    // directory and registering the worktree.
    if dir.exists() {
        let leftover = std::fs::read_dir(dir)
            .context(format!("reading {}", dir.display()))?
            .next()
            .is_some();
        if leftover {
            bail!(
                "{} has files in it but is not a skot worktree.\n\
                 This usually means an earlier run was interrupted. \
                 `{remedy}` removes it.",
                dir.display(),
            );
        }
        std::fs::remove_dir(dir)
            .context(format!("clearing the empty directory {}", dir.display()))?;
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).context(format!("creating {}", parent.display()))?;
    }

    // `checkout_existing` populates the new worktree from its HEAD. Without it libgit2 adds
    // the worktree and leaves it empty, and the worktree would have to be reset separately.
    let mut options = git2::WorktreeAddOptions::new();
    options.checkout_existing(true);

    mirror
        .repo()
        .worktree(name, dir, Some(&options))
        .context(format!(
            "adding a linked worktree for `{name}` at {}",
            dir.display()
        ))?;

    detach_and_drop_stub_branch(mirror, dir, name)?;
    Ok(())
}

/// Make the new sandbox a detached checkout, and remove the branch libgit2 made for it.
///
/// `Repository::worktree` creates a branch named after the worktree and points the worktree's
/// HEAD at it, so `skot patch apply` would leave a `sandbox` branch sitting in the mirror. Two
/// problems with that: it shows up in `skot check sandbox` and `skot status` as though it
/// were something upstream published, and it *goes stale*, because a sandbox reset detaches
/// HEAD without touching the branch. A stale branch in a mirror is exactly the kind of thing
/// that makes a later run check a series against a commit nobody is looking at.
///
/// A sandbox has no work of its own to commit, so it never needs a branch. Detaching is what
/// `reset_to` wants anyway; doing it here means the branch is removed the moment it exists
/// rather than lingering.
fn detach_and_drop_stub_branch(mirror: &Mirror, dir: &Path, name: &str) -> Result<()> {
    let worktree = mirror
        .repo()
        .find_worktree(name)
        .context(format!("looking up the `{name}` worktree just added"))?;
    let repo = git2::Repository::open_from_worktree(&worktree)
        .context(format!("opening `{name}` at {}", dir.display()))?;

    if let Ok(head) = repo.head()
        && head.name() == Ok(format!("refs/heads/{name}").as_str())
    {
        let target = head
            .target()
            .context("the worktree's own branch does not point at a commit")?;
        repo.set_head_detached(target)
            .context("detaching the worktree, which should not be on a branch")?;
    }

    // `if let`: an earlier version of skot, or git itself, may have left no branch here,
    // and that is fine. This is tidying, not a correctness requirement.
    if let Ok(mut branch) = mirror.repo().find_reference(&format!("refs/heads/{name}")) {
        branch
            .delete()
            .context("removing the placeholder branch the worktree made")?;
    }
    Ok(())
}

/// Open the sandbox as a repository.
///
/// A linked worktree is not a repository in its own right: its `.git` is a file naming the
/// mirror's worktree directory, so opening it means asking the mirror for that worktree and
/// opening through it.
pub fn open(layout: &Layout) -> Result<git2::Repository> {
    let mirror = Mirror::open(layout)?;
    open_named(
        &mirror,
        &sandbox_worktree_name(layout),
        &layout.sandbox_dir(),
        &format!("Run `{}` to create one.", crate::invocation::skot("apply")),
    )
}

/// Open a registered worktree as a repository, by its registration name.
///
/// `remedy` says how to get one when the registration is missing, because the caller knows
/// what will bring it back and this does not.
pub fn open_named(
    mirror: &Mirror,
    name: &str,
    dir: &Path,
    remedy: &str,
) -> Result<git2::Repository> {
    let worktree = mirror.repo().find_worktree(name).context(format!(
        "the mirror at {} has no `{name}` worktree registered.\n{remedy}",
        mirror.path().display()
    ))?;
    git2::Repository::open_from_worktree(&worktree)
        .context(format!("opening `{name}` at {}", dir.display()))
}

/// The commit the sandbox is checked out at.
pub fn head_sha(repo: &git2::Repository) -> Result<Sha> {
    let head = repo.head().context("reading the sandbox HEAD")?;
    let id = head
        .target()
        .context("the sandbox HEAD does not point at a commit")?;
    Ok(id.to_string())
}

/// The ref the sandbox is on, when it can be read without git.
pub fn current_ref(layout: &Layout) -> Option<String> {
    // A linked worktree's HEAD lives in the mirror's worktree directory, not in the
    // sandbox, so this reads it from the mirror. A detached HEAD — which is what a sandbox
    // reset produces — is not a ref name, so it reads as no ref and the caller falls back
    // to the configured default.
    let mirror = Mirror::open(layout).ok()?;
    let worktree = mirror
        .repo()
        .find_worktree(&sandbox_worktree_name(layout))
        .ok()?;
    let text = std::fs::read_to_string(worktree.path().join("HEAD")).ok()?;
    text.strip_prefix("ref: ").map(|r| r.trim().to_owned())
}

/// Total bytes under the sandbox, for `status` and `clean`.
pub fn size(layout: &Layout) -> u64 {
    crate::layout::dir_size(&layout.sandbox_dir())
}

/// The ref the sandbox is on, read through the filesystem only.
///
/// Never fails: a sandbox that is not a repository simply has no ref, and the caller says
/// so rather than erroring.
pub fn try_current_ref(layout: &Layout) -> Result<Option<String>> {
    Ok(current_ref(layout))
}

/// The id of the tree the sandbox's HEAD points at, if it has one.
///
/// Captured *before* a reset so the caller can work out which `keep` paths it is about to
/// delete, and put them back afterwards. Returns an id rather than a `Tree` because a `Tree`
/// borrows the repository, and the caller needs to resolve it against its own handle — which
/// may be opened later, after the reset has already moved HEAD.
pub fn head_tree_oid(layout: &Layout) -> Result<Option<git2::Oid>> {
    let repo = open(layout)?;
    Ok(repo
        .head()
        .ok()
        .and_then(|head| head.target())
        .and_then(|id| repo.find_commit(id).ok())
        .and_then(|c| c.tree().ok())
        .map(|t| t.id()))
}

/// Put back the `keep` paths that checking out `new_tree` removed.
///
/// Separate from [`reset_to`] because a reset is not the only checkout that happens: `apply`
/// resets to the base *and then* checks out the patched tree, and that second checkout removes
/// whatever the first one restored. Restoring only inside the reset means a keep path that the
/// series does not itself carry silently disappears from the sandbox at the end of an apply —
/// which looks exactly like a bug that eats files, because it does.
///
/// Returns the paths it put back.
pub fn restore_kept(
    layout: &Layout,
    old_tree: git2::Oid,
    new_tree: git2::Oid,
    keep: &[String],
) -> Result<Vec<std::path::PathBuf>> {
    if keep.is_empty() {
        return Ok(Vec::new());
    }
    // Both trees are resolved through one handle on purpose: libgit2 refuses to check out an
    // object that came from a different `Repository` instance ("object to checkout does not
    // match repository"), which is what happens if the caller passes a tree it is still
    // holding from its own handle.
    let repo = open(layout)?;
    let old = repo
        .find_tree(old_tree)
        .context(format!("reading the previous sandbox tree ({old_tree})"))?;
    let new = repo
        .find_tree(new_tree)
        .context(format!("reading the new sandbox tree ({new_tree})"))?;

    let survivors = super::diff::keep_survivors(&old, &new, keep);
    if survivors.is_empty() {
        return Ok(Vec::new());
    }
    super::diff::restore_paths(&repo, &old, &survivors).context(format!(
        "restoring keep paths in {}",
        layout.sandbox_dir().display()
    ))
}

/// Check out `sha` into the sandbox, leaving the working tree clean.
///
/// Assumes [`create`] has run. The worktree is detached at `sha` and forced there, which is
/// what makes a reset a reset rather than a merge: the sandbox holds no work of its own that
/// a branch name could imply it should keep.
pub fn reset_to(layout: &Layout, sha: &Sha, keep: &[String]) -> Result<()> {
    let dir = layout.sandbox_dir();
    if !is_repository(layout) {
        bail!("the sandbox at {} does not exist yet.", dir.display());
    }

    let repo = open(layout)?;
    reset_at(&repo, &sandbox_worktree_name(layout), &dir, sha, keep)
}

/// Check out `sha` into a worktree, leaving the working tree clean.
///
/// The shared half of [`reset_to`]: the sandbox and every patch session reset the same way,
/// detached at the target and forced there, which is what makes a reset a reset rather than
/// a merge. `name` and `dir` say which worktree and which registration, so a session can
/// refuse to move a branch in the mirror just as the sandbox does.
pub fn reset_at(
    repo: &git2::Repository,
    name: &str,
    dir: &Path,
    sha: &Sha,
    keep: &[String],
) -> Result<()> {
    // An empty glob would match every path, which would turn `keep` into "keep everything"
    // from a typo in a config file. Refuse rather than silently doing the wrong thing.
    if keep.iter().any(|g| g.trim().is_empty()) {
        bail!("sandbox.keep entries must not be empty: an empty glob would match everything");
    }

    let oid =
        git2::Oid::from_str(sha).map_err(|e| anyhow::anyhow!("{sha} is not a commit id: {e}"))?;
    let commit = repo
        .find_commit(oid)
        .context(format!("the {name} worktree cannot see commit {sha}"))?;
    let tree = commit
        .tree()
        .context(format!("reading the tree of {sha}"))?;

    // What the worktree holds right now, so the paths a `keep` glob covers can be put back
    // afterwards. A force checkout removes tracked files the target tree does not have, and a
    // keep path is by definition one where that is wrong.
    let previous_tree = repo
        .head()
        .ok()
        .and_then(|head| head.target())
        .and_then(|id| repo.find_commit(id).ok())
        .and_then(|c| c.tree().ok());
    let survivors: Vec<PathBuf> = previous_tree
        .as_ref()
        .map(|old| super::diff::keep_survivors(old, &tree, keep))
        .unwrap_or_default();

    // Detach first, then force-checkout. Without the detach the worktree would try to update
    // the branch it is on, and a reset is not allowed to move a branch in the mirror.
    repo.set_head_detached(oid)
        .context(format!("detaching `{name}` at {sha}"))?;

    // A worktree added by git rather than by `add` carries a branch named after itself, and
    // detaching HEAD above would leave it behind, pointing at a commit the worktree no longer
    // has. Remove it so the mirror lists only what upstream actually published.
    if let Ok(mut branch) = repo.find_reference(&format!("refs/heads/{name}")) {
        branch.delete().context(format!(
            "removing the stale `{name}` branch from the mirror"
        ))?;
    }

    // `force` is what makes this a reset rather than a checkout: without it libgit2 refuses to
    // overwrite files it believes were modified, and a worktree full of build output would never
    // converge. `remove_untracked` is left off deliberately, because a keep path like
    // `node_modules` is untracked and removing it would throw away the dependency cache that
    // makes a persistent sandbox worth having.
    let mut options = git2::build::CheckoutBuilder::new();
    options.force();

    // `checkout_tree` takes the object to check out, not the tree. A tree *is* an object, and
    // this is the form that checks out one commit's content.
    let object = tree.clone().into_object();
    repo.checkout_tree(&object, Some(&mut options))
        .context(format!("checking out {sha} into {}", dir.display()))?;

    // Put back the tracked files a keep glob covers that the target tree dropped. Without this
    // step `keep` only ever protected untracked paths, which is the easy half.
    if let Some(old) = previous_tree.as_ref()
        && !survivors.is_empty()
    {
        super::diff::restore_paths(repo, old, &survivors)
            .context(format!("restoring keep paths after the reset to {sha}"))?;
    }

    Ok(())
}

/// Check `tree` out into the sandbox working tree.
///
/// This is the operation `apply`, `push` and `pop` all end with, and it deliberately does not
/// move HEAD: the sandbox stays detached at the upstream commit it was reset to, so a stack
/// applied on top is never mistaken for upstream state. `force` because a sandbox holds build
/// output that libgit2 would otherwise refuse to overwrite, and `remove_untracked` is left off
/// so a dependency cache survives.
pub fn checkout_tree(repo: &git2::Repository, tree: git2::Oid) -> Result<()> {
    let object = repo
        .find_tree(tree)
        .context(format!("reading tree {tree}"))?
        .into_object();
    let mut options = git2::build::CheckoutBuilder::new();
    options.force();
    repo.checkout_tree(&object, Some(&mut options))
        .context(format!("checking out tree {tree}"))?;
    Ok(())
}

/// Verify the sandbox can actually reach the commit it is supposed to be on.
///
/// A linked worktree depends on two links: the mirror must still be a repository, and the
/// worktree must still be registered inside it. Break either and the sandbox stops working,
/// but the failure surfaces much later and much less clearly — as "path is not a repository"
/// or "object not found" in the middle of a build. This checks each link in turn and says which
/// one broke, because the remedy differs: a missing mirror needs a `sync`, and a missing
/// registration needs a fresh sandbox.
pub fn verify_objects(layout: &Layout, mirror: &Mirror, sha: &Sha) -> Result<()> {
    let oid =
        git2::Oid::from_str(sha).map_err(|e| anyhow::anyhow!("{sha} is not a commit id: {e}"))?;

    let broken = |what: String| -> anyhow::Error {
        anyhow::anyhow!(
            "{what}\n\
             The sandbox at {} shares its objects with the mirror at {}, so it cannot work \
             without it.\n\
             Fix it with `{} && {} && {}`.",
            layout.sandbox_dir().display(),
            mirror.path().display(),
            crate::invocation::skot_args("clean", "sandbox"),
            crate::invocation::skot("sync"),
            crate::invocation::skot("apply"),
        )
    };

    let repo = git2::Repository::open_bare(mirror.path()).map_err(|e| {
        broken(format!(
            "the mirror at {} is unusable: {e}",
            mirror.path().display()
        ))
    })?;

    let name = sandbox_worktree_name(layout);
    if repo.find_worktree(&name).is_err() {
        return Err(broken(format!(
            "the mirror at {} has no `{name}` worktree registered, so the sandbox \
             at {} is not a worktree of it.",
            mirror.path().display(),
            layout.sandbox_dir().display()
        )));
    }

    let worktree = repo.find_worktree(&name).expect("just checked");
    let sandbox = git2::Repository::open_from_worktree(&worktree).map_err(|e| {
        broken(format!(
            "the sandbox at {} cannot be opened through the mirror: {e}",
            layout.sandbox_dir().display()
        ))
    })?;

    sandbox.find_object(oid, None).map(|_| ()).map_err(|e| {
        broken(format!(
            "the sandbox at {} cannot read commit {sha}: {e}",
            layout.sandbox_dir().display()
        ))
    })
}

/// Remove the sandbox, unregistering every sandbox worktree from the mirror.
///
/// Every mode's registration goes: the shared `sandbox`, and each per-series
/// `sandbox@<series>`, because `clean sandbox` frees the whole root and a registration
/// left behind is what makes a later create fail with "worktree already exists".
pub fn remove(layout: &Layout) -> Result<()> {
    let root = layout.sandbox_root();
    if let Ok(mirror) = Mirror::open(layout) {
        let names: Vec<String> = mirror
            .repo()
            .worktrees()
            .map(|all| {
                all.iter()
                    .filter_map(|n| n.ok().flatten().map(str::to_owned))
                    .filter(|n| is_sandbox_name(n))
                    .collect()
            })
            .unwrap_or_default();
        for name in names {
            // The directory argument is only for the message and for the remove that
            // follows the unregister: clean frees the whole root, and libgit2's prune
            // treats a missing working tree as fine rather than as an error.
            prune(&mirror, &name, &root)?;
        }
    }
    if root.exists() {
        std::fs::remove_dir_all(&root)
            .context(format!("removing the sandbox at {}", root.display()))?;
    }
    Ok(())
}

/// Unregister a worktree from the mirror and remove its directory.
///
/// The registration has to go, not just the directory: a stale registration is what makes a
/// later [`add`] fail with a confusing "worktree already exists". A missing registration is
/// fine rather than an error, because this is the tidy-up path and it must work when the
/// directory is all that is left.
pub fn prune(mirror: &Mirror, name: &str, dir: &Path) -> Result<()> {
    if let Ok(worktree) = mirror.repo().find_worktree(name) {
        // `valid` and `working_tree` because the worktree is skot's to discard, and a dirty
        // one is the normal case rather than a reason to refuse.
        let mut options = git2::WorktreePruneOptions::new();
        options.valid(true).working_tree(true);
        worktree.prune(Some(&mut options)).context(format!(
            "unregistering the `{name}` worktree for {}",
            dir.display()
        ))?;
    }
    if dir.exists() {
        std::fs::remove_dir_all(dir)
            .context(format!("removing the worktree directory {}", dir.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{DEFAULT_SERIES, Layout};

    /// A bare mirror containing `files`, plus the commit id of `HEAD`.
    ///
    /// Built with git2 alone, because the thing under test is a worktree of a bare mirror:
    /// a fixture created by shelling out to `git` would be a different shape and would
    /// quietly stop testing the linked-worktree path.
    fn mirror_with_files(root: &Path, files: &[(&str, &str)]) -> (Layout, Sha) {
        let layout = Layout::new(root);
        std::fs::create_dir_all(layout.mirror_dir()).expect("mirror parent");
        let repo = git2::Repository::init_bare(layout.mirror_dir()).expect("a bare mirror");

        // Build the tree by hand. A fresh bare repo has only the empty tree, so this tests
        // the "first checkout into a fresh sandbox" path, not a diff.
        let mut builder = repo.treebuilder(None).expect("tree builder");
        for (path, content) in files {
            let oid = repo.blob(content.as_bytes()).expect("write blob");
            builder.insert(*path, oid, 0o100644).expect("insert");
        }
        let tree_id = builder.write().expect("write tree");

        // A bare repo has no user configured, and `Repository::commit` refuses to guess an
        // identity. Name it explicitly so the fixture does not depend on ambient git config
        // in whatever environment the suite runs in.
        let who = git2::Signature::new("test", "test@example.com", &git2::Time::new(0, 0)).unwrap();
        let tree = repo.find_tree(tree_id).expect("find tree");
        let sha = repo
            .commit(None, &who, &who, "one", &tree, &[])
            .expect("write commit")
            .to_string();

        // Point HEAD at the commit so `worktree add` has a branch-ish name to base on.
        repo.set_head_detached(git2::Oid::from_str(&sha).unwrap())
            .unwrap();

        (layout, sha)
    }

    fn mirror_with_one_commit(dir: &Path) -> (Layout, Sha) {
        mirror_with_files(dir, &[("a.txt", "one\n")])
    }

    #[test]
    fn the_sandbox_is_a_worktree_of_the_mirror_and_stores_no_objects() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, sha) = mirror_with_one_commit(tmp.path());
        let mirror = Mirror::open(&layout).unwrap();

        create(&layout, &mirror).unwrap();

        // The claim that matters: the sandbox owns no object store of its own. A linked
        // worktree's `.git` is a file, not a directory.
        let dot_git = layout.sandbox_dir().join(".git");
        assert!(
            !dot_git.join("objects").exists(),
            "a linked worktree must not have its own objects directory"
        );
        assert!(
            dot_git.is_file(),
            "a linked worktree's .git should be a file naming the mirror"
        );
        // libgit2 writes `gitdir: <path>`, the same form git does. What matters is that the path
        // is absolute: a relative one would break the moment anything changed directory, and the
        // breakage would look like "object not found" rather than "bad path".
        let text = std::fs::read_to_string(&dot_git).unwrap();
        let gitdir = text
            .trim()
            .strip_prefix("gitdir:")
            .unwrap_or_else(|| panic!("unexpected .git file: {}", text.trim()))
            .trim();
        assert!(
            Path::new(gitdir).is_absolute(),
            "a relative gitdir breaks on cd: {gitdir}"
        );

        // And it really can read the mirror's objects, which is the point.
        verify_objects(&layout, &mirror, &sha).unwrap();
        let repo = open(&layout).unwrap();
        assert_eq!(head_sha(&repo).unwrap(), sha);
    }

    #[test]
    fn the_sandbox_does_not_leave_a_branch_behind_in_the_mirror() {
        // `Repository::worktree` creates a branch named after the worktree and points the
        // worktree's HEAD at it. That branch would be listed as though upstream published it,
        // and it would go stale on the first reset, so `check sandbox` could pass against a
        // commit nobody is looking at.
        let tmp = tempfile::tempdir().unwrap();
        let (layout, sha) = mirror_with_one_commit(tmp.path());
        let mirror = Mirror::open(&layout).unwrap();

        create(&layout, &mirror).unwrap();

        assert!(
            mirror.refs().unwrap().branches.is_empty(),
            "the mirror should list only upstream branches, not the sandbox's own"
        );
        assert!(
            !mirror.repo().find_reference("refs/heads/sandbox").is_ok(),
            "the placeholder branch should be gone as soon as the worktree exists"
        );

        // And a reset, which is what would leave it stale, keeps it gone.
        reset_to(&layout, &sha, &[]).unwrap();
        assert!(
            mirror.refs().unwrap().branches.is_empty(),
            "a reset must not leave a stale sandbox branch for `{}` to find",
            crate::invocation::skot_args("check", "sandbox")
        );
    }

    #[test]
    fn reset_checks_out_the_target_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, sha) = mirror_with_one_commit(tmp.path());
        let mirror = Mirror::open(&layout).unwrap();
        create(&layout, &mirror).unwrap();

        reset_to(&layout, &sha, &[]).unwrap();

        assert_eq!(
            std::fs::read_to_string(layout.sandbox_dir().join("a.txt")).unwrap(),
            "one\n"
        );
    }

    #[test]
    fn a_keep_glob_protects_a_tracked_file_the_target_tree_dropped() {
        // The other half of `keep`. An untracked cache survives because nothing removes it;
        // a *tracked* file that upstream deleted is removed by the force checkout, and a keep
        // glob has to put it back or the glob means nothing for the case that needs it.
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = mirror_with_files(tmp.path(), &[("a.txt", "one\n")]);
        let mirror = Mirror::open(&layout).unwrap();
        create(&layout, &mirror).unwrap();
        reset_to(&layout, &_sha, &[]).unwrap();

        let repo = git2::Repository::open_bare(layout.mirror_dir()).unwrap();
        let who = git2::Signature::new("test", "test@example.com", &git2::Time::new(0, 0)).unwrap();
        let empty = repo.treebuilder(None).unwrap().write().unwrap();
        let dropped = repo
            .commit(
                None,
                &who,
                &who,
                "drop a.txt",
                &repo.find_tree(empty).unwrap(),
                &[],
            )
            .unwrap();
        drop(repo);

        reset_to(&layout, &dropped.to_string(), &["a.txt".to_owned()]).unwrap();
        assert_eq!(
            std::fs::read_to_string(layout.sandbox_dir().join("a.txt")).unwrap(),
            "one\n",
            "a keep glob must keep a tracked file the new tree dropped"
        );

        // And a path no glob names is still removed, or `keep` would protect everything.
        // This needs its own commit: each fixture has its own mirror, and an object only
        // exists in the store that wrote it.
        let tmp2 = tempfile::tempdir().unwrap();
        let (layout2, _sha2) = mirror_with_files(tmp2.path(), &[("a.txt", "one\n")]);
        let mirror2 = Mirror::open(&layout2).unwrap();
        create(&layout2, &mirror2).unwrap();
        reset_to(&layout2, &_sha2, &[]).unwrap();
        let repo2 = git2::Repository::open_bare(layout2.mirror_dir()).unwrap();
        let empty2 = repo2.treebuilder(None).unwrap().write().unwrap();
        let dropped2 = repo2
            .commit(
                None,
                &who,
                &who,
                "drop a.txt",
                &repo2.find_tree(empty2).unwrap(),
                &[],
            )
            .unwrap();
        drop(repo2);

        reset_to(
            &layout2,
            &dropped2.to_string(),
            &["something-else".to_owned()],
        )
        .unwrap();
        assert!(
            !layout2.sandbox_dir().join("a.txt").exists(),
            "an unmatched keep glob must not keep everything alive"
        );
    }

    #[test]
    fn an_empty_keep_glob_is_refused() {
        // A typo in a config file would otherwise turn `keep` into "keep the entire tree".
        let tmp = tempfile::tempdir().unwrap();
        let (layout, sha) = mirror_with_files(tmp.path(), &[("a.txt", "one\n")]);
        let mirror = Mirror::open(&layout).unwrap();
        create(&layout, &mirror).unwrap();

        let err = reset_to(&layout, &sha, &["".to_owned()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("must not be empty"), "got {err}");
    }

    #[test]
    fn reset_removes_a_stale_tracked_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = mirror_with_files(tmp.path(), &[("a.txt", "one\n")]);
        let mirror = Mirror::open(&layout).unwrap();
        create(&layout, &mirror).unwrap();
        reset_to(&layout, &_sha, &[]).unwrap();

        // A second commit that drops `a.txt`. Resetting to it must take the file away,
        // otherwise the sandbox would keep showing a file the target tree does not have.
        let repo = git2::Repository::open_bare(layout.mirror_dir()).unwrap();
        let who = git2::Signature::new("test", "test@example.com", &git2::Time::new(0, 0)).unwrap();
        let empty = repo.treebuilder(None).unwrap().write().unwrap();
        let commit = repo
            .commit(
                None,
                &who,
                &who,
                "drop a.txt",
                &repo.find_tree(empty).unwrap(),
                &[],
            )
            .unwrap();
        drop(repo);

        reset_to(&layout, &commit.to_string(), &[]).unwrap();

        assert!(
            !layout.sandbox_dir().join("a.txt").exists(),
            "a tracked file the target tree dropped must not survive a reset"
        );
    }

    #[test]
    fn reset_keeps_an_untracked_dependency_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, sha) = mirror_with_one_commit(tmp.path());
        let mirror = Mirror::open(&layout).unwrap();
        create(&layout, &mirror).unwrap();
        let keep = ["node_modules/**".to_owned()];
        reset_to(&layout, &sha, &keep).unwrap();

        // This is the whole point of a persistent sandbox: a dependency cache the target
        // tree knows nothing about has to survive a reset.
        let cache = layout.sandbox_dir().join("node_modules/dep/index.js");
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
        std::fs::write(&cache, b"module.exports = {};").unwrap();

        reset_to(&layout, &sha, &keep).unwrap();

        assert!(
            cache.exists(),
            "reset deleted a keep path; that is the dependency cache the sandbox exists to hold"
        );
    }

    #[test]
    fn reset_does_not_move_a_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, sha) = mirror_with_one_commit(tmp.path());
        let mirror = Mirror::open(&layout).unwrap();
        create(&layout, &mirror).unwrap();

        // A branch the sandbox is not on, which a non-detached checkout would try to move.
        let repo = git2::Repository::open_bare(layout.mirror_dir()).unwrap();
        let oid = git2::Oid::from_str(&sha).unwrap();
        repo.branch("release", &repo.find_commit(oid).unwrap(), false)
            .unwrap();
        let before = repo.find_reference("refs/heads/release").unwrap().target();
        std::mem::forget(repo);

        reset_to(&layout, &sha, &[]).unwrap();

        let repo = git2::Repository::open_bare(layout.mirror_dir()).unwrap();
        assert_eq!(
            repo.find_reference("refs/heads/release").unwrap().target(),
            before,
            "a sandbox reset must not move a branch in the mirror"
        );
        assert!(
            open(&layout).unwrap().head_detached().unwrap_or(false),
            "the sandbox should be detached, not on a branch"
        );
    }

    #[test]
    fn create_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = mirror_with_one_commit(tmp.path());
        let mirror = Mirror::open(&layout).unwrap();
        create(&layout, &mirror).unwrap();

        // Something a build left behind must survive a second create.
        let marker = layout.sandbox_dir().join("build-artifact");
        std::fs::write(&marker, b"keep me").unwrap();

        create(&layout, &mirror).unwrap();
        assert!(
            marker.exists(),
            "create overwrote state from a previous build"
        );
    }

    #[test]
    fn a_missing_mirror_is_refused_before_anything_is_created() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path());
        // No mirror at all, so `Mirror::open` cannot succeed; construct the failure path.
        assert!(!layout.sandbox_dir().exists());
        let Err(err) = Mirror::open(&layout) else {
            panic!("no mirror here, so opening must fail");
        };
        assert!(err.to_string().contains("opening the mirror"), "{err}");
    }

    #[test]
    fn verify_reports_a_broken_sandbox_clearly() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = mirror_with_one_commit(tmp.path());
        let mirror = Mirror::open(&layout).unwrap();
        create(&layout, &mirror).unwrap();

        // Resolve before breaking anything: after the objects are gone the mirror cannot
        // answer this question either, and the test would fail on the fixture rather than
        // on the verification it is meant to exercise.
        let missing = mirror.resolve("HEAD").unwrap();

        // Simulate the mirror losing its objects while the sandbox survives.
        std::fs::remove_dir_all(mirror.objects_dir()).unwrap();

        let err = verify_objects(&layout, &mirror, &missing)
            .expect_err("a sandbox with no reachable objects must not look healthy")
            .to_string();
        assert!(err.contains("mirror"), "must name what broke: {err}");
        assert!(
            err.contains(&crate::invocation::skot_args("clean", "sandbox")),
            "no remedy offered: {err}"
        );
    }

    #[test]
    fn verify_reports_a_missing_worktree_registration() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, sha) = mirror_with_one_commit(tmp.path());
        let mirror = Mirror::open(&layout).unwrap();
        create(&layout, &mirror).unwrap();

        // The sandbox directory survives but its registration in the mirror does not, which is
        // what an interrupted `clean` looks like. `prune` refuses to remove a valid working
        // tree, so remove the registration directly, as the interruption would.
        std::fs::remove_dir_all(mirror.path().join("worktrees").join(WORKTREE_NAME)).unwrap();

        let err = verify_objects(&layout, &mirror, &sha)
            .expect_err("an unregistered sandbox must not look healthy")
            .to_string();
        assert!(err.contains("no `sandbox` worktree registered"), "{err}");
        assert!(
            err.contains(&crate::invocation::skot_args("clean", "sandbox")),
            "no remedy offered: {err}"
        );
    }

    #[test]
    fn remove_unregisters_the_worktree_so_create_can_run_again() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, sha) = mirror_with_one_commit(tmp.path());
        let mirror = Mirror::open(&layout).unwrap();
        create(&layout, &mirror).unwrap();
        reset_to(&layout, &sha, &[]).unwrap();

        remove(&layout).unwrap();
        assert!(!layout.sandbox_dir().exists());

        // A stale registration would make this fail with "worktree already exists".
        create(&layout, &Mirror::open(&layout).unwrap()).unwrap();
        assert!(layout.sandbox_dir().exists());
    }

    #[test]
    fn remove_is_a_no_op_when_there_is_no_sandbox() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = mirror_with_one_commit(tmp.path());
        remove(&layout).expect("removing nothing must not error");
    }

    #[test]
    fn a_second_worktree_can_be_added_reset_and_pruned_under_its_own_name() {
        // The session shape: another linked worktree of the same mirror, named after a
        // patch instead of `sandbox`, obeying the same rules (no branch, detached reset,
        // registration gone when it goes).
        let tmp = tempfile::tempdir().unwrap();
        let (layout, sha) = mirror_with_one_commit(tmp.path());
        let mirror = Mirror::open(&layout).unwrap();
        let dir = layout.work_patch_dir("0001-a.patch").unwrap();

        add(
            &mirror,
            "0001-a.patch",
            &dir,
            &crate::invocation::skot_args("clean", "--sessions"),
        )
        .unwrap();

        assert!(dir.join(".git").is_file(), "a linked worktree, not a clone");
        let refs = mirror.refs().unwrap();
        let branches: Vec<&str> = refs.branches.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            !branches.contains(&"0001-a.patch"),
            "a named worktree must not leave a branch behind either"
        );

        let repo = open_named(
            &mirror,
            "0001-a.patch",
            &dir,
            &crate::invocation::skot_args("clean", "--sessions"),
        )
        .unwrap();
        reset_at(&repo, "0001-a.patch", &dir, &sha, &[]).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "one\n",
            "the reset should check the target tree out"
        );
        assert!(
            repo.head_detached().unwrap(),
            "a session is detached, never on a branch"
        );

        prune(&mirror, "0001-a.patch", &dir).unwrap();
        assert!(!dir.exists());
        // And the registration went with it: re-adding works rather than refusing.
        add(&mirror, "0001-a.patch", &dir, "remedy").unwrap();
    }

    #[test]
    fn a_non_empty_leftover_is_refused_with_the_remedy_it_was_given() {
        // The message differs by worktree (the sandbox is cleaned one way, a session
        // another), which is the whole reason `remedy` is a parameter.
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = mirror_with_one_commit(tmp.path());
        let mirror = Mirror::open(&layout).unwrap();
        let dir = layout.work_patch_dir("0001-a.patch").unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("leftover"), b"interrupted").unwrap();

        let remedy = crate::invocation::skot_args("clean", "--sessions");
        let err = add(&mirror, "0001-a.patch", &dir, &remedy)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a skot worktree"), "got {err}");
        assert!(
            err.contains(&remedy),
            "the caller's remedy must appear: {err}"
        );
    }

    #[test]
    fn worktree_names_survive_the_characters_series_names_allow() {
        // Proved against libgit2 directly before it was written here: spaces and `/`
        // fail as registration names, so anything outside `[A-Za-z0-9._-]` is encoded.
        assert_eq!(pct("plain"), "plain");
        assert_eq!(pct("my series"), "my%20series");
        assert_eq!(pct("a/b"), "a%2Fb");
        assert_eq!(
            session_worktree_name("my series", "sub/0001-a.patch"),
            "my%20series@sub%2F0001-a.patch"
        );
        assert_eq!(sandbox_worktree_name(&Layout::new(".")), "sandbox");
        let per = Layout::new(".").with_per_series_sandbox(true);
        assert_eq!(
            sandbox_worktree_name(&per.with_series("exp").unwrap()),
            "sandbox@exp"
        );
    }

    #[test]
    fn a_per_series_sandbox_coexists_with_the_shared_slot() {
        // The two scopes are siblings under the root: a slot left by the other mode
        // neither sits inside the new tree's path nor claims its registration name, so
        // the create goes through and `clean sandbox` reaps what the mode no longer uses.
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = mirror_with_one_commit(tmp.path());
        let mirror = Mirror::open(&layout).unwrap();
        create(&layout, &mirror).unwrap();
        assert!(layout.sandbox_dir().ends_with("@shared"));

        let per = layout.with_per_series_sandbox(true);
        create(&per, &mirror).unwrap();
        assert!(per.sandbox_dir().ends_with(DEFAULT_SERIES));
        assert!(per.sandbox_dir().exists());
        // The slot survives the other scope's create.
        assert!(layout.sandbox_dir().exists());

        let names: Vec<String> = mirror
            .repo()
            .worktrees()
            .unwrap()
            .iter()
            .filter_map(|n| n.ok().flatten().map(str::to_owned))
            .filter(|n| is_sandbox_name(n))
            .collect();
        assert_eq!(names.len(), 2, "got {names:?}");
    }
}
