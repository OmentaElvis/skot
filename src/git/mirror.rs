//! A bare mirror of the upstream repository.
//!
//! This is the only thing in skot that touches the network. Everything else reads
//! from it, which is what makes `check` and `status` work offline and cost seconds
//! rather than minutes.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::{Sha, resolve_ref};
use crate::layout::Layout;
use crate::ui;

/// A bare repository skot can read refs and objects from.
///
/// Not `Debug`: `git2::Repository` is not, and a derived impl would either not compile or
/// print nothing useful about the mirror.
pub struct Mirror {
    repo: git2::Repository,
    path: PathBuf,
}

impl Mirror {
    /// Open an existing mirror.
    pub fn open(layout: &Layout) -> Result<Self> {
        let path = layout.mirror_dir();
        let repo = git2::Repository::open_bare(&path)
            .context(format!("opening the mirror at {}", path.display()))?;

        Ok(Self { repo, path })
    }

    /// Open the mirror, creating it from `url` if it is not there yet.
    pub fn open_or_clone(layout: &Layout, url: &str, ref_name: Option<&str>) -> Result<Self> {
        if layout.mirror_dir().join("HEAD").is_file() {
            return Self::open(layout);
        }
        let repo = Self::fetch(url, &layout.mirror_dir(), ref_name, "fetching upstream")?;
        Ok(Self {
            repo,
            path: layout.mirror_dir(),
        })
    }

    /// Fetch refs and tags from the remote into the existing mirror.
    pub fn sync(&self, ref_name: Option<&str>) -> Result<()> {
        let url = self.remote_url()?;
        Self::fetch(&url, &self.path, ref_name, "fetching upstream").map(|_| ())
    }

    /// The one network call, shared by the initial fetch and every later one.
    ///
    /// There is deliberately no `RepoBuilder::clone` shortcut for a mirror that does not exist
    /// yet. `clone` takes its refspecs from the remote's config, which means the first
    /// `skot sync <ref>` would fetch whatever the remote's default branch is and ignore the
    /// ref that was actually asked for — and would then leave the mirror missing every other
    /// branch. Initialising and fetching by hand costs three lines and makes the first sync
    /// behave exactly like the second, which is the property that makes this understandable.
    ///
    /// The progress is wired to the spinner rather than discarded. A mirror of a real project
    /// is minutes of network, and silence that long is indistinguishable from a hang.
    /// Fetch into `path`, showing `message` on a spinner.
    ///
    /// Whether the spinner is drawn is decided where a spinner is made, from the process's
    /// own situation (`--json`, not a terminal) — not handed in as an instruction from a
    /// caller. See `ui::Spinner`.
    fn fetch(
        url: &str,
        path: &Path,
        ref_name: Option<&str>,
        message: &str,
    ) -> Result<git2::Repository> {
        // Refspecs, not config, so a fetch behaves like `git clone --mirror` regardless of
        // what the remote or the mirror's own config happens to say.
        //
        // The leading `+` forces the update: a rewritten upstream ref is taken as-is rather
        // than rejected as a non-fast-forward. That is the entire point of
        // `skot sync <ref>`, since a branch that force-pushes is precisely the one a mirror
        // would otherwise refuse to advance.
        //
        // Note what `ref_name` does and does not do here. It force-updates the refs named by
        // that name, and it does *not* narrow the fetch to them: the wildcards stay, so the
        // mirror always holds every branch and tag. Naming a ref says "this one gets
        // force-updated", not "only fetch this one", because skot's job is moving a patch
        // series between bases and a mirror missing branches turns every other base into an
        // extra fetch.
        let mut refspecs: Vec<String> = vec![
            "+refs/heads/*:refs/heads/*".into(),
            "+refs/tags/*:refs/tags/*".into(),
        ];
        if let Some(name) = ref_name {
            refspecs.push(format!("+refs/heads/{name}:refs/heads/{name}"));
            refspecs.push(format!("+refs/tags/{name}:refs/tags/{name}"));
        }

        let repo = if path.join("HEAD").is_file() {
            git2::Repository::open_bare(path)
                .context(format!("opening the mirror at {}", path.display()))?
        } else {
            // `init_bare` creates only the last component, so the parent has to exist first.
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .context(format!("creating {}", parent.display()))?;
            }
            git2::Repository::init_bare(path)
                .context(format!("creating the mirror at {}", path.display()))?
        };

        // `origin` is written on the first fetch and reused after that, so the URL survives
        // without skot keeping its own copy of it. A later `sync` reads it back with
        // `remote_url`.
        if repo.find_remote("origin").is_err() {
            repo.remote("origin", url)
                .context(format!("recording {url} as the mirror's `origin`"))?;
        }

        let progress = ui::Progress::new(message);
        let mut callbacks = git2::RemoteCallbacks::new();
        progress.attach_to(&mut callbacks);
        let mut options = git2::FetchOptions::new();
        options.remote_callbacks(callbacks);

        {
            let mut remote = repo
                .find_remote("origin")
                .context(format!("preparing a fetch from {url}"))?;
            remote
                .fetch(&refspecs, Some(&mut options), None)
                .context(format!(
                    "fetching from {url}. If this says 'not found' or 'no such remote', check the URL and that you can reach it."
                ))?;

            // A freshly initialised mirror has HEAD pointing at whatever git2's template chose
            // (`refs/heads/master`, usually), which nothing points at after a wildcard fetch.
            // `init_bare` never guesses the remote's actual default branch, so HEAD dangles
            // and every bare `HEAD` lookup fails — including `skot check` with no ref
            // argument. Point it at the remote's default branch instead.
            // `head()` succeeds on an *unborn* HEAD — it resolves the symbolic ref without checking
            // that anything is there — so the test has to be for the ref actually pointing at a commit.
            let head_dangles = repo
                .head()
                .ok()
                .and_then(|h| h.peel_to_commit().ok())
                .is_none();
            if head_dangles {
                // `default_branch` is only answerable once the remote has connected, which is
                // why this is inside the fetch's scope. It returns the *full* ref name.
                let default = remote
                    .default_branch()
                    .ok()
                    .and_then(|b| b.as_str().ok().map(str::to_owned));
                if let Some(default) = default.as_deref().map(str::trim)
                    && repo.find_reference(default).is_ok()
                {
                    repo.set_head(default)
                        .context(format!("pointing the mirror's HEAD at {default}"))?;
                }
            }
        }

        progress.finish();
        Ok(repo)
    }

    /// The URL the mirror was cloned from, or the `remote.origin.url` from its config.
    pub fn remote_url(&self) -> Result<String> {
        let url = self
            .repo
            .find_remote("origin")
            .ok()
            .and_then(|r| r.url().ok().map(str::to_owned));
        let Some(url) = url else {
            anyhow::bail!(
                "the mirror at {} has no fetch URL for `origin`, so skot cannot tell where to fetch from.\n\
                 Set one by hand, or delete the mirror and re-run `skot sync`.",
                self.path.display()
            );
        };
        Ok(url)
    }

    pub fn is_initialised(&self) -> bool {
        self.path.join("HEAD").is_file()
    }

    pub fn repo(&self) -> &git2::Repository {
        &self.repo
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The mirror's object store.
    ///
    /// A linked worktree shares the mirror's object store implicitly — that is what a
    /// worktree is — so this path is only needed by `clean` and `status`, for reporting how
    /// much disk the mirror occupies.
    pub fn objects_dir(&self) -> PathBuf {
        self.path.join("objects")
    }

    /// The mirror is skot's own cache, and this records the one rule that keeps a
    /// worktree sandbox sound.
    ///
    /// A linked worktree shares the mirror's object store and holds refs of its own, so
    /// the dependency is narrower than it looks: nothing outside the mirror is keeping
    /// those objects alive either. If the mirror is pruned or replaced while a worktree
    /// exists, the worktree still looks healthy and fails later, mid-build, with "object
    /// not found" — a failure whose cause is nowhere near its symptom. Therefore:
    /// **skot never gc's or prunes the mirror.** It is disposable state, rebuilt by
    /// `skot sync`, so there is nothing to gain from reclaiming space and a
    /// correctness hazard to pay for it.
    pub fn is_owned_by_skot(&self) -> bool {
        true
    }

    /// Resolve a user-supplied ref name to a commit id.
    pub fn resolve(&self, name: &str) -> Result<Sha> {
        resolve_ref(&self.repo, name)
    }

    /// Every ref, grouped.
    pub fn refs(&self) -> Result<super::Refs> {
        super::refs(&self.repo)
    }

    /// Tag names, sorted by version, newest last. A tag that is not `X.Y.Z` is
    /// excluded, because guessing how to order `nightly-2024-01-01` against `v1.2.3`
    /// is how you ship the wrong thing.
    pub fn version_tags(&self) -> Result<Vec<String>> {
        let refs = self.refs()?;
        let mut tags: Vec<(Vec<u32>, String)> = refs
            .tags
            .iter()
            .filter_map(|(name, _)| parse_version(name).map(|v| (v, name.clone())))
            .collect();
        tags.sort();
        Ok(tags.into_iter().map(|(_, name)| name).collect())
    }
}

/// Parse `v1.2.3` or `1.2.3` into a sortable triple. `None` for anything else.
fn parse_version(tag: &str) -> Option<Vec<u32>> {
    let body = tag.strip_prefix('v').unwrap_or(tag);
    let mut parts = body.split('.').map(str::parse::<u32>);
    let a = parts.next()?.ok()?;
    let b = parts.next()?.ok()?;
    let c = parts.next()?.ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(vec![a, b, c])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bare "upstream" holding the named branches, one commit each, all with `HEAD`
    /// on `main`.
    ///
    /// A local path as the remote URL is libgit2's own transport, so this exercises the real
    /// fetch path — refspecs, force updates, default-branch detection — without a network or
    /// a `git` binary.
    fn upstream(dir: &Path) -> PathBuf {
        let path = dir.join("upstream.git");
        let repo = git2::Repository::init_bare(&path).unwrap();
        let tree = repo
            .treebuilder(None)
            .unwrap()
            .write()
            .map(|oid| repo.find_tree(oid).unwrap())
            .unwrap();
        let who = git2::Signature::now("skot", "skot@example.com").unwrap();
        for branch in ["main", "release", "feature"] {
            let commit = repo
                .commit(
                    Some(&format!("refs/heads/{branch}")),
                    &who,
                    &who,
                    branch,
                    &tree,
                    &[],
                )
                .unwrap();
            let _ = commit;
        }
        repo.set_head("refs/heads/main").unwrap();
        path
    }

    fn layout_in(dir: &Path) -> Layout {
        Layout::new(dir.to_path_buf())
    }

    #[test]
    fn the_first_sync_fetches_every_branch_not_just_the_named_one() {
        // `sync <ref>` force-updates the ref it is given; it must not quietly turn the
        // mirror into a mirror of one branch. Losing branches here is the failure that is
        // invisible until someone checks a series against a base they did not name.
        let tmp = tempfile::tempdir().unwrap();
        let url = upstream(tmp.path());
        let layout = layout_in(tmp.path());

        let mirror =
            Mirror::open_or_clone(&layout, url.to_str().unwrap(), Some("release")).unwrap();

        let refs = mirror.refs().unwrap();
        let branches: Vec<&str> = refs.branches.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            branches,
            vec!["feature", "main", "release"],
            "the mirror should hold every upstream branch, not just the requested one"
        );
    }

    #[test]
    fn the_mirror_head_follows_the_upstream_default_branch() {
        // An unborn HEAD makes every bare `HEAD` lookup fail, which is what `skot check`
        // with no ref argument does.
        let tmp = tempfile::tempdir().unwrap();
        let url = upstream(tmp.path());
        let layout = layout_in(tmp.path());

        let mirror = Mirror::open_or_clone(&layout, url.to_str().unwrap(), None).unwrap();

        assert_eq!(
            mirror.resolve("HEAD").unwrap(),
            mirror.resolve("main").unwrap(),
            "HEAD should point at the upstream default branch, not dangle"
        );
    }

    #[test]
    fn a_rewritten_ref_is_updated_rather_than_rejected() {
        // The reason `+` is on every refspec. A mirror that refuses a force-pushed branch
        // silently reports the *old* commit as the base, and the patch series then checks
        // against a history that never existed.
        let tmp = tempfile::tempdir().unwrap();
        let url = upstream(tmp.path());
        let layout = layout_in(tmp.path());

        Mirror::open_or_clone(&layout, url.to_str().unwrap(), None).unwrap();
        let before = git2::Repository::open_bare(&url)
            .unwrap()
            .resolve_reference_from_short_name("main")
            .unwrap()
            .target()
            .unwrap();

        // Rewrite `main` upstream, as a rebase-and-force-push PR does: the branch's history
        // is replaced, so the new tip is not a descendant of the old one. That is exactly what
        // a fetch without `+` rejects.
        {
            let up = git2::Repository::open_bare(&url).unwrap();
            let tree = up
                .treebuilder(None)
                .unwrap()
                .write()
                .map(|o| up.find_tree(o).unwrap())
                .unwrap();
            let who = git2::Signature::now("skot", "skot@example.com").unwrap();
            let parent = up
                .revparse_single("refs/heads/release")
                .unwrap()
                .peel_to_commit()
                .unwrap();
            // Commit unattached, then move the ref with `force`. `Repository::commit` against
            // an existing ref insists the new commit's first parent be the current tip, which
            // is precisely the constraint a rewrite violates.
            let rewritten = up
                .commit(None, &who, &who, "rewritten", &tree, &[&parent])
                .unwrap();
            up.reference("refs/heads/main", rewritten, true, "rewritten history")
                .unwrap();
        }

        let mirror = Mirror::open(&layout).unwrap();
        mirror.sync(None).unwrap();
        let after = mirror.resolve("main").unwrap();

        assert_ne!(
            after.to_string(),
            before.to_string(),
            "the mirror kept the pre-rewrite commit, so checks run against a history \
             that upstream threw away"
        );
    }

    #[test]
    fn the_fetch_url_survives_between_runs() {
        // Nothing outside the mirror records where it came from, so `origin` in the
        // mirror's config is the only memory a bare `skot sync` has.
        let tmp = tempfile::tempdir().unwrap();
        let url = upstream(tmp.path());
        let layout = layout_in(tmp.path());

        let mirror = Mirror::open_or_clone(&layout, url.to_str().unwrap(), None).unwrap();

        assert_eq!(mirror.remote_url().unwrap(), url.to_str().unwrap());
        // And a later sync needs no URL argument at all.
        Mirror::open(&layout).unwrap().sync(None).unwrap();
    }

    #[test]
    fn a_bad_url_says_what_to_do_about_it() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = layout_in(tmp.path());

        let err = match Mirror::open_or_clone(&layout, "not-a-real-url", None) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("fetching from a nonexistent URL must fail"),
        };

        assert!(
            err.contains("not-a-real-url"),
            "the error must name the URL that failed: {err}"
        );
    }

    #[test]
    fn version_parsing_is_deliberately_strict() {
        assert_eq!(parse_version("v1.2.3"), Some(vec![1, 2, 3]));
        assert_eq!(parse_version("1.2.3"), Some(vec![1, 2, 3]));
        assert_eq!(parse_version("v10.0.0"), Some(vec![10, 0, 0]));

        for tag in [
            "v1.2",
            "v1.2.3-rc1",
            "main",
            "nightly",
            "v1.2.3.4",
            "vx.y.z",
            "",
        ] {
            assert_eq!(
                parse_version(tag),
                None,
                "{tag} should not be treated as a version"
            );
        }
    }

    #[test]
    fn version_tags_sort_numerically_not_lexically() {
        let mut tags = vec![vec![1, 9, 0], vec![1, 10, 0], vec![1, 2, 3]];
        tags.sort();
        assert_eq!(tags, vec![vec![1, 2, 3], vec![1, 9, 0], vec![1, 10, 0]]);
    }
}
