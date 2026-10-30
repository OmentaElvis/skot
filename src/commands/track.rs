//! `skot track`: record an unversioned directory as the series base.
//!
//! This is the bridge for an upstream that ships as a ZIP or tarball. The directory is
//! read once into the mirror as a deterministic synthetic commit, a ref is pointed at
//! it, and `patches/<series>/base` names both. Nothing about the source folder is
//! versioned in place, and the project never needs a fake `.git` directory.
//!
//! The synthetic commit has fixed metadata (`skot <skot@localhost>`, Unix epoch), so the
//! same files always produce the same commit id. A fresh checkout with no `.skot/` can
//! therefore re-run `track` and land back on the commit its patches were written
//! against.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use log::info;
use serde_json::json;

use crate::cli::TrackArgs;
use crate::commands::{Ctx, Exit, emit_json};
use crate::git;
use crate::git::mirror::Mirror;
use crate::git::track::Snapshot;
use crate::hints::Hints;
use crate::invocation::skot;
use crate::layout::Layout;

/// What tracking did, for whichever layer presents it.
#[derive(Debug)]
struct Tracked {
    dir: PathBuf,
    ref_name: String,
    full_ref: String,
    sha: String,
    files: usize,
    created_project: bool,
    created_mirror: bool,
    previous: Option<String>,
}

pub fn run(ctx: &Ctx, args: &TrackArgs) -> Result<Exit> {
    let done = track(&ctx.layout, args)?;
    let short = &done.sha[..done.sha.len().min(12)];

    if ctx.cli.json {
        emit_json(&json!({
            "ok": true,
            "series_name": ctx.layout.series(),
            "ref": done.ref_name,
            "full_ref": done.full_ref,
            "sha": done.sha,
            "directory": done.dir.display().to_string(),
            "files": done.files,
            "base": ctx.layout.base_file().display().to_string(),
            "created_project": done.created_project,
            "created_mirror": done.created_mirror,
            "previous": done.previous,
        }))?;
        return Ok(Exit::Success);
    }

    if done.created_project {
        info!(
            target: "track",
            "no project here; created {} and {}",
            ctx.layout.config_file().display(),
            ctx.layout.patches_dir().display()
        );
    }
    if let Some(previous) = &done.previous {
        info!(target: "track", "replaced the old target {previous} of {}", done.full_ref);
    }
    if done.created_mirror {
        info!(target: "track", "the mirror was missing; created it for this snapshot.");
    }
    info!(
        target: "track",
        "tracked {} from {} as `{}` ({short}) for the series `{}`",
        super::push::count(done.files, "file", "files"),
        done.dir.display(),
        done.ref_name,
        ctx.layout.series()
    );
    Hints::new("next")
        .entry(skot("check"), "verify the series applies to this snapshot")
        .entry(skot("build"), "the whole run: apply, then your hooks")
        .print();
    Ok(Exit::Success)
}

/// The work behind [`run`]: validate, read the directory, then point the ref and base.
fn track(layout: &Layout, args: &TrackArgs) -> Result<Tracked> {
    let dir = &args.source;
    if !dir.is_dir() {
        bail!(
            "`{}` is not a directory.\n\
             Point `track` at the folder holding the unpacked source.",
            dir.display()
        );
    }
    let full_ref = full_ref(&args.ref_name)?;

    // A different recorded base means the series was written against something else.
    // Moving it is allowed, but only when the user says so.
    let recorded_ref = layout.base_ref();
    let recorded_sha = layout.base_sha();
    if !args.force
        && let Some(recorded) = &recorded_ref
        && recorded != &args.ref_name
    {
        bail!(
            "the series `{}` is already based on `{recorded}`, not `{}`.\n\
             Pass `--force` to move it to this snapshot.",
            layout.series(),
            args.ref_name
        );
    }

    // No project yet: `track` is the first command. It lays the project down after the
    // snapshot is read, so tracking the current directory cannot capture the config and
    // patches it is about to create. A wizard here would ask for a URL that the tracked
    // snapshot makes unnecessary, and then offer to mirror something that does not exist.
    let created_project = !layout.config_file().is_file();

    let lock = layout.lock()?;
    let created_mirror = !layout.mirror_dir().join("HEAD").is_file();
    let mirror = Mirror::open_or_init(layout)?;
    let Snapshot { commit, files } = git::track::snapshot(mirror.repo(), dir, &args.ref_name)?;
    let sha = commit.to_string();

    // The same ref may already name a different commit: an earlier snapshot, an upstream
    // tag that happens to share the name, or a hand-made ref. Only `--force` replaces it.
    let previous = mirror
        .repo()
        .find_reference(&full_ref)
        .ok()
        .and_then(|reference| reference.target())
        .map(|oid| oid.to_string());

    if !args.force {
        if let Some(old) = &recorded_sha
            && old != &sha
        {
            bail!(
                "`{}` records `{}` at {old}, but this directory now produces {sha}.\n\
                 Pass `--force` to move the series base to the new snapshot.",
                layout.base_file().display(),
                args.ref_name
            );
        }
        if let Some(old) = &previous
            && old != &sha
        {
            bail!(
                "`{full_ref}` already points to {old}, and this directory produces {sha}.\n\
                 Pass `--force` to replace the ref."
            );
        }
    }

    if created_project {
        super::init::scaffold_for_track(layout)
            .context("creating a project for the tracked snapshot")?;
    }

    mirror
        .repo()
        .reference(&full_ref, commit, true, &format!("track {}", args.ref_name))
        .context(format!("pointing {full_ref} at the snapshot"))?;
    point_head(&mirror, &full_ref, commit, created_mirror)?;
    layout
        .set_base(&args.ref_name, Some(&sha))
        .context("recording the series base")?;
    drop(lock);

    Ok(Tracked {
        dir: dir.clone(),
        ref_name: args.ref_name.clone(),
        full_ref,
        sha,
        files,
        created_project,
        created_mirror,
        previous,
    })
}

/// Give a mirror that has no usable HEAD one the sandbox can check out.
///
/// `git2::Repository::init_bare` leaves HEAD on an unborn branch, and `worktree_add` then
/// fails before `apply` can reset to the tracked ref. An existing mirror with a working
/// HEAD is left alone: its own upstream still decides what a bare fallback means.
fn point_head(
    mirror: &Mirror,
    full_ref: &str,
    commit: git2::Oid,
    created_mirror: bool,
) -> Result<()> {
    if !created_mirror && mirror.repo().head().is_ok() {
        return Ok(());
    }
    if full_ref.starts_with("refs/heads/") {
        mirror
            .repo()
            .set_head(full_ref)
            .context("pointing the mirror HEAD at the tracked branch")
    } else {
        mirror
            .repo()
            .set_head_detached(commit)
            .context("pointing the mirror HEAD at the tracked snapshot")
    }
}

/// The full ref a short name points at.
///
/// A plain name becomes a tag, because a snapshot is not a moving branch. A name already
/// written as `refs/heads/...` or `refs/tags/...` is kept, so a caller that wants a branch
/// can ask for one. Other ref namespaces are refused: they do not name the local refs a
/// series base can stand on.
fn full_ref(name: &str) -> Result<String> {
    let full = if name.starts_with("refs/") {
        name.to_owned()
    } else {
        format!("refs/tags/{name}")
    };
    if !(full.starts_with("refs/tags/") || full.starts_with("refs/heads/")) {
        bail!(
            "`{name}` is not a tag or branch ref.\n\
             Give a name such as `v1.0.0`, or write the ref in full as \
             `refs/heads/vendor`."
        );
    }
    if !git2::Reference::is_valid_name(&full) {
        bail!(
            "`{name}` is not a valid ref name.\n\
             A name cannot be empty, contain whitespace or `..`, or end in a slash."
        );
    }
    Ok(full)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testsupport;
    use clap::Parser as _;

    fn args(dir: &std::path::Path, ref_name: &str) -> TrackArgs {
        TrackArgs {
            source: dir.to_path_buf(),
            ref_name: ref_name.to_owned(),
            force: false,
        }
    }

    fn source(root: &std::path::Path) -> PathBuf {
        let dir = root.join("upstream-src");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("f.txt"), "base\n").unwrap();
        std::fs::write(dir.join("sub/g.txt"), "other\n").unwrap();
        dir
    }

    fn ctx_for(layout: &Layout) -> Ctx {
        Ctx {
            layout: layout.clone(),
            cli: crate::cli::Cli::try_parse_from(["skot", "track", "unused"]).unwrap(),
            yes: false,
        }
    }

    #[test]
    fn a_directory_is_tracked_as_a_tag_and_the_series_base() {
        let (tmp, layout) = testsupport::project();
        let dir = source(tmp.path());

        let done = track(&layout, &args(&dir, "upstream")).unwrap();

        assert_eq!(done.files, 2);
        assert_eq!(done.full_ref, "refs/tags/upstream");
        assert_eq!(layout.base_ref().as_deref(), Some("upstream"));
        assert_eq!(layout.base_sha().as_deref(), Some(done.sha.as_str()));
        let mirror = Mirror::open(&layout).unwrap();
        assert_eq!(mirror.resolve("upstream").unwrap(), done.sha);
    }

    #[test]
    fn tracking_again_is_the_same_commit() {
        let (tmp, layout) = testsupport::project();
        let dir = source(tmp.path());

        let first = track(&layout, &args(&dir, "upstream")).unwrap();
        let second = track(&layout, &args(&dir, "upstream")).unwrap();

        assert_eq!(first.sha, second.sha);
        assert_eq!(layout.base_sha().as_deref(), Some(first.sha.as_str()));
    }

    #[test]
    fn a_series_anchored_elsewhere_needs_force() {
        let (tmp, layout) = testsupport::project();
        let dir = source(tmp.path());
        layout.set_base("main", Some("aaaa")).unwrap();

        let err = track(&layout, &args(&dir, "upstream"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("already based on `main`"), "got: {err}");
        assert!(err.contains("--force"), "got: {err}");
        assert!(
            Mirror::open(&layout).is_err(),
            "a refusal leaves no half-made mirror"
        );

        let forced = track(
            &layout,
            &TrackArgs {
                force: true,
                ..args(&dir, "upstream")
            },
        )
        .unwrap();
        assert_eq!(layout.base_ref().as_deref(), Some("upstream"));
        assert_eq!(layout.base_sha().as_deref(), Some(forced.sha.as_str()));
    }

    #[test]
    fn changing_the_directory_needs_force_to_move_the_base() {
        let (tmp, layout) = testsupport::project();
        let dir = source(tmp.path());
        let first = track(&layout, &args(&dir, "upstream")).unwrap();

        std::fs::write(dir.join("f.txt"), "changed\n").unwrap();
        let err = track(&layout, &args(&dir, "upstream"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("now produces"), "got: {err}");
        assert!(err.contains("--force"), "got: {err}");
        assert_eq!(layout.base_sha().as_deref(), Some(first.sha.as_str()));

        let forced = track(
            &layout,
            &TrackArgs {
                force: true,
                ..args(&dir, "upstream")
            },
        )
        .unwrap();
        assert_ne!(forced.sha, first.sha);
        assert_eq!(layout.base_sha().as_deref(), Some(forced.sha.as_str()));
    }

    #[test]
    fn a_missing_project_is_scaffolded_around_the_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fresh");
        let layout = Layout::new(&root).with_per_series_sandbox(true);
        let dir = source(tmp.path());

        let done = track(&layout, &args(&dir, "upstream")).unwrap();

        assert!(done.created_project);
        assert!(layout.config_file().is_file());
        assert!(layout.series_file().is_file());
        assert!(layout.base_file().is_file());
        assert_eq!(layout.base_ref().as_deref(), Some("upstream"));
        assert_eq!(layout.base_sha().as_deref(), Some(done.sha.as_str()));
        let config = std::fs::read_to_string(layout.config_file()).unwrap();
        assert!(
            !config.lines().any(|line| line.starts_with("url = ")),
            "a project created by `track` records no upstream URL:\n{config}"
        );
    }

    #[test]
    fn tracking_the_current_directory_does_not_capture_the_scaffold() {
        // `skot track .` is a natural thing to type inside an unpacked archive. The
        // project files created for it must not become part of the upstream snapshot.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fresh");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), "source\n").unwrap();
        let layout = Layout::new(&root).with_per_series_sandbox(true);

        let done = track(&layout, &args(&root, "upstream")).unwrap();

        let repo = Mirror::open(&layout).unwrap();
        let tree = repo
            .repo()
            .find_commit(git2::Oid::from_str(&done.sha).unwrap())
            .unwrap()
            .tree()
            .unwrap();
        assert!(tree.get_path(std::path::Path::new("a.txt")).is_ok());
        for project_file in ["skot.toml", "patches", ".gitignore", ".skot"] {
            assert!(
                tree.get_path(std::path::Path::new(project_file)).is_err(),
                "{project_file} must not be in the snapshot"
            );
        }
    }

    #[test]
    fn a_missing_mirror_is_rebuilt_from_the_directory() {
        let (tmp, layout) = testsupport::project();
        let dir = source(tmp.path());
        let first = track(&layout, &args(&dir, "upstream")).unwrap();

        std::fs::remove_dir_all(layout.state_dir()).unwrap();
        let second = track(&layout, &args(&dir, "upstream")).unwrap();

        assert_eq!(second.sha, first.sha);
        assert!(second.created_mirror);
        assert_eq!(layout.base_sha().as_deref(), Some(first.sha.as_str()));
    }

    #[test]
    fn a_full_branch_ref_is_kept_as_one() {
        let (tmp, layout) = testsupport::project();
        let dir = source(tmp.path());

        let done = track(&layout, &args(&dir, "refs/heads/vendor")).unwrap();

        assert_eq!(done.full_ref, "refs/heads/vendor");
        let mirror = Mirror::open(&layout).unwrap();
        assert_eq!(
            mirror
                .repo()
                .find_reference("refs/heads/vendor")
                .unwrap()
                .target(),
            Some(git2::Oid::from_str(&done.sha).unwrap())
        );
    }

    #[test]
    fn a_non_directory_is_refused() {
        let (tmp, layout) = testsupport::project();
        let file = tmp.path().join("archive.zip");
        std::fs::write(&file, "not a folder").unwrap();

        let err = track(&layout, &args(&file, "upstream"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a directory"), "got: {err}");
    }

    #[test]
    fn a_ref_outside_tags_and_heads_is_refused() {
        let (tmp, layout) = testsupport::project();
        let dir = source(tmp.path());

        let err = track(&layout, &args(&dir, "refs/remotes/upstream"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("tag or branch"), "got: {err}");
    }

    #[test]
    fn json_reports_what_was_tracked() {
        let (tmp, layout) = testsupport::project();
        let dir = source(tmp.path());
        let mut ctx = ctx_for(&layout);
        ctx.cli.json = true;

        let out = run(
            &ctx,
            &TrackArgs {
                source: dir,
                ref_name: "v1.0.0".to_owned(),
                force: false,
            },
        )
        .unwrap();

        assert_eq!(out, Exit::Success);
    }
}
