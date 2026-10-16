//! Authoring sessions: one worktree per patch, under `.skot/work/`.
//!
//! This is the other half of the sandbox doctrine. The sandbox is derived state: like
//! `node_modules`, rebuilt from the series at any moment and safe to throw away. What is
//! not disposable is the work in progress, the edits not yet saved into a patch. So
//! authoring does not happen in the sandbox; it happens in a linked worktree of its own,
//! one per patch, recorded in `.skot/work/sessions.json`.
//!
//! Three properties follow from that split, and each closes a bug the shared sandbox had:
//!
//! - **A session names its patch.** The old loop targeted "the patch on top of the applied
//!   set", so `new --at 2` followed by a bare `edit-save` exported into the *tail* patch
//!   instead. Here a session has exactly one patch, or a command has the list of sessions
//!   to pick from.
//! - **A session records its base.** `base_sha` is the commit it opened on, and
//!   `base_tree` the tree the patches below produced then. `edit-save` diffs against that
//!   record, so editing the series below the patch cannot silently move what its edits are
//!   measured against; a mismatch is refused and named rather than written.
//! - **A session touches neither `state.json` nor the sandbox.** The old `new` wrote the
//!   new patch into the applied set and reset the sandbox, which is how a later `pop` and
//!   `edit` could clobber a different patch's contents. Nothing here reads or writes shared
//!   run state.
//!
//! Every mutation assumes the caller holds [`Layout::lock`]: the manifest is exactly the
//! kind of state two runs must not interleave, and taking the lock here as well would fail
//! against callers that already hold it, since a second lock in one process errors rather
//! than nests.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::check;
use crate::commands::authoring;
use crate::git::{Sha, diff, mirror::Mirror, sandbox};
use crate::invocation::skot_args;
use crate::layout::{DEFAULT_SERIES, Layout};
use crate::series::Entry;

/// One patch's open editing session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// The series this session belongs to.
    ///
    /// A manifest is per project and can hold open work areas in every series at once,
    /// and two series can hold patches of the same name, so a patch name alone is not
    /// an identity. An entry written before this field existed belongs to the default
    /// series, which is the only series such an entry could have been opened in.
    #[serde(default)]
    pub series: String,
    /// The series entry's name at the moment the session opened.
    ///
    /// Not stable: `new --at`, `move` and `drop` renumber the tail, so a session opened as
    /// `0001-a.patch` is on `0002-a.patch` once a patch lands in front of it. What follows
    /// the rename is this field; what does not move is [`Session::dir`], because the
    /// worktree is registered under a name git will not follow.
    pub patch: String,
    /// The worktree directory's name under `.skot/work/<series>/`, fixed when the session
    /// opened.
    ///
    /// This is the one thing that never changes for the life of a session. The directory is
    /// a linked worktree registered in the mirror under a name, and git has no operation
    /// that renames a registration in place: moving it would mean prunes and re-adds, each
    /// one a chance to lose the edits the session exists to hold. So the path is stable and
    /// the *name* follows the series, which is the direction the user sees.
    #[serde(default)]
    pub dir: String,
    /// The ref the session opened on, kept for messages and for later advice.
    pub ref_name: String,
    /// The commit `ref_name` pointed at when the session opened.
    pub base_sha: Sha,
    /// The tree the patches below this one produced when the session opened, as an oid.
    ///
    /// This is the drift guard's anchor: `edit-save` replays the series below against
    /// `base_sha` and refuses if that replay no longer reaches this tree, because the
    /// patch's recorded base has moved out from under it.
    pub base_tree: String,
}

impl Session {
    /// The series this session belongs to, with the default series as the fallback for an
    /// entry written before the field existed.
    pub fn series_name(&self) -> &str {
        if self.series.is_empty() {
            DEFAULT_SERIES
        } else {
            &self.series
        }
    }

    /// The worktree directory's name under the series' work directory: fixed when the
    /// session opened, with the patch name as the fallback for a record written before
    /// `dir` existed.
    pub fn dir_name(&self) -> &str {
        if self.dir.is_empty() {
            &self.patch
        } else {
            &self.dir
        }
    }

    /// The worktree directory's full path: the session's own series, not whichever series
    /// happens to be active when the manifest is read.
    pub fn dir_path(&self, layout: &Layout) -> Result<std::path::PathBuf> {
        layout.work_patch_dir_for(self.series_name(), self.dir_name())
    }

    /// The registration name this session's worktree holds in the mirror.
    pub fn worktree_name(&self) -> String {
        sandbox::session_worktree_name(self.series_name(), self.dir_name())
    }
}

/// The sessions currently open, as `.skot/work/sessions.json`.
///
/// Read and written whole, write-then-rename, so an interrupted save cannot leave a
/// half-parsed manifest behind. An entry that will not parse with the rest is a corrupt
/// manifest, which is an error rather than a silent empty default: defaulting here would
/// forget sessions whose directories still hold edits, and the next tidy-up would delete
/// them as unrecorded.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Manifest {
    pub sessions: Vec<Session>,
}

impl Manifest {
    pub fn load(layout: &Layout) -> Result<Self> {
        let path = layout.sessions_file();
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).context(format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).context(format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, layout: &Layout) -> Result<()> {
        std::fs::create_dir_all(layout.work_dir())
            .context(format!("creating {}", layout.work_dir().display()))?;
        let path = layout.sessions_file();
        let text = serde_json::to_string_pretty(self)?;
        // Write-then-rename, the same shape as `state.json`: a crash mid-save leaves the
        // previous manifest intact rather than a truncated one.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text).context(format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).context(format!("writing {}", path.display()))?;
        Ok(())
    }

    /// The session open on `patch` in the layout's series, if there is one.
    pub fn get(&self, layout: &Layout, patch: &str) -> Option<&Session> {
        self.sessions
            .iter()
            .find(|s| s.series_name() == layout.series() && s.patch == patch)
    }

    /// Every open patch in the layout's series, in the order they were opened.
    pub fn patches(&self, layout: &Layout) -> Vec<&str> {
        self.sessions
            .iter()
            .filter(|s| s.series_name() == layout.series())
            .map(|s| s.patch.as_str())
            .collect()
    }

    /// Record a session, replacing any entry already under that series and patch.
    pub fn insert(&mut self, session: Session) {
        match self
            .sessions
            .iter_mut()
            .find(|s| s.series_name() == session.series_name() && s.patch == session.patch)
        {
            Some(existing) => *existing = session,
            None => self.sessions.push(session),
        }
    }

    /// Forget the session on `patch` in the layout's series. Returns whether there was one.
    pub fn remove(&mut self, layout: &Layout, patch: &str) -> bool {
        let Some(at) = self
            .sessions
            .iter()
            .position(|s| s.series_name() == layout.series() && s.patch == patch)
        else {
            return false;
        };
        self.sessions.remove(at);
        true
    }

    /// Follow a tail renumbering, the way [`crate::state::State::follow_renames`] does for
    /// the applied set: the record's patch name changes to whatever the series now calls
    /// it, and the worktree stays exactly where it is.
    ///
    /// Only the layout's own series is touched: a renumbering renames files in the active
    /// series, and another series' same-named patch has nothing to do with it.
    ///
    /// Without this, `new --at` or `move` in front of an open patch leaves the manifest
    /// naming a patch the series no longer has: `list` loses the `(unsaved)` marker,
    /// `edit-save` finds no work area, and the next `reconcile` treats the worktree as
    /// an orphan and deletes the edits.
    pub fn follow_renames(&mut self, layout: &Layout, renames: &[(String, String)]) {
        for (from, to) in renames {
            if let Some(session) = self
                .sessions
                .iter_mut()
                .find(|s| s.series_name() == layout.series() && s.patch == *from)
            {
                session.patch = to.clone();
            }
        }
    }
}

/// Bring `.skot/work/` and the manifest back into agreement. The caller holds the lock.
///
/// Both sides can outlive a crash: a worktree directory whose manifest entry never got
/// written, or an entry whose directory was deleted outside skot. Neither is a decision
/// anyone was told about, so both are discarded rather than repaired. A session skot
/// cannot open is not a session, and a directory skot did not record is not authoring
/// work; what *is* recorded and present is left exactly as it is, edits and all.
///
/// A work area that matches its patch is *not* retired here. It has nothing unsaved in it,
/// but it is where the user was: deleting it because they ran `check` in between would pull
/// the directory out from under an open editor. Being clean costs nothing: [`unsaved`]
/// decides what counts as a complaint, and only unsaved work areas ever block anything.
/// `edit-save` retires one when it saves it, and `clean` reaps the rest
/// as derived state.
pub fn reconcile(layout: &Layout) -> Result<Manifest> {
    let mut manifest = Manifest::load(layout)?;
    let mut changed = false;

    // Entries whose directory is gone (or could never be one, if the name cannot be a
    // directory at all). Any registration the deletion left behind goes with them, or the
    // next attempt to open a worktree under that name would refuse with "already exists".
    let stale: Vec<Session> = manifest
        .sessions
        .iter()
        .filter(|s| s.dir_path(layout).map(|d| !d.exists()).unwrap_or(true))
        .cloned()
        .collect();
    for session in stale {
        if let Ok(dir) = session.dir_path(layout)
            && let Ok(mirror) = Mirror::open(layout)
        {
            sandbox::prune(&mirror, &session.worktree_name(), &dir)?;
        }
        manifest.remove(layout, &session.patch);
        changed = true;
    }

    // Directories nobody recorded, at any depth under the work directory: the per-series
    // level, and the patch's own subdirectory inside it. A directory that *contains* a
    // recorded session is not an orphan: a patch name may carry a subdirectory
    // (`sub/0002-fix.patch`), and deleting the parent to tidy an "orphan" would delete
    // the session inside it, so a kept directory is walked into rather than skipped.
    let recorded: Vec<std::path::PathBuf> = manifest
        .sessions
        .iter()
        .filter_map(|s| s.dir_path(layout).ok())
        .collect();
    fn sweep(
        layout: &Layout,
        dir: &std::path::Path,
        recorded: &[std::path::PathBuf],
        changed: &mut bool,
    ) -> Result<()> {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                if recorded.iter().any(|r| r == &path || r.starts_with(&path)) {
                    sweep(layout, &path, recorded, changed)?;
                    continue;
                }
                layout.guarded_remove(&path)?;
                *changed = true;
            }
        }
        Ok(())
    }
    sweep(layout, &layout.work_dir(), &recorded, &mut changed)?;

    if changed {
        manifest.save(layout)?;
    }
    Ok(manifest)
}

/// What a session needs, computed before anything on disk changes.
#[derive(Debug)]
pub struct Plan {
    /// The ref the base resolved from, kept for the record and for later advice.
    pub ref_name: String,
    /// The commit that ref pointed at.
    pub base_sha: Sha,
    /// The tree the patches below the target produced: where its edits count from.
    pub base_tree: git2::Oid,
    /// `base_tree` with the target's current content on top: what the worktree should hold.
    pub work_tree: git2::Oid,
}

/// Resolve the ref and replay the series to the trees a session needs. Nothing is written.
///
/// `entries_below` are the enabled patches *under* the one being opened; `self_entry` is
/// that patch's own file, when it has content worth replaying. `None` stands for a patch
/// with nothing in it yet, which is the normal case for `new` and changes nothing, so
/// `work_tree` stays at `base_tree`.
///
/// The replay runs on the mirror, before any worktree exists: a series that does not apply
/// is the failure this is here to catch, and failing before the first write is what keeps
/// a run's promise that nothing was created. The ref resolves through the sync choke point
/// on the way, so a mirror that already has it pays no network and one that does not
/// fetches it rather than sending the user elsewhere first.
pub fn plan(
    layout: &Layout,
    ref_name: &str,
    entries_below: &[Entry],
    self_entry: Option<&Entry>,
) -> Result<Plan> {
    let (mirror, base_sha) = crate::commands::sync::resolve_in_mirror(layout, ref_name)?;

    let mut whole: Vec<Entry> = entries_below.to_vec();
    whole.extend(self_entry.cloned());
    let applied = check::replay_series(mirror.repo(), &base_sha, &whole).context(format!(
        "replaying {} patch(es) onto {ref_name}",
        whole.len()
    ))?;
    if !applied.report.ok {
        bail!(
            "the series does not apply to {ref_name}.\n\
             {}\n\
             Nothing was written.",
            applied.report.failure_line()
        );
    }

    let base_tree = if entries_below.is_empty() {
        crate::git::commit_tree(mirror.repo(), &base_sha)
            .context(format!("reading the tree of {base_sha}"))?
            .id()
    } else {
        applied.per_patch[entries_below.len() - 1]
            .context("the replay recorded no tree for the patch below")?
    };

    Ok(Plan {
        ref_name: ref_name.to_owned(),
        base_sha,
        base_tree,
        work_tree: applied.tree,
    })
}

/// Create the session's worktree from `plan` and record it. The caller holds the lock.
///
/// The order is deliberate and mirrors [`plan`]'s: everything that can fail because of the
/// series has already been checked, so what is left is the worktree itself. The manifest is
/// written last, because a session skot cannot open is not a session: an entry for a
/// worktree that was never finished would strand the user in a dead directory, while a
/// worktree with no entry is recognised as a husk and replaced by the next attempt.
pub fn materialise(layout: &Layout, patch: &str, plan: &Plan) -> Result<Session> {
    let dir = layout.work_patch_dir(patch)?;
    let name = sandbox::session_worktree_name(layout.series(), patch);
    let mut manifest = Manifest::load(layout)?;
    if manifest.get(layout, patch).is_some() {
        bail!(
            "`{patch}` already has a work area at {}.\n\
             `{}` saves what is in it.",
            dir.display(),
            skot_args("edit-save", patch),
        );
    }

    let mirror = Mirror::open(layout)?;
    // Nothing recorded this name, so anything registered or standing under it is the husk
    // of an interrupted run rather than work someone was told about. Clearing it here is
    // what keeps one half-finished attempt from making every later one refuse.
    sandbox::prune(&mirror, &name, &dir)?;
    sandbox::add(&mirror, &name, &dir, &skot_args("clean", "--sessions"))?;
    let repo = sandbox::open_named(
        &mirror,
        &name,
        &dir,
        &format!(
            "`{}` clears it so `{}` can open a fresh one.",
            skot_args("clean", "--sessions"),
            skot_args("edit", patch)
        ),
    )?;

    // Detach on the recorded base, then check the patched tree out on top of it: HEAD is
    // what the patch sits *on*, the working tree is base plus the patch's content, and the
    // difference between them is exactly what a session edits.
    sandbox::reset_at(&repo, &name, &dir, &plan.base_sha, &[])?;
    sandbox::checkout_tree(&repo, plan.work_tree)?;

    let session = Session {
        series: layout.series().to_owned(),
        patch: patch.to_owned(),
        dir: patch.to_owned(),
        ref_name: plan.ref_name.clone(),
        base_sha: plan.base_sha.clone(),
        base_tree: plan.base_tree.to_string(),
    };
    manifest.insert(session.clone());
    manifest.save(layout)?;
    Ok(session)
}

/// Open an existing session's worktree as a repository.
///
/// Read-only, so no lock: the manifest and a registered worktree are only ever changed
/// under one.
pub fn open(layout: &Layout, session: &Session) -> Result<git2::Repository> {
    let mirror = Mirror::open(layout)?;
    let dir = session.dir_path(layout)?;
    sandbox::open_named(
        &mirror,
        &session.worktree_name(),
        &dir,
        &format!(
            "`{}` clears it so `{}` can open a fresh one.",
            skot_args("clean", "--sessions"),
            skot_args("edit", &session.patch)
        ),
    )
}

/// Close a session: drop its registration, its directory, and its record. The caller holds
/// the lock.
///
/// What goes is the worktree, not the patch: the file and the series line stay exactly as
/// they are. Anything the worktree held that was never saved goes with it, which is why
/// `edit` checks for exactly that first and why `--yes` is the thing that overrides it.
/// The manifest is written last, so an interrupted end leaves a record that still points
/// at a worktree (recognised, retried) rather than a directory nobody owns (deleted as a
/// husk, edits and all).
pub fn end(layout: &Layout, session: &Session) -> Result<()> {
    let mirror = Mirror::open(layout)?;
    let dir = session.dir_path(layout)?;
    sandbox::prune(&mirror, &session.worktree_name(), &dir)?;
    let mut manifest = Manifest::load(layout)?;
    // By series and directory, not by patch name: a renumbering may have renamed the
    // record since the caller resolved it, and the pair is the one identity that never
    // moves. The series half matters too: two series can hold a patch of the same name
    // with its own work area, and ending one must not forget the other.
    manifest.sessions.retain(|s| {
        !(s.series_name() == session.series_name() && s.dir_name() == session.dir_name())
    });
    manifest.save(layout)?;
    Ok(())
}

/// Whether the work area holds changes the patch file does not.
///
/// The same comparison `edit-save` makes before writing: export the worktree against the
/// recorded base and compare the body with what is on disk, ignoring the header, because
/// re-rendering it normalises things the author never wrote. This is the one thing that
/// makes a work area worth keeping: a work area that compares equal has nothing unsaved
/// in it, and [`reconcile`] leaves it alone while `clean` reaps it as derived state.
pub fn unsaved(layout: &Layout, session: &Session) -> Result<bool> {
    // The patch file of the session's own series, not of whichever series is active:
    // `clean` and `list` compare every session in the manifest.
    let patch_path = layout
        .with_series(session.series_name())?
        .patches_dir()
        .join(&session.patch);
    let existing = std::fs::read_to_string(&patch_path).unwrap_or_default();
    let repo = open(layout, session)?;
    let recorded =
        git2::Oid::from_str(&session.base_tree).context("reading the recorded base tree")?;
    let base = repo
        .find_tree(recorded)
        .context(format!("reading the tree {}", session.base_tree))?;
    let header = authoring::header_for(
        Some(&existing),
        &authoring::subject_from_name(&session.patch),
        &authoring::signature(&repo),
    );
    let patch = diff::tree_to_patch(&repo, &base, &header)
        .context(format!("exporting {} for comparison", session.patch))?;
    Ok(diff::change_of(&patch) != diff::change_of(&existing))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testsupport;

    /// A project whose mirror serves `main`, holding `f.txt` with `base\n`.
    fn project(root: &std::path::Path) -> (Layout, String) {
        let (layout, sha) = testsupport::mirror_with_one_commit(root);
        std::fs::write(
            layout.config_file(),
            "[upstream]\nurl = \"unused\"\ndefault-ref = \"main\"\n",
        )
        .unwrap();
        (layout, sha)
    }

    fn entries_of(layout: &Layout) -> Vec<Entry> {
        layout.project().unwrap().1.entries(layout).unwrap()
    }

    /// What a tree in the mirror holds at `path`.
    fn text_in(layout: &Layout, tree: git2::Oid, path: &str) -> String {
        let mirror = Mirror::open(layout).unwrap();
        let tree = mirror.repo().find_tree(tree).unwrap();
        let entry = tree.get_path(std::path::Path::new(path)).unwrap();
        String::from_utf8(
            mirror
                .repo()
                .find_blob(entry.id())
                .unwrap()
                .content()
                .to_vec(),
        )
        .unwrap()
    }

    #[test]
    fn the_manifest_round_trips_and_a_missing_one_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path());
        assert_eq!(Manifest::load(&layout).unwrap(), Manifest::default());

        let mut manifest = Manifest::default();
        manifest.insert(Session {
            series: DEFAULT_SERIES.to_owned(),
            patch: "0001-a.patch".into(),
            dir: "0001-a.patch".into(),
            ref_name: "main".into(),
            base_sha: "abc123".into(),
            base_tree: "def456".into(),
        });
        manifest.save(&layout).unwrap();
        assert_eq!(Manifest::load(&layout).unwrap(), manifest);
        assert_eq!(manifest.patches(&layout), ["0001-a.patch"]);
    }

    #[test]
    fn a_corrupt_manifest_is_an_error_rather_than_an_empty_one() {
        // Defaulting would forget the sessions whose directories still hold edits, and the
        // next reconcile would delete them as unrecorded.
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path());
        std::fs::create_dir_all(layout.work_dir()).unwrap();
        std::fs::write(layout.sessions_file(), "{ not json").unwrap();

        let err = Manifest::load(&layout).unwrap_err().to_string();
        assert!(err.contains("sessions.json"), "got {err}");
    }

    #[test]
    fn an_unrecorded_name_that_would_leave_the_work_directory_is_refused() {
        // The manifest is read back and acted on, including by deletion, so the refusal
        // belongs where the path is formed.
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path());
        assert!(layout.work_patch_dir("../outside").is_err());
    }

    #[test]
    fn plan_reaches_the_tree_the_session_should_hold() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, sha) = project(tmp.path());
        testsupport::write_series(
            &layout,
            &[
                ("0001-a.patch", testsupport::patch_one()),
                ("0002-b.patch", testsupport::patch_two()),
            ],
        );
        let entries = entries_of(&layout);

        // Position zero: nothing below, so the base tree is the ref's own tree and the
        // worktree starts there, with the patch's own content adding nothing yet.
        let for_0001 = plan(&layout, "main", &[], None).unwrap();
        assert_eq!(for_0001.base_sha, sha);
        assert_eq!(
            for_0001.base_tree,
            crate::git::commit_tree(Mirror::open(&layout).unwrap().repo(), &sha)
                .unwrap()
                .id()
        );
        assert_eq!(for_0001.work_tree, for_0001.base_tree);
        assert_eq!(text_in(&layout, for_0001.work_tree, "f.txt"), "base\n");

        // Opening 0002: below holds 0001's output, so the base tree is the tree 0001
        // produced, and the worktree is that with nothing of its own on top yet.
        let for_0002 = plan(&layout, "main", &entries[..1], None).unwrap();
        assert_eq!(text_in(&layout, for_0002.base_tree, "f.txt"), "first\n");
        assert_eq!(for_0002.work_tree, for_0002.base_tree);

        // With the patch's own file replayed on top, the worktree holds its content.
        let for_0002 = plan(&layout, "main", &entries[..1], Some(&entries[1])).unwrap();
        assert_eq!(text_in(&layout, for_0002.base_tree, "f.txt"), "first\n");
        assert_eq!(text_in(&layout, for_0002.work_tree, "f.txt"), "second\n");
    }

    #[test]
    fn an_empty_patch_replays_as_no_change_at_all() {
        // `new` writes an empty patch and `edit` can open on it before any content exists.
        // If an empty file did not replay, the very first session would refuse to open.
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = project(tmp.path());
        testsupport::write_series(&layout, &[("0001-a.patch", "")]);
        let entries = entries_of(&layout);

        let empty_plan = plan(&layout, "main", &[], Some(&entries[0])).unwrap();
        assert_eq!(
            empty_plan.work_tree, empty_plan.base_tree,
            "an empty patch must change nothing"
        );
    }

    #[test]
    fn a_series_that_does_not_apply_is_refused_before_anything_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = project(tmp.path());
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
        let entries = entries_of(&layout);

        let err = plan(&layout, "main", &entries, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("0001-a.patch"), "the failing patch: {err}");
        assert!(err.contains("Nothing was written"), "{err}");
        assert!(
            !layout.work_dir().exists(),
            "plan must not create the work area"
        );
        assert!(!layout.sessions_file().exists());
    }

    #[test]
    fn a_session_worktree_holds_the_patched_tree_and_nothing_else_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, sha) = project(tmp.path());
        testsupport::write_series(
            &layout,
            &[
                ("0001-a.patch", testsupport::patch_one()),
                ("0002-b.patch", testsupport::patch_two()),
            ],
        );
        let entries = entries_of(&layout);

        let for_0002 = plan(&layout, "main", &entries[..1], None).unwrap();
        let session = materialise(&layout, "0002-b.patch", &for_0002).unwrap();

        let dir = layout.work_patch_dir("0002-b.patch").unwrap();
        assert!(dir.join(".git").is_file(), "a linked worktree, not a clone");
        assert_eq!(
            std::fs::read_to_string(dir.join("f.txt")).unwrap(),
            "first\n"
        );

        // HEAD is the base, so the patch's own change reads as the working diff.
        let repo = open(&layout, &session).unwrap();
        assert_eq!(sandbox::head_sha(&repo).unwrap(), sha);
        assert!(
            repo.head_detached().unwrap(),
            "sessions are never on a branch"
        );

        // The session is recorded, and the shared run state is not touched at all: the
        // whole point of the split is that `new` and friends stop writing it.
        let manifest = Manifest::load(&layout).unwrap();
        assert_eq!(manifest.patches(&layout), ["0002-b.patch"]);
        assert_eq!(manifest.get(&layout, "0002-b.patch").unwrap().base_sha, sha);
        assert!(
            !layout.state_file().exists(),
            "sessions never write state.json"
        );
        assert!(
            !layout.sandbox_dir().exists(),
            "sessions never touch the sandbox"
        );

        // And the mirror gained no branch: a session is a checkout, not a publish.
        let refs = Mirror::open(&layout).unwrap().refs().unwrap();
        let branches: Vec<&str> = refs.branches.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(branches, ["main"], "a session publishes nothing");
    }

    #[test]
    fn two_patches_can_be_open_at_once() {
        // The property the shared sandbox could not offer: two sessions side by side,
        // each holding its own position in the series.
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = project(tmp.path());
        testsupport::write_series(
            &layout,
            &[
                ("0001-a.patch", testsupport::patch_one()),
                ("0002-b.patch", testsupport::patch_two()),
            ],
        );
        let entries = entries_of(&layout);

        materialise(
            &layout,
            "0001-a.patch",
            &plan(&layout, "main", &[], None).unwrap(),
        )
        .unwrap();
        materialise(
            &layout,
            "0002-b.patch",
            &plan(&layout, "main", &entries[..1], None).unwrap(),
        )
        .unwrap();

        assert_eq!(
            Manifest::load(&layout).unwrap().patches(&layout),
            ["0001-a.patch", "0002-b.patch"]
        );
        let one = layout.work_patch_dir("0001-a.patch").unwrap();
        let two = layout.work_patch_dir("0002-b.patch").unwrap();
        assert_eq!(
            std::fs::read_to_string(one.join("f.txt")).unwrap(),
            "base\n"
        );
        assert_eq!(
            std::fs::read_to_string(two.join("f.txt")).unwrap(),
            "first\n"
        );
    }

    #[test]
    fn two_series_can_hold_a_session_for_the_same_patch_name() {
        // The registration names carry the series, so `0001-a.patch` in one series and
        // `0001-a.patch` in another are two worktrees of one mirror, not a collision.
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = project(tmp.path());
        testsupport::write_series(&layout, &[("0001-a.patch", testsupport::patch_one())]);
        materialise(
            &layout,
            "0001-a.patch",
            &plan(&layout, "main", &[], None).unwrap(),
        )
        .unwrap();

        let exp = layout.with_series("exp").unwrap();
        std::fs::create_dir_all(exp.patches_dir()).unwrap();
        std::fs::write(exp.series_file(), "0001-a.patch\n").unwrap();
        std::fs::write(
            exp.patches_dir().join("0001-a.patch"),
            testsupport::patch_adds_g(),
        )
        .unwrap();
        let session = materialise(
            &exp,
            "0001-a.patch",
            &plan(&exp, "main", &[], Some(&entries_of(&exp)[0])).unwrap(),
        )
        .unwrap();

        assert_eq!(session.series_name(), "exp");
        assert_eq!(
            Manifest::load(&layout).unwrap().patches(&layout),
            ["0001-a.patch"]
        );
        assert_eq!(
            Manifest::load(&layout).unwrap().patches(&exp),
            ["0001-a.patch"]
        );
        // Different directories, both openable: the names in the mirror did not collide.
        assert_ne!(
            layout.work_patch_dir("0001-a.patch").unwrap(),
            session.dir_path(&exp).unwrap()
        );
        open(
            &layout,
            &Manifest::load(&layout)
                .unwrap()
                .get(&layout, "0001-a.patch")
                .unwrap()
                .clone(),
        )
        .unwrap();
        open(&exp, &session).unwrap();
    }

    #[test]
    fn a_second_session_for_a_patch_already_open_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = project(tmp.path());
        let first = plan(&layout, "main", &[], None).unwrap();
        materialise(&layout, "0001-a.patch", &first).unwrap();

        let err = materialise(&layout, "0001-a.patch", &first)
            .unwrap_err()
            .to_string();
        assert!(err.contains("already has a work area"), "got {err}");
        assert_eq!(
            Manifest::load(&layout).unwrap().patches(&layout),
            ["0001-a.patch"],
            "the first session must be untouched"
        );
    }

    #[test]
    fn a_husk_worktree_nobody_recorded_is_replaced_rather_than_reused() {
        // What an interrupted run leaves: the worktree registered, the manifest entry
        // never written. The next attempt clears it and succeeds instead of refusing with
        // "worktree already exists".
        let tmp = tempfile::tempdir().unwrap();
        let (layout, _sha) = project(tmp.path());
        let dir = layout.work_patch_dir("0001-a.patch").unwrap();
        let mirror = Mirror::open(&layout).unwrap();
        sandbox::add(&mirror, "0001-a.patch", &dir, "remedy").unwrap();
        std::fs::write(dir.join("leftover.txt"), b"half-finished edit").unwrap();

        let plan = plan(&layout, "main", &[], None).unwrap();
        materialise(&layout, "0001-a.patch", &plan).unwrap();

        assert_eq!(
            Manifest::load(&layout).unwrap().patches(&layout),
            ["0001-a.patch"]
        );
        assert!(
            !dir.join("leftover.txt").exists(),
            "the husk's leftovers must be gone, replaced by a fresh checkout"
        );
    }

    #[test]
    fn reconcile_drops_a_session_whose_directory_is_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path());
        let mut manifest = Manifest::default();
        manifest.insert(Session {
            series: DEFAULT_SERIES.to_owned(),
            patch: "0001-a.patch".into(),
            dir: "0001-a.patch".into(),
            ref_name: "main".into(),
            base_sha: "abc".into(),
            base_tree: "def".into(),
        });
        manifest.save(&layout).unwrap();

        let reconciled = reconcile(&layout).unwrap();
        assert!(reconciled.sessions.is_empty());
        assert!(
            Manifest::load(&layout).unwrap().sessions.is_empty(),
            "the dropped entry must be persisted, not just not returned"
        );
    }

    #[test]
    fn reconcile_removes_a_directory_no_session_recorded() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path());
        let husk = layout.work_patch_dir("0001-a.patch").unwrap();
        std::fs::create_dir_all(&husk).unwrap();
        std::fs::write(husk.join("f.txt"), b"left behind").unwrap();

        reconcile(&layout).unwrap();

        assert!(
            !husk.exists(),
            "an unrecorded directory is not anyone's work"
        );
        assert!(
            layout.work_dir().exists(),
            "the work directory itself stays"
        );
    }

    #[test]
    fn reconcile_keeps_a_nested_session_directory_it_might_look_like_an_orphan() {
        // A patch name may carry a subdirectory, so `work/sub` is a *parent* of the
        // session, not an unrecorded directory beside it. Walking by file name alone
        // would delete the session inside.
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path());
        let dir = layout.work_patch_dir("sub/0002-fix.patch").unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), b"edits").unwrap();
        let mut manifest = Manifest::default();
        manifest.insert(Session {
            series: DEFAULT_SERIES.to_owned(),
            patch: "sub/0002-fix.patch".into(),
            dir: "sub/0002-fix.patch".into(),
            ref_name: "main".into(),
            base_sha: "abc".into(),
            base_tree: "def".into(),
        });
        manifest.save(&layout).unwrap();

        let reconciled = reconcile(&layout).unwrap();

        assert_eq!(reconciled.patches(&layout), ["sub/0002-fix.patch"]);
        assert!(
            dir.join("f.txt").exists(),
            "the session's edits must survive"
        );
    }

    #[test]
    fn a_renamed_patch_keeps_its_session_and_the_worktree_it_opened() {
        // `new --at` or `move` in front of an open patch renames the tail. The session
        // record has to follow the *name* (so `list` and `edit-save` find it by what
        // the series now calls it) without following the *directory* (git cannot rename a
        // worktree registration in place, and the edits live in that directory).
        let tmp = tempfile::tempdir().unwrap();
        let (layout, sha) = project(tmp.path());
        testsupport::write_series(&layout, &[("0001-a.patch", "")]);
        let plan = plan(&layout, "main", &[], None).unwrap();
        materialise(&layout, "0001-a.patch", &plan).unwrap();

        // What `new --at 1` does to the rest of the manifest: the name moves, the
        // directory does not.
        let mut manifest = Manifest::load(&layout).unwrap();
        manifest.follow_renames(
            &layout,
            &[("0001-a.patch".to_owned(), "0002-a.patch".to_owned())],
        );
        manifest.save(&layout).unwrap();

        let old_dir = layout.work_patch_dir("0001-a.patch").unwrap();
        assert!(old_dir.exists(), "the worktree does not move");

        let reconciled = reconcile(&layout).unwrap();
        let session = reconciled.get(&layout, "0002-a.patch").unwrap();
        assert_eq!(session.dir_name(), "0001-a.patch");
        assert_eq!(session.base_sha, sha);
        assert!(
            old_dir.exists(),
            "reconcile must not treat the moved name's directory as an orphan"
        );
    }
}
