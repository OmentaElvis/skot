//! A bare mirror of the upstream repository.
//!
//! This is the only thing in skot that touches the network. Everything else reads
//! from it, which is what makes `check` and `status` work offline and cost seconds
//! rather than minutes. Being the only thing, it is also where `--offline` is kept:
//! a fetch from a remote transport is refused here, before anything connects, and a
//! local directory or the mirror already on disk keeps working.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use log::debug;

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
    /// Whether this run promised not to use the network, read from the layout the
    /// mirror was opened with.
    offline: bool,
}

impl Mirror {
    /// Open an existing mirror.
    pub fn open(layout: &Layout) -> Result<Self> {
        let path = layout.mirror_dir();
        let repo = git2::Repository::open_bare(&path)
            .context(format!("opening the mirror at {}", path.display()))?;

        Ok(Self {
            repo,
            path,
            offline: layout.offline(),
        })
    }

    /// Open the mirror, creating it from `url` if it is not there yet.
    pub fn open_or_clone(layout: &Layout, url: &str, ref_name: Option<&str>) -> Result<Self> {
        if layout.mirror_dir().join("HEAD").is_file() {
            return Self::open(layout);
        }
        let offline = layout.offline();
        let repo = Self::fetch(
            url,
            &layout.mirror_dir(),
            ref_name,
            "fetching upstream",
            offline,
        )?;
        Ok(Self {
            repo,
            path: layout.mirror_dir(),
            offline,
        })
    }

    /// Fetch the named ref from the remote into the existing mirror, or the whole
    /// upstream, when no ref was named.
    ///
    /// The mirror's own `origin` is where a re-fetch comes from. A mirror built without
    /// one falls back to `fallback` (the configured URL), and the fetch records that URL
    /// as `origin` on the way through, so the mirror heals as it works.
    pub fn sync(&self, fallback: Option<&str>, ref_name: Option<&str>) -> Result<()> {
        let url = match self.remote_url() {
            Ok(url) => url,
            Err(err) => fallback.map(str::to_owned).ok_or(err)?,
        };
        Self::fetch(
            &url,
            &self.path,
            ref_name,
            "fetching upstream",
            self.offline,
        )
        .map(|_| ())
    }

    /// Fetch `refspecs` from `url` through the mirror's remote called `remote_name`,
    /// showing `message` while it runs.
    ///
    /// This is `import`'s network access: a fork or a pull request is fetched *into* the
    /// mirror under whatever destinations `refspecs` name, so everything after the fetch
    /// reads the same object store every other command already reads. A remote other than
    /// `origin` is retargeted to `url` on every call, because one project can be imported
    /// from several repositories; `origin` keeps the URL it was cloned with.
    ///
    /// `depth` bounds how much history comes back: `1` is the tip commit and its tree
    /// (what a patch series diffs against), [`i32::MAX`] is everything, what `import`
    /// needs for a merge base. A local URL ignores it; libgit2 refuses shallow fetches
    /// over its own local transport.
    pub fn fetch_refspecs(
        &self,
        url: &str,
        remote_name: &str,
        refspecs: &[String],
        message: &str,
        depth: i32,
    ) -> Result<()> {
        Self::transfer(
            url,
            &self.path,
            remote_name,
            refspecs,
            message,
            None,
            depth,
            self.offline,
        )
        .map(|_| ())
    }

    /// Whether the mirror holds truncated history: a shallow fetch over a network
    /// transport, which is how a mirror of a huge upstream stays small.
    pub fn is_shallow(&self) -> bool {
        self.repo.is_shallow()
    }

    /// Resolve `name` to a commit id if it resolves at all, for callers that are probing
    /// several candidate names and want a miss rather than an error naming a remedy.
    pub fn try_resolve(&self, name: &str) -> Option<Sha> {
        super::try_commit(&self.repo, name)
    }

    /// `sync`'s refspecs: exactly the ref asked for, or the whole upstream when nothing
    /// was.
    fn fetch(
        url: &str,
        path: &Path,
        ref_name: Option<&str>,
        message: &str,
        offline: bool,
    ) -> Result<git2::Repository> {
        // Take the ref you are working on and nothing else. The mirror is a cache, not a
        // clone: a ref that a later command needs is fetched when it is asked for
        // (`resolve_in_mirror` points at `skot sync <ref>` for exactly that case), so
        // pulling every branch and tag "to be safe" is how checking out one 10Mb tree
        // costs hundreds of megabytes of history the series never reads.
        //
        // A short name can be either a branch or a tag, so both refspecs are offered and
        // libgit2 takes whichever the remote has; a refspec that matches nothing is not
        // an error (unlike the git CLI, which refuses the whole fetch). The leading `+`
        // forces the update: a rewritten upstream ref is taken as-is rather than
        // rejected as a non-fast-forward, which is the entire point of syncing a branch
        // that force-pushes.
        let refspecs: Vec<String> = match ref_name {
            None => vec![
                "+refs/heads/*:refs/heads/*".into(),
                "+refs/tags/*:refs/tags/*".into(),
            ],
            // A commit id names no ref upstream; the caller's post-fetch check reports
            // it with a message that says so rather than a fetch that returns nothing.
            Some(name) if is_commit_id(name) => Vec::new(),
            Some(name) if name.starts_with("refs/") => vec![format!("+{name}:{name}")],
            Some(name) => vec![
                format!("+refs/heads/{name}:refs/heads/{name}"),
                format!("+refs/tags/{name}:refs/tags/{name}"),
            ],
        };
        // What this fetch actually asks for, before any of it becomes network: the
        // decision to take one ref, a tag, a whole upstream, or nothing at all.
        if refspecs.is_empty() {
            debug!(target: "git", "no named ref to ask for (a commit id); not connecting");
        } else {
            debug!(target: "git", "asking for {}", refspecs.join(" "));
        }

        Self::transfer(
            url,
            path,
            "origin",
            &refspecs,
            message,
            ref_name,
            FETCH_DEPTH,
            offline,
        )
    }

    /// The one network call, shared by the initial fetch, every later `sync`, and `import`.
    ///
    /// Every byte skot transfers to or from a remote passes through here, which is what
    /// makes this the place for two things that must not be per command: `--offline` is
    /// checked at the door below, before anything connects or is written, and a
    /// staleness probe (does the upstream have anything this mirror lacks?) would sit
    /// beside the fetch when it is wanted, one call instead of one per command.
    ///
    /// There is deliberately no `RepoBuilder::clone` shortcut for a mirror that does not exist
    /// yet. `clone` takes its refspecs from the remote's config, which means the first
    /// `skot sync <ref>` would fetch whatever the remote's default branch is and ignore the
    /// ref that was actually asked for — and would then leave the mirror missing every other
    /// branch. Initialising and fetching by hand costs three lines and makes the first sync
    /// behave exactly like the second, which is the property that makes this understandable.
    ///
    /// The progress is wired to the spinner rather than discarded. A mirror of a real project
    /// is minutes of network, and silence that long is indistinguishable from a hang. Whether
    /// the spinner is drawn is decided where a spinner is made, from the process's own
    /// situation (`--json`, not a terminal) — not handed in as an instruction from a caller.
    /// See `ui::Spinner`.
    // One network function on purpose, so the arguments are its whole shape: a struct
    // would name the same eight values one indirection further away.
    #[allow(clippy::too_many_arguments)]
    fn transfer(
        url: &str,
        path: &Path,
        remote_name: &str,
        refspecs: &[String],
        message: &str,
        wanted: Option<&str>,
        depth: i32,
        offline: bool,
    ) -> Result<git2::Repository> {
        // Before the repo, before the spinner: `--offline` promised no connection, so
        // the refusal costs nothing and leaves nothing half-written behind it.
        if offline && !is_local_url(url) {
            bail!(
                "`--offline`, so {url} is not being contacted.\n\
                 Drop `--offline` for this run, or serve the source from a local directory."
            );
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
        // `remote_url`. Any other remote belongs to a one-off source, so it is retargeted:
        // importing from repository A and then repository B must fetch from B, not from A.
        match repo.find_remote(remote_name) {
            Ok(_) if remote_name == "origin" => {}
            Ok(_) => {
                repo.remote_set_url(remote_name, url)
                    .context(format!("pointing `{remote_name}` at {url}"))?;
            }
            Err(_) => {
                repo.remote(remote_name, url)
                    .context(format!("recording {url} as the mirror's `{remote_name}`"))?;
            }
        }

        // A depth limit is what keeps one checked-out tree cheap: beyond the tip commit
        // and its tree there is nothing a patch series reads: patches diff trees, they
        // do not walk history. libgit2 refuses shallow fetches over its local transport
        // (path and `file://` URLs), so those take full history, which is what a local
        // source should cost anyway.
        let depth = if is_local_url(url) {
            if depth > 0 {
                debug!(
                    target: "git",
                    "{url} is a local transport, which libgit2 will not shallow-fetch; \
                     taking full history"
                );
            }
            0
        } else {
            depth
        };
        debug!(
            target: "git",
            "fetching into {} at depth {}",
            path.display(),
            if depth == 0 { "unlimited".to_owned() } else { depth.to_string() }
        );

        let progress = ui::Progress::new(message);
        let mut callbacks = git2::RemoteCallbacks::new();
        progress.attach_to(&mut callbacks);
        let mut options = git2::FetchOptions::new();
        options.remote_callbacks(callbacks);
        if depth > 0 {
            options.depth(depth);
        }

        {
            // An empty refspec list means there was nothing to ask for by name (a
            // commit id, say). Nothing is fetched and no connection is needed; the
            // caller's post-fetch check turns this into the right message.
            if !refspecs.is_empty() {
                let mut remote = repo
                    .find_remote(remote_name)
                    .context(format!("preparing a fetch from {url}"))?;
                remote
                    .fetch(refspecs, Some(&mut options), None)
                    .map_err(|err| Self::fetch_failure(url, err))?;

                // A freshly initialised mirror has HEAD pointing at whatever git2's template chose
                // (`refs/heads/master`, usually), which nothing points at after a fetch of a
                // different ref. `init_bare` never guesses the remote's actual default branch, so
                // HEAD dangles and every bare `HEAD` lookup fails, including `skot check` with no
                // ref argument. Point it at the ref that was just fetched: the fetch brought it,
                // so no second round-trip asks the remote which branch it likes, falling back to
                // the remote's default branch when the whole upstream came down instead.
                let head_dangles = repo
                    .head()
                    .ok()
                    .and_then(|h| h.peel_to_commit().ok())
                    .is_none();
                if head_dangles && remote_name == "origin" {
                    let pointed = wanted
                        .map(|name| point_head_at(&repo, name))
                        .transpose()?
                        .unwrap_or(false);
                    if pointed {
                        debug!(
                            target: "git",
                            "HEAD dangled after init; pointed it at {}",
                            wanted.unwrap_or_default()
                        );
                    } else {
                        // `default_branch` is only answerable once the remote has connected, which
                        // is why this is inside the fetch's scope. It returns the *full* ref name.
                        let default = remote
                            .default_branch()
                            .ok()
                            .and_then(|b| b.as_str().ok().map(str::to_owned));
                        if let Some(default) = default.as_deref().map(str::trim)
                            && repo.find_reference(default).is_ok()
                        {
                            repo.set_head(default)
                                .context(format!("pointing the mirror's HEAD at {default}"))?;
                            debug!(target: "git", "HEAD dangled after init; pointed it at {default}");
                        }
                    }
                }
            }
        }

        progress.finish();
        Ok(repo)
    }

    /// What to say when the one network call fails.
    ///
    /// The sentence above the error, decided from libgit2's own error class rather than
    /// guessed at from its wording; libgit2's message stays in the chain underneath, where
    /// it is the detail rather than the thing the user has to interpret. Each arm is a
    /// failure mode verified against a real fetch (see the tests), and anything
    /// unrecognised falls back to naming the URL, which is always true.
    fn fetch_failure(url: &str, err: git2::Error) -> anyhow::Error {
        let diagnosis = match err.code() {
            git2::ErrorCode::Auth => format!("authentication to {url} failed"),
            _ => match err.class() {
                git2::ErrorClass::Http => format!("the server at {url} answered with an error"),
                git2::ErrorClass::Repository => format!("{url} is not a git repository"),
                git2::ErrorClass::Os if err.message().starts_with("failed to connect") => {
                    format!("cannot reach {url}")
                }
                git2::ErrorClass::Os if err.message().starts_with("failed to resolve path") => {
                    format!("no repository at {url}: that path does not exist")
                }
                // libgit2 reports a scheme it does not speak and a bare path that is not
                // there identically, so the path is checked to tell them apart.
                git2::ErrorClass::Net if err.message().contains("unsupported URL protocol") => {
                    match url.split_once("://") {
                        Some((scheme, _)) => {
                            format!("{url} uses {scheme}://, which skot cannot fetch from")
                        }
                        None if !Path::new(url).exists() => {
                            format!("no repository at {url}: that path does not exist")
                        }
                        _ => format!("cannot fetch from {url}"),
                    }
                }
                _ => format!("fetching from {url}"),
            },
        };
        anyhow::Error::new(err).context(diagnosis)
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
                 Set one by hand, or delete the mirror and re-run `{}`.",
                self.path.display(),
                crate::invocation::skot("sync")
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

/// How much history a `sync`-driven fetch takes over a network transport: the tip commit
/// and its tree, which is all a patch series ever diffs against. `import` asks for more
/// when it needs a merge base. Local transports ignore it (see [`is_local_url`]).
const FETCH_DEPTH: i32 = 1;

/// Whether `url` names a local repository: a path, `file://`, or anything without a host
/// in it. Only fetch depth cares: libgit2 refuses shallow fetches over its local
/// transport, so those fetches take full history.
fn is_local_url(url: &str) -> bool {
    match url.split_once("://") {
        Some((scheme, _)) => scheme == "file",
        // The scp form, `host:path` or `user@host:path`, is a network transport; a path
        // containing `/` before the colon is not.
        None => match url.split_once(':') {
            Some((head, _)) => head.contains('/'),
            None => true,
        },
    }
}

/// A 40- or 64-character hex string: a commit id, which names no ref upstream.
pub(crate) fn is_commit_id(name: &str) -> bool {
    matches!(name.len(), 40 | 64) && name.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Point the mirror's HEAD at `name` as a branch, or detached at it as a tag. False
/// when the mirror has neither, for the caller to fall back on.
fn point_head_at(repo: &git2::Repository, name: &str) -> Result<bool> {
    let candidates: Vec<String> = if name.starts_with("refs/") {
        vec![name.to_owned()]
    } else {
        vec![format!("refs/heads/{name}"), format!("refs/tags/{name}")]
    };
    for candidate in &candidates {
        let Ok(reference) = repo.find_reference(candidate) else {
            continue;
        };
        if candidate.starts_with("refs/heads/") {
            repo.set_head(candidate)
                .context(format!("pointing the mirror's HEAD at {candidate}"))?;
        } else if let Ok(commit) = reference.peel(git2::ObjectType::Commit) {
            repo.set_head_detached(commit.id())
                .context(format!("pointing the mirror's HEAD at {candidate}"))?;
        }
        return Ok(true);
    }
    Ok(false)
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

    /// Opening a mirror that cannot be fetched, as the error it fails with.
    fn open_err(layout: &Layout, url: &str) -> anyhow::Error {
        match Mirror::open_or_clone(layout, url, None) {
            Err(err) => err,
            Ok(_) => panic!("fetching from {url} must fail"),
        }
    }

    #[test]
    fn the_first_sync_fetches_only_the_ref_it_was_asked_for() {
        // The mirror is a cache, fetched from on demand: a base a later command needs is
        // fetched when it is asked for (`resolve_in_mirror` points at `skot sync <ref>`),
        // and pulling every branch "to be safe" is how checking out one 10Mb tree costs
        // hundreds of megabytes of history the series never reads. HEAD follows what was
        // fetched, so the bare `HEAD` lookups `skot check` does keep working.
        let tmp = tempfile::tempdir().unwrap();
        let url = upstream(tmp.path());
        let layout = layout_in(tmp.path());

        let mirror =
            Mirror::open_or_clone(&layout, url.to_str().unwrap(), Some("release")).unwrap();

        let refs = mirror.refs().unwrap();
        let branches: Vec<&str> = refs.branches.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            branches,
            vec!["release"],
            "only the ref that was asked for comes down"
        );
        assert!(
            mirror.resolve("main").is_err(),
            "the other branches are fetched on demand, not eagerly"
        );
        assert_eq!(
            mirror.resolve("HEAD").unwrap(),
            mirror.resolve("release").unwrap(),
            "HEAD follows the ref that was fetched"
        );
    }

    #[test]
    fn local_urls_are_the_ones_libgit2_will_not_shallow_fetch() {
        // The depth limit is skipped for exactly these, because libgit2 refuses a shallow
        // fetch over its own local transport.
        assert!(is_local_url("/srv/upstream"));
        assert!(is_local_url("./upstream.git"));
        assert!(is_local_url("file:///srv/upstream"));
        assert!(!is_local_url("https://example.com/up.git"));
        assert!(!is_local_url("git://example.com/up.git"));
        assert!(!is_local_url("git@example.com:up.git"));
        assert!(!is_local_url("ssh://user@example.com/up.git"));
    }

    #[test]
    fn a_commit_id_is_recognised_as_not_a_ref_name() {
        assert!(is_commit_id("87eb48dca76453d80378bf69e829b31873ff5b28"));
        assert!(is_commit_id(&"a".repeat(64)));
        assert!(!is_commit_id("main"));
        assert!(!is_commit_id("deadbeef"));
        assert!(!is_commit_id("v1.2.3"));
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
        mirror.sync(None, None).unwrap();
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
        Mirror::open(&layout).unwrap().sync(None, None).unwrap();
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
    fn a_path_that_is_not_there_says_the_path_is_missing() {
        // libgit2 reports a bare path that does not exist as "unsupported URL
        // protocol", which reads like a URL-syntax problem. The diagnosis checks
        // the path and says what is actually the case.
        let tmp = tempfile::tempdir().unwrap();
        let layout = layout_in(tmp.path());

        let text = open_err(&layout, "/nonexistent/skot-probe").to_string();

        assert!(text.contains("/nonexistent/skot-probe"), "{text}");
        assert!(text.contains("does not exist"), "{text}");
    }

    #[test]
    fn a_directory_that_is_not_a_repository_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = layout_in(tmp.path());
        let plain = tmp.path().join("plain");
        std::fs::create_dir(&plain).unwrap();

        let text = open_err(&layout, plain.to_str().unwrap()).to_string();

        assert!(text.contains("is not a git repository"), "{text}");
    }

    #[test]
    fn a_refused_connection_is_reported_as_unreachable() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = layout_in(tmp.path());

        // Port 9 on localhost: nothing listens there, so the connect fails at once
        // and without leaving the machine.
        let text = open_err(&layout, "http://127.0.0.1:9/skot-probe.git").to_string();

        assert!(text.contains("cannot reach"), "{text}");
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
