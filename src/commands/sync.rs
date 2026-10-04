//! `sync`: mirror the upstream repository, and keep its refs current.
//!
//! This is where upstream itself is fetched. `import` fetches too — a fork or a pull
//! request — but only ever into this same mirror, through `git::mirror`, because that is
//! where every command already reads from. Everything else just reads the mirror, which is
//! what makes `check` and `apply` work offline.
//!
//! Two functions, with a deliberate boundary between them:
//!
//! * [`fetch`] does the work and reports what it found. It takes a [`Layout`] and a ref
//!   name — the only two things it acts on — so it is testable without a `Ctx`, a `Cli`,
//!   or a clap parser anywhere near it. It logs, because a record is a record no matter
//!   who called; it writes nothing to stdout.
//! * [`run`] is the command. It orchestrates the lifetime: call the work, then decide
//!   presentation — the one JSON document if `--json`, the "what to do next" advice if
//!   not. `build` calls `fetch` directly and keeps its own narration; there is no
//!   quiet-mode flag asking a callee to guess what its caller intends to print.

use anyhow::{Context, Result, bail};
use log::{debug, info, warn};

use crate::cli::RefArg;
use crate::commands::{Ctx, Exit, emit_json};
use crate::git::mirror::Mirror;
use crate::hints::Hints;
use crate::layout::Layout;

/// What a fetch learned, for whichever layer presents it.
#[derive(Debug, serde::Serialize)]
pub struct SyncOutcome {
    pub mirror: String,
    pub url: String,
    /// False when the mirror already existed — an update, not a first fetch.
    pub created: bool,
    pub head: String,
    pub branches: Vec<String>,
    pub tags: Vec<String>,
}

/// Fetch from `upstream.url`, creating the mirror on the first run.
///
/// Presentation decisions are not taken here. `--json` is already accounted for: the logger
/// was silenced at startup and `ui::Spinner` declines to draw, so nothing this does can
/// reach stdout or corrupt a document someone else is writing.
pub fn fetch(layout: &Layout, ref_name: Option<&str>) -> Result<SyncOutcome> {
    let config = layout.config()?;
    let url = config
        .upstream
        .url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty());
    let Some(url) = url else {
        bail!(
            "no `upstream.url` in {}.\n\
             Add one, or re-run `skot init --url <repository>`.",
            layout.config_file().display()
        );
    };

    let existed = layout.mirror_dir().join("HEAD").is_file();
    let mirror = if existed {
        let mirror = Mirror::open(layout)?;
        mirror.sync(ref_name)?;
        mirror
    } else {
        // Before the work, not after: a first fetch is minutes of network and the log is
        // the only sign it has begun.
        info!(target: "sync", "mirroring {url}");
        Mirror::open_or_clone(layout, url, ref_name)?
    };

    let refs = mirror.refs()?;
    let head = mirror
        .resolve("HEAD")
        .unwrap_or_else(|_| "(unresolved)".to_owned());

    info!(
        target: "sync",
        "{} {} branch refs and {} tag refs",
        if existed { "updated" } else { "fetched" },
        refs.branches.len(),
        refs.tags.len()
    );
    debug!(target: "sync", "mirror {}", mirror.path().display());

    // Naming the newest version tag is the fastest way to tell whether a sync actually
    // brought anything new, and whether the default ref in the config still exists.
    if let Some(latest) = mirror.version_tags().ok().and_then(|t| t.last().cloned()) {
        debug!(target: "sync", "latest version tag {latest}");
    }
    // A `default-ref` that does not resolve is the single most common misconfiguration, and it
    // shows up much later as "no ref to apply against" from a different command.
    if let Some(wanted) = config.upstream.default_ref.as_deref()
        && mirror.resolve(wanted).is_err()
    {
        warn!(
            target: "sync",
            "`{wanted}` is not in the mirror, so `upstream.default-ref` does not resolve. \
             Either it is a typo, or it has not been published yet."
        );
    }

    Ok(SyncOutcome {
        mirror: mirror.path().display().to_string(),
        url: mirror.remote_url().unwrap_or_else(|_| url.to_owned()),
        created: !existed,
        head,
        branches: refs.branches.into_iter().map(|(n, _)| n).collect(),
        tags: refs.tags.into_iter().map(|(n, _)| n).collect(),
    })
}

/// `skot sync`: fetch, then present. The only place a document reaches stdout.
pub fn run(ctx: &Ctx, args: &RefArg) -> Result<Exit> {
    let out = fetch(&ctx.layout, args.ref_name.as_deref())?;

    if ctx.cli.json {
        emit_json(&out)?;
        return Ok(Exit::Success);
    }

    Hints::new("next")
        .entry("skot build", "the whole run: fetch, apply, build, verify")
        .entry(
            "skot check <ref>",
            "verify the series applies, without building",
        )
        .print();
    Ok(Exit::Success)
}

/// Resolve `name` against the mirror, with a message that says what to do when the
/// mirror is missing rather than "no such ref".
pub fn resolve_in_mirror(layout: &Layout, name: &str) -> Result<(Mirror, crate::git::Sha)> {
    let mirror = Mirror::open(layout).map_err(|e| {
        anyhow::anyhow!(
            "{e}\n\
             Run `skot sync <ref>` to fetch it."
        )
    })?;
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
        assert!(err.contains("skot init --url"), "no remedy offered: {err}");
    }

    #[test]
    fn resolve_in_mirror_points_at_sync_when_there_is_no_mirror() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path().to_path_buf());

        let err = match resolve_in_mirror(&layout, "main") {
            Err(e) => e.to_string(),
            Ok(_) => panic!("there is no mirror, so this must not resolve"),
        };
        assert!(err.contains("skot sync"), "got {err}");
    }

    #[test]
    fn the_sync_subcommand_takes_an_optional_ref() {
        let cli = Cli::parse_from(["skot", "sync"]);
        let Command::Sync(s) = cli.command else {
            panic!("expected sync")
        };
        assert!(s.ref_name.is_none());
        let cli = Cli::parse_from(["skot", "sync", "release/2.0"]);
        let Command::Sync(s) = cli.command else {
            panic!("expected sync")
        };
        assert_eq!(s.ref_name.as_deref(), Some("release/2.0"));
    }
}
