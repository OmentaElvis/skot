//! `sync`: bring the mirror up with what is missing, and present it.
//!
//! This is where upstream itself is fetched. `import` fetches too — a fork or a pull
//! request — but only ever into this same mirror, through `git::mirror`, because that is
//! where every command already reads from. Everything else just reads the mirror, which is
//! what makes `check` and `apply` work offline.
//!
//! Four functions, with a deliberate boundary between them:
//!
//! * [`ensure`] is the local-first one: if the mirror can already serve the ref, nothing
//!   is fetched: no connection, no negotiation, no failure because the network happens
//!   to be down. A run that asked for `--refresh` refreshes anyway: the flag is read in
//!   [`sync`], once, so every caller of `ensure` gets it. This is what `build`'s sync
//!   phase runs, and what bare `skot sync` runs. A ref the mirror lacks is fetched on
//!   the spot, narrowed to that ref.
//! * [`fetch`] always contacts the upstream: the first fetch of a project, and any
//!   caller that means "update now" regardless of what the run asked for. It reports
//!   what it found, and refuses a ref the upstream does not have rather than reporting
//!   a silent success.
//! * [`resolve_in_mirror`] is what every ref-reading command goes through: it calls
//!   [`ensure`] and then resolves, so a command reads what it needs instead of stopping
//!   to tell the user to run this one first, the same way a build tool synchronizes a
//!   missing dependency rather than refusing to compile.
//! * [`run`] is the command: presentation, and nothing else about fetching. `build`
//!   calls `ensure` directly and keeps its own narration; there is no quiet-mode flag
//!   asking a callee to guess what its caller intends to print.
//!
//! `--offline` is read deeper down, where the network itself is (see
//! [`crate::git::mirror`]): the warm path above never reaches it, and a fetch that
//! would cross the network is refused there, at the one place that could start it.
//!
//! All the work functions take a [`Layout`] and a ref name (the only two things they act
//! on) so they are testable without a `Ctx`, a `Cli`, or a clap parser anywhere near
//! them. They log, because a record is a record no matter who called; they write nothing
//! to stdout.

use anyhow::{Context, Result, bail};
use log::{debug, info, warn};

use crate::cli::RefArg;
use crate::commands::{Ctx, Exit, emit_json};
use crate::git::mirror::{self, Mirror};
use crate::hints::Hints;
use crate::invocation::{skot, skot_args};
use crate::layout::Layout;

/// What a fetch learned, for whichever layer presents it.
#[derive(Debug, serde::Serialize)]
pub struct SyncOutcome {
    pub mirror: String,
    pub url: String,
    /// False when the mirror already existed — an update, not a first fetch.
    pub created: bool,
    /// Whether the network was used. False means the mirror already had the ref: the
    /// report exists so a script can tell "synced" from "nothing to do".
    pub fetched: bool,
    pub head: String,
    pub branches: Vec<String>,
    pub tags: Vec<String>,
}

/// Fetch from `upstream.url`, creating the mirror on the first run, and contacting the
/// upstream even when the mirror is already warm: the explicit "update me", for callers
/// that mean it regardless of what the run asked for. This is the first fetch of a
/// project, and any refusal to guess "warm enough".
///
/// A ref the upstream does not have is an error here, not a silent success: the caller
/// asked for an update, so "there is nothing to update to" is the answer it needs.
pub fn fetch(layout: &Layout, ref_name: Option<&str>) -> Result<SyncOutcome> {
    sync(layout, ref_name, true)
}

/// Fetch only what the mirror cannot already serve.
///
/// With the ref in the mirror there is no network call at all: no connection, nothing to
/// negotiate, and no failure when the connection would be down: the mirror is the local
/// answer, which is exactly the state a warm build should run in. A run that asked for
/// `--refresh` is the exception, read in the shared body below. A missing ref (or a
/// missing mirror) fetches just that ref, narrowed by name.
pub fn ensure(layout: &Layout, ref_name: Option<&str>) -> Result<SyncOutcome> {
    sync(layout, ref_name, false)
}

/// The shared body: decide whether the network is needed at all, then do or don't.
fn sync(layout: &Layout, ref_name: Option<&str>, refresh: bool) -> Result<SyncOutcome> {
    // `--refresh` describes the run, so it lives on the layout: whichever entry point
    // got here, a run that asked for it refreshes.
    let refresh = refresh || layout.refresh();
    let config = layout.config()?;
    // The URL is only needed to *start* a mirror: a warm one re-fetches from the URL its
    // own `origin` records, and a warm ref needs neither. So a missing `upstream.url` is
    // an error only on the path that would have used it, never a reason to refuse an
    // answer the mirror can already give.
    let url = config
        .upstream
        .url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(str::to_owned);

    // What is worth having: the ref the caller named, else the project's default. It is
    // both the thing to report on and the thing that narrows the fetch when it comes to
    // that; a commit id names no ref upstream and fetches nothing.
    let wanted = ref_name
        .map(str::to_owned)
        .or_else(|| config.upstream.default_ref.clone());
    let unfetchable = wanted.as_deref().is_some_and(mirror::is_commit_id);

    let existed = layout.mirror_dir().join("HEAD").is_file();

    // A commit id names no ref upstream: fetch cannot bring it, whether the mirror is
    // warm (it may already hold it, so report that) or cold (there is nothing to ask for).
    if unfetchable {
        let wanted = wanted.as_deref().unwrap_or_default();
        if existed {
            let mirror = Mirror::open(layout)?;
            if let Ok(sha) = mirror.resolve(wanted) {
                info!(target: "sync", "have {sha}");
                debug!(target: "sync", "mirror {}", mirror.path().display());
                return report(&mirror, url.as_deref(), false, false);
            }
        }
        bail!(
            "`{wanted}` is not in the mirror, and a commit id cannot be fetched by name.\n\
             Sync the branch or tag that contains it."
        );
    }

    // The warm path: the mirror can already serve the ref, so no connection is made,
    // no round-trip, and no failure because the network happens to be down. `--refresh`
    // is how an update is asked for; the default does not spend a round-trip confirming
    // what it can see.
    if existed
        && !refresh
        && let Some(wanted) = &wanted
    {
        let mirror = Mirror::open(layout)?;
        if let Ok(sha) = mirror.resolve(wanted) {
            let seen = if wanted == &sha {
                format!("have {sha}")
            } else {
                format!("have {wanted} at {sha}")
            };
            info!(target: "sync", "{seen}");
            debug!(target: "sync", "mirror {}", mirror.path().display());
            return report(&mirror, url.as_deref(), false, false);
        }
    }

    // The network path: the mirror is cold, the ref is missing, or an update was asked
    // for. The fetch takes exactly `wanted` (or the whole upstream when nothing names a
    // ref), so one checked-out tree does not drag every branch and tag behind it.
    let mirror = if existed {
        let mirror = Mirror::open(layout)?;
        mirror.sync(url.as_deref(), wanted.as_deref())?;
        mirror
    } else {
        // Starting a mirror is the one place the configured URL is required.
        let Some(url) = url.as_deref() else {
            bail!(
                "no `upstream.url` in {}.\n\
                 Add one, or re-run `{}`.",
                layout.config_file().display(),
                skot_args("init", "--url <repository>")
            );
        };
        // Before the work, not after: a first fetch is minutes of network and the log is
        // the only sign it has begun.
        info!(target: "sync", "mirroring {url}");
        Mirror::open_or_clone(layout, url, wanted.as_deref())?
    };

    if let Some(wanted) = &wanted
        && mirror.resolve(wanted).is_err()
    {
        bail!(
            "`{wanted}` is not in the mirror, and the upstream does not have it either.\n\
             Check the spelling; a ref published only just may need a moment."
        );
    }

    let refs = mirror.refs()?;
    info!(
        target: "sync",
        "{} {} branch refs and {} tag refs",
        if existed { "updated" } else { "fetched" },
        refs.branches.len(),
        refs.tags.len()
    );
    debug!(target: "sync", "mirror {}", mirror.path().display());

    // Naming the newest version tag is the fastest way to tell whether a sync actually
    // brought anything new.
    if let Some(latest) = mirror.version_tags().ok().and_then(|t| t.last().cloned()) {
        debug!(target: "sync", "latest version tag {latest}");
    }
    // A `default-ref` that does not resolve is the single most common misconfiguration,
    // and it shows up much later as "no ref to apply against" from a different command.
    // Only the un-narrowed fetch was supposed to bring it, though: a narrow fetch of some
    // other ref leaves the default for its own fetch, on demand, and a warning about that
    // would be noise on every `sync <other-ref>`.
    if wanted.is_none()
        && let Some(wanted) = config.upstream.default_ref.as_deref()
        && mirror.resolve(wanted).is_err()
    {
        warn!(
            target: "sync",
            "`{wanted}` is not in the mirror, so `upstream.default-ref` does not resolve. \
             Either it is a typo, or it has not been published yet."
        );
    }

    report(&mirror, url.as_deref(), !existed, true)
}

/// Assemble the outcome both paths return: the same object whether the network was used
/// or the mirror already had the answer. The mirror's own `origin` is the URL of record;
/// the configured one only stands in when the mirror cannot say.
fn report(mirror: &Mirror, url: Option<&str>, created: bool, fetched: bool) -> Result<SyncOutcome> {
    let refs = mirror.refs()?;
    let head = mirror
        .resolve("HEAD")
        .unwrap_or_else(|_| "(unresolved)".to_owned());
    let url = match mirror.remote_url() {
        Ok(url) => url,
        Err(_) => url.unwrap_or_default().to_owned(),
    };
    Ok(SyncOutcome {
        mirror: mirror.path().display().to_string(),
        url,
        created,
        fetched,
        head,
        branches: refs.branches.into_iter().map(|(n, _)| n).collect(),
        tags: refs.tags.into_iter().map(|(n, _)| n).collect(),
    })
}

/// `skot sync`: fetch what is missing (or, with `--refresh`, everything), then present.
/// The only place a document reaches stdout.
///
/// The fetch itself is [`ensure`]: a run's `--refresh` is already on the layout, which
/// is where the shared body reads it.
pub fn run(ctx: &Ctx, args: &RefArg) -> Result<Exit> {
    let out = ensure(&ctx.layout, args.ref_name.as_deref())?;

    if ctx.cli.json {
        emit_json(&out)?;
        return Ok(Exit::Success);
    }

    Hints::new("next")
        .entry(skot("build"), "the whole run: fetch, apply, build")
        .entry(
            skot_args("check", "<ref>"),
            "verify the series applies, without building",
        )
        .print();
    Ok(Exit::Success)
}

/// Resolve `name` against the mirror, fetching it first when the mirror cannot serve it.
///
/// This is the choke point every ref-reading command goes through, so it is where the
/// cargo principle lives: synchronize what is missing, then act. A mirror that already
/// has the ref pays no network (the common case, unchanged); one that does not gets the
/// ref fetched rather than a referral to go run `skot sync` first. What is left as an
/// error is what only an error can be: the remote is unreachable, or the upstream does
/// not have the ref.
pub fn resolve_in_mirror(layout: &Layout, name: &str) -> Result<(Mirror, crate::git::Sha)> {
    ensure(layout, Some(name))?;
    resolve_local(layout, name)
}

/// Resolve `name` against the mirror that is already on disk. No fetch, no create.
///
/// For a caller that has ensured the ref earlier in the same run (build's sync phase):
/// the choke point has been paid, and this is only the lookup half of it. Everyone else
/// wants [`resolve_in_mirror`], which fetches what the mirror cannot serve.
pub fn resolve_local(layout: &Layout, name: &str) -> Result<(Mirror, crate::git::Sha)> {
    let mirror = Mirror::open(layout).context(format!(
        "opening the mirror at {}",
        layout.mirror_dir().display()
    ))?;
    let sha = mirror
        .resolve(name)
        .context(format!("cannot resolve `{name}` against the mirror"))?;
    Ok((mirror, sha))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command};
    use clap::Parser as _;

    /// A project on disk. No `Cli`, no clap: `fetch` reads a `Layout`, so that is all a
    /// test has to build.
    fn fixture(dir: &std::path::Path, url: &str) -> Layout {
        std::fs::create_dir_all(dir).unwrap();
        let layout = Layout::new(dir.to_path_buf());
        std::fs::write(
            layout.config_file(),
            format!("[upstream]\nurl = \"{url}\"\n"),
        )
        .unwrap();
        layout
    }

    /// A local bare repo standing in for the network.
    fn upstream(dir: &std::path::Path) -> String {
        let path = dir.join("upstream.git");
        let repo = git2::Repository::init_bare(&path).unwrap();
        let tree_oid = repo.treebuilder(None).unwrap().write().unwrap();
        let who = git2::Signature::now("t", "t@e").unwrap();
        {
            let tree = repo.find_tree(tree_oid).unwrap();
            repo.commit(Some("refs/heads/main"), &who, &who, "m", &tree, &[])
                .unwrap();
            repo.set_head("refs/heads/main").unwrap();
        }
        drop(repo);
        path.to_str().unwrap().to_owned()
    }

    #[test]
    fn sync_creates_the_mirror_and_records_where_it_came_from() {
        let tmp = tempfile::tempdir().unwrap();
        let url = upstream(tmp.path());
        let layout = fixture(&tmp.path().join("proj"), &url);

        let out = fetch(&layout, None).unwrap();

        assert!(out.created, "the first fetch must report itself as one");
        assert_eq!(out.url, url);
        assert_eq!(out.branches, ["main"]);
        assert!(layout.mirror_dir().join("HEAD").is_file());

        let mirror = Mirror::open(&layout).unwrap();
        assert_eq!(mirror.remote_url().unwrap(), url);
        let branches = mirror.refs().unwrap().branches;
        assert_eq!(branches.len(), 1);
        assert_eq!(branches[0].0, "main");
        assert_eq!(
            branches[0].1.as_deref(),
            Some(mirror.resolve("main").unwrap().as_str())
        );
    }

    #[test]
    fn sync_is_idempotent_and_keeps_the_url() {
        let tmp = tempfile::tempdir().unwrap();
        let url = upstream(tmp.path());
        let layout = fixture(&tmp.path().join("proj"), &url);

        let first_outcome = fetch(&layout, None).unwrap();
        let first = Mirror::open(&layout).unwrap().resolve("main").unwrap();
        let second_outcome = fetch(&layout, None).unwrap();

        assert!(first_outcome.created);
        assert!(!second_outcome.created, "a repeat fetch re-cloned");

        let mirror = Mirror::open(&layout).unwrap();
        assert_eq!(mirror.remote_url().unwrap(), url);
        assert_eq!(
            mirror.resolve("main").unwrap(),
            first,
            "a second sync moved a ref"
        );
    }

    #[test]
    fn a_missing_url_says_where_to_put_one() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = fixture(tmp.path(), "");

        let err = fetch(&layout, None).unwrap_err().to_string();
        assert!(err.contains("upstream.url"), "got {err}");
        assert!(
            err.contains(&skot_args("init", "--url")),
            "no remedy offered: {err}"
        );
    }

    #[test]
    fn resolve_in_mirror_fetches_a_ref_the_mirror_does_not_have() {
        // The choke point every ref-reading command goes through: a ref the mirror
        // lacks is fetched on the spot, not deferred to a `skot sync` round trip.
        let tmp = tempfile::tempdir().unwrap();
        let url = upstream_with_branch(tmp.path(), "other");
        let layout = fixture(&tmp.path().join("proj"), &url);
        fetch(&layout, Some("main")).unwrap();

        let (mirror, sha) = resolve_in_mirror(&layout, "other").unwrap();

        assert_eq!(mirror.resolve("other").unwrap(), sha);
    }

    #[test]
    fn resolve_in_mirror_without_a_url_says_where_to_put_one() {
        // What is left as an error after the fetch has been attempted: there is nowhere
        // to fetch from. The advice names the fix rather than another command to run.
        let tmp = tempfile::tempdir().unwrap();
        let layout = fixture(&tmp.path().join("proj"), "");

        let err = match resolve_in_mirror(&layout, "main") {
            Err(e) => e.to_string(),
            Ok(_) => panic!("there is nowhere to fetch from, so this must not resolve"),
        };

        assert!(err.contains("upstream.url"), "got {err}");
        assert!(
            err.contains(&skot_args("init", "--url")),
            "no remedy offered: {err}"
        );
    }

    #[test]
    fn resolve_local_is_a_lookup_and_never_starts_a_mirror() {
        // The half of the choke point a caller uses when the ref was ensured earlier in
        // the same run: no fetch, no clone, just the mirror that must already be there.
        let tmp = tempfile::tempdir().unwrap();
        let url = upstream(tmp.path());
        let layout = fixture(&tmp.path().join("proj"), &url);

        let err = match resolve_local(&layout, "main") {
            Err(e) => e.to_string(),
            Ok(_) => panic!("there is no mirror yet, so this must not resolve"),
        };
        assert!(err.contains("opening the mirror"), "got {err}");
        assert!(
            !layout.mirror_dir().exists(),
            "resolve_local must not create the mirror"
        );

        fetch(&layout, None).unwrap();
        let (_mirror, sha) = resolve_local(&layout, "main").unwrap();
        assert_eq!(
            Mirror::open(&layout).unwrap().resolve("main").unwrap(),
            sha,
            "got {sha}"
        );
    }

    /// A bare repo standing in for the network, with an extra branch on top of `main`.
    fn upstream_with_branch(dir: &std::path::Path, branch: &str) -> String {
        let url = upstream(dir);
        let repo = git2::Repository::open_bare(dir.join("upstream.git")).unwrap();
        let main = repo
            .revparse_single("main")
            .unwrap()
            .peel_to_commit()
            .unwrap();
        let tree = main.tree().unwrap();
        let who = git2::Signature::now("t", "t@e").unwrap();
        repo.commit(
            Some(&format!("refs/heads/{branch}")),
            &who,
            &who,
            branch,
            &tree,
            &[&main],
        )
        .unwrap();
        url
    }

    #[test]
    fn ensure_never_touches_the_network_when_the_mirror_has_the_ref() {
        let tmp = tempfile::tempdir().unwrap();
        let url = upstream(tmp.path());
        let layout = fixture(&tmp.path().join("proj"), &url);
        fetch(&layout, Some("main")).unwrap();

        // The URL now points nowhere: a fetch would fail loudly, so a pass here proves
        // none was attempted. The warm mirror is the whole answer.
        std::fs::write(
            layout.config_file(),
            "[upstream]\nurl = \"/nonexistent/never.git\"\ndefault-ref = \"main\"\n",
        )
        .unwrap();
        let out = ensure(&layout, Some("main")).unwrap();

        assert!(!out.fetched, "the mirror had main, so nothing was fetched");
        assert!(!out.created, "the mirror already existed");
        let mut branches = out.branches;
        branches.sort();
        assert_eq!(branches, ["main"]);
    }

    #[test]
    fn ensure_fetches_a_ref_the_mirror_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let url = upstream_with_branch(tmp.path(), "other");
        let layout = fixture(&tmp.path().join("proj"), &url);
        fetch(&layout, Some("main")).unwrap();

        let out = ensure(&layout, Some("other")).unwrap();

        assert!(
            out.fetched,
            "`other` was not in the mirror, so it was fetched"
        );
        assert!(!out.created, "the mirror already existed");
        let mut branches = out.branches;
        branches.sort();
        assert_eq!(branches, ["main", "other"]);
    }

    #[test]
    fn a_ref_the_upstream_lacks_is_an_error_not_a_quiet_success() {
        let tmp = tempfile::tempdir().unwrap();
        let url = upstream(tmp.path());
        let layout = fixture(&tmp.path().join("proj"), &url);

        let err = fetch(&layout, Some("nope")).unwrap_err().to_string();

        assert!(err.contains("nope"), "the ref must be named: {err}");
        assert!(err.contains("upstream does not have"), "got {err}");
    }

    #[test]
    fn the_sync_subcommand_takes_an_optional_ref() {
        let cli = Cli::parse_from(["skot", "sync"]);
        let Command::Sync(s) = cli.command else {
            panic!("expected sync")
        };
        assert!(s.ref_name.is_none());
        assert!(
            !cli.refresh,
            "without --refresh sync is the local-first one"
        );
        let cli = Cli::parse_from(["skot", "sync", "--refresh", "release/2.0"]);
        let Command::Sync(s) = cli.command else {
            panic!("expected sync")
        };
        assert_eq!(s.ref_name.as_deref(), Some("release/2.0"));
        assert!(cli.refresh, "--refresh belongs to the run, not to sync");
    }

    #[test]
    fn ensure_refreshes_when_the_run_asked_for_it() {
        // `--refresh` rides on the layout, so the fetch phase refreshes wherever it runs:
        // the mirror is warm, then its own `origin` points somewhere that cannot answer,
        // and a loud failure is the only proof that the fetch was attempted at all.
        let tmp = tempfile::tempdir().unwrap();
        let url = upstream(tmp.path());
        let layout = fixture(&tmp.path().join("proj"), &url);
        fetch(&layout, Some("main")).unwrap();
        git2::Repository::open_bare(layout.mirror_dir())
            .unwrap()
            .remote_set_url("origin", "/nonexistent/never.git")
            .unwrap();

        let err = ensure(&layout.with_refresh(true), Some("main"))
            .unwrap_err()
            .to_string();

        assert!(err.contains("never.git"), "the fetch must run: {err}");
    }

    #[test]
    fn offline_will_not_fetch_from_a_url_on_the_network() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = fixture(
            &tmp.path().join("proj"),
            "https://example.invalid/upstream.git",
        );

        let err = fetch(&layout.with_offline(true), None)
            .unwrap_err()
            .to_string();

        assert!(err.contains("--offline"), "the flag is named: {err}");
        assert!(err.contains("example.invalid"), "and so is the URL: {err}");
        assert!(
            !layout.mirror_dir().exists(),
            "refused before anything was written"
        );
    }

    #[test]
    fn offline_still_fetches_from_a_local_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let url = upstream(tmp.path());
        let layout = fixture(&tmp.path().join("proj"), &url);

        let out = fetch(&layout.with_offline(true), None).unwrap();

        assert!(out.created, "a local directory is not the network");
        assert_eq!(out.branches, ["main"]);
    }

    #[test]
    fn offline_serves_from_the_mirror_it_already_has() {
        let tmp = tempfile::tempdir().unwrap();
        let url = upstream(tmp.path());
        let layout = fixture(&tmp.path().join("proj"), &url);
        fetch(&layout, Some("main")).unwrap();
        std::fs::write(
            layout.config_file(),
            "[upstream]\nurl = \"https://example.invalid/upstream.git\"\ndefault-ref = \"main\"\n",
        )
        .unwrap();

        let out = ensure(&layout.with_offline(true), Some("main")).unwrap();

        assert!(!out.fetched, "the cached mirror is the whole answer");
        assert_eq!(out.branches, ["main"]);
    }

    #[test]
    fn offline_wins_over_refresh_for_a_url_on_the_network() {
        // Both flags, warm mirror, origin on the network: `--refresh` skips the warm
        // path and `--offline` refuses what that uncovers. No connection either way,
        // and the refusal is the whole answer rather than a fetch that half-happens.
        let tmp = tempfile::tempdir().unwrap();
        let url = upstream(tmp.path());
        let layout = fixture(&tmp.path().join("proj"), &url);
        fetch(&layout, Some("main")).unwrap();
        git2::Repository::open_bare(layout.mirror_dir())
            .unwrap()
            .remote_set_url("origin", "https://example.invalid/upstream.git")
            .unwrap();

        let err = ensure(&layout.with_refresh(true).with_offline(true), Some("main"))
            .unwrap_err()
            .to_string();

        assert!(err.contains("--offline"), "got {err}");
        assert!(err.contains("example.invalid"), "and the URL: {err}");
    }

    #[test]
    fn refresh_and_offline_together_still_read_a_local_directory() {
        // The pair is only a contradiction for a network transport: a local directory
        // can be re-read as often as the run likes.
        let tmp = tempfile::tempdir().unwrap();
        let url = upstream(tmp.path());
        let layout = fixture(&tmp.path().join("proj"), &url);

        let out = ensure(&layout.with_refresh(true).with_offline(true), Some("main")).unwrap();

        assert!(out.fetched, "a local re-read is still a fetch");
    }
}
