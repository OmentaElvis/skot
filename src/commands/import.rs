//! `import`: turn commits from somewhere else into a patch in the series.
//!
//! The range can be refs already in the mirror, a repository skot does not track (a fork,
//! a local clone), or a pull request. All three end the same way: two trees in the mirror,
//! one diff between them, one patch file, one line in the series.
//!
//! The network stays in the mirror layer, as everywhere else. `--repo` and `--pr` fetch
//! into `.skot/mirror.git` through [`Mirror::fetch_refspecs`], so once the fetch finishes
//! there is nothing external left to read from: the objects sit where every other command
//! already looks for them, and `check` verifies the imported patch like any other.
//!
//! What import does *not* do is apply anything. The patch joins the series at the end and
//! the sandbox is untouched, so the next step is the same as after `new`: verify with
//! `skot check`, or take the whole run with `skot build`.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use log::{debug, info};
use serde_json::json;

use crate::cli::ImportArgs;
use crate::commands::{Ctx, Exit, authoring, emit_json};
use crate::git::diff;
use crate::git::mirror::Mirror;
use crate::git::{self, Sha};
use crate::hints::Hints;
use crate::layout::Layout;
use crate::series;

/// Where a `--repo` fetch lands: out of the way of `refs/heads` (which `skot sync` treats
/// as upstream's own) and out of the way of `refs/tags`, so an imported branch can share a
/// name with both without overwriting anything.
const NS: &str = "refs/skot-import";

/// What `import` did, for whichever layer presents it.
#[derive(Debug)]
struct Imported {
    patch: String,
    path: PathBuf,
    /// The line the patch was inserted at, or `None` with `--no-register`.
    series_line: Option<usize>,
    from: String,
    from_sha: Sha,
    to: String,
    to_sha: Sha,
    commits: usize,
    summary: String,
}

/// The two ends as written, before anything is resolved against the mirror.
struct Request {
    /// The `to` end: a ref name, or `refs/pull/N/head` for `--pr`.
    to: String,
    /// Where the patch's diff starts.
    base: Base,
    /// The pull request to fetch, if that is what was asked for.
    pull: Option<u64>,
}

/// How the `from` end is chosen.
enum Base {
    /// A ref, used as written.
    Named(String),
    /// The merge base of two refs, from `A...B`.
    MergeBaseOf(String, String),
    /// The merge base with `upstream.default-ref`, when nothing specified a base.
    MergeWithDefault,
}

/// The two ends, resolved to commit ids in the mirror.
struct Ends {
    from: String,
    from_sha: Sha,
    to: String,
    to_sha: Sha,
}

/// `skot import`: resolve the range, fetch whatever it is missing, write the patch.
pub fn run(ctx: &Ctx, args: &ImportArgs) -> Result<Exit> {
    let done = import(&ctx.layout, args)?;

    if ctx.cli.json {
        emit_json(&json!({
            "patch": done.patch,
            "path": done.path.display().to_string(),
            "series_line": done.series_line,
            "from": done.from,
            "from_sha": done.from_sha,
            "to": done.to,
            "to_sha": done.to_sha,
            "commits": done.commits,
            "summary": done.summary,
        }))?;
        return Ok(Exit::Success);
    }

    info!(
        "imported {} into {}",
        super::push::count(done.commits, "commit", "commits"),
        done.patch
    );
    debug!("  {} -> {}", done.from, done.to);
    debug!("  {}", done.summary);
    if done.series_line.is_none() {
        info!("Not added to the series (--no-register).");
    }
    Hints::new("next")
        .entry("skot check", "verify the series applies")
        .entry("skot build", "the whole run: fetch, apply, build, verify")
        .print();
    Ok(Exit::Success)
}

/// The plain work behind [`run`]: no clap, no stdout, no verdict.
///
/// Everything that can be rejected is rejected before the first fetch: a mistyped range
/// should cost no network.
fn import(layout: &Layout, args: &ImportArgs) -> Result<Imported> {
    let (config, mut series) = layout.project()?;
    let request = request(args)?;

    let mirror = Mirror::open(layout).map_err(|_| {
        anyhow::anyhow!(
            "no mirror at {}.\n\
             Run `skot sync` first: it is what every command reads refs and objects from.",
            layout.mirror_dir().display()
        )
    })?;

    if let Some(source) = args.repo.as_deref() {
        mirror
            .fetch_refspecs(
                source,
                "import",
                &[
                    format!("+refs/heads/*:{NS}/heads/*"),
                    format!("+refs/tags/*:{NS}/tags/*"),
                ],
                "fetching the source repository",
            )
            .context(format!("fetching from {source}"))?;
    }
    if let Some(number) = request.pull {
        let url = mirror.remote_url().context(
            "the mirror records no `origin`, so there is nowhere to fetch the pull request \
             from.\nRun `skot sync` first.",
        )?;
        mirror
            .fetch_refspecs(
                &url,
                "origin",
                &[format!("+refs/pull/{number}/head:refs/pull/{number}/head")],
                &format!("fetching pull request {number}"),
            )
            .context(format!("fetching `refs/pull/{number}/head` from {url}"))?;
    }

    let ends = resolve(&mirror, layout, &config, &request, args.repo.as_deref())?;

    let repo = mirror.repo();
    let old = git::commit_tree(repo, &ends.from_sha)
        .context(format!("reading the tree of {}", ends.from))?;
    let new =
        git::commit_tree(repo, &ends.to_sha).context(format!("reading the tree of {}", ends.to))?;
    if old.id() == new.id() {
        bail!(
            "`{}` and `{}` are the same tree, so there is nothing to import.",
            ends.from,
            ends.to
        );
    }
    let commits = count_between(repo, &ends.to_sha, &ends.from_sha)?;

    // The tip commit speaks for a single-commit import: its author, its date, its message.
    // A range of several commits has no one author, so it falls back to whoever is running
    // the import — the same stamp `new` gives a patch it creates.
    let tip = git2::Oid::from_str(&ends.to_sha)
        .ok()
        .and_then(|oid| repo.find_commit(oid).ok());
    let (tip_subject, tip_body) = match tip.as_ref().and_then(|c| c.message().ok()) {
        Some(message) => {
            let mut lines = message.lines();
            let subject = lines.next().map(str::to_owned);
            let body = lines.collect::<Vec<_>>().join("\n");
            (subject, body.trim().to_owned())
        }
        None => (None, String::new()),
    };

    let fallback = format!("Import {}..{}", ends.from, ends.to);
    let derived = tip_subject
        .filter(|s| !s.is_empty() && commits == 1)
        .unwrap_or(fallback);
    let slug = match &args.name {
        Some(name) => super::new::slug(name)?,
        None => filename_slug(&derived),
    };
    let prefix = series.len() + 1;
    let patch_name = series::default_patch_name(&prefix.to_string(), &slug);
    let path = layout.patches_dir().join(&patch_name);
    if path.exists() {
        bail!(
            "{} already exists.\n\
             Choose another name with `--name`, or remove it if it is stale.",
            path.display()
        );
    }

    // A name the user gave is the subject the same way it is for `new`: read back from the
    // filename, so `skot list` and the patch header say the same words. Without one, the
    // imported commit speaks for itself.
    let subject = if args.name.is_some() {
        authoring::subject_from_name(&patch_name)
    } else {
        derived
    };
    let own = authoring::signature(repo);
    let signature = match tip.as_ref() {
        Some(commit) if commits == 1 => commit.author(),
        _ => own,
    };
    let mut header = authoring::header_for(None, &subject, &signature);
    if commits == 1 {
        header.body = tip_body;
    }
    let text = diff::tree_to_tree_patch(repo, &old, &new, &header)
        .context(format!("rendering the import as {}", patch_name))?;
    let summary = diff::summarise(&text);

    // From here on the command writes, so the same lock that stands between two `refresh`
    // runs stands between two imports.
    let lock = layout.lock()?;
    std::fs::create_dir_all(layout.patches_dir()).context("creating the patch directory")?;
    std::fs::write(&path, &text).context(format!("writing {}", path.display()))?;
    let series_line = if args.no_register {
        None
    } else {
        let line = series.insert(&patch_name, series.len())?;
        series.save(layout)?;
        Some(line)
    };
    drop(lock);

    Ok(Imported {
        patch: patch_name,
        path,
        series_line,
        from: ends.from,
        from_sha: ends.from_sha,
        to: ends.to,
        to_sha: ends.to_sha,
        commits,
        summary,
    })
}

/// Read the command line as a [`Request`], rejecting every shape that cannot become one.
fn request(args: &ImportArgs) -> Result<Request> {
    if let Some(number) = args.pr {
        let base = match &args.from {
            Some(from) => Base::Named(from.clone()),
            None => Base::MergeWithDefault,
        };
        return Ok(Request {
            to: format!("refs/pull/{number}/head"),
            base,
            pull: Some(number),
        });
    }

    let Some(range) = args.range.as_deref() else {
        bail!(
            "nothing to import.\n\
             Give a range (`skot import main..my-branch`), a ref (`skot import my-branch`), \
             or a pull request (`skot import --pr 42`)."
        );
    };

    // `A...B` first: it contains `..` as well, so splitting on `..` alone would read it as
    // `A.` and `.B` — two refs that resolve for nobody.
    let (from, to, merge) = if let Some((a, b)) = range.split_once("...") {
        (Some(a), b, true)
    } else if let Some((a, b)) = range.split_once("..") {
        (Some(a), b, false)
    } else {
        (None, range, false)
    };
    let to = to.trim();

    let from = from.map(str::trim);
    if from.is_some() && args.from.is_some() {
        bail!(
            "`--from` and the `FROM..` half of `{range}` say the same thing.\n\
             Give one or the other."
        );
    }
    if range.contains("..") && (from.is_none_or(str::is_empty) || to.is_empty()) {
        bail!("`{range}` needs a ref on both sides of `..`.");
    }

    let base = match from {
        Some(from) if merge => Base::MergeBaseOf(from.to_owned(), to.to_owned()),
        Some(from) => Base::Named(from.to_owned()),
        None => match &args.from {
            Some(from) => Base::Named(from.clone()),
            None => Base::MergeWithDefault,
        },
    };
    Ok(Request {
        to: to.to_owned(),
        base,
        pull: None,
    })
}

/// The two ends as commit ids, after any fetch has put them in the mirror.
fn resolve(
    mirror: &Mirror,
    layout: &Layout,
    config: &crate::config::Config,
    request: &Request,
    source: Option<&str>,
) -> Result<Ends> {
    let to_sha = resolve_named(mirror, source, &request.to)?;

    let (from, from_sha) = match &request.base {
        Base::Named(name) => (name.clone(), resolve_named(mirror, source, name)?),
        Base::MergeBaseOf(a, b) => {
            let sha_a = resolve_named(mirror, source, a)?;
            let sha_b = resolve_named(mirror, source, b)?;
            let sha = merge_base(mirror, &sha_a, &sha_b)
                .context(format!("`{a}` and `{b}` share no history"))?;
            (format!("merge-base({a}, {b})"), sha)
        }
        Base::MergeWithDefault => {
            let default = config.upstream.default_ref.clone().context(format!(
                "cannot work out what `{}` is based on.\n\
                 Pass a base (`skot import --from <ref> {}`), or set `upstream.default-ref` \
                 in {}.",
                request.to,
                request.to,
                layout.config_file().display()
            ))?;
            let default_sha = mirror.resolve(&default).with_context(|| {
                format!("cannot resolve `upstream.default-ref` (`{default}`) in the mirror")
            })?;
            let sha = merge_base(mirror, &default_sha, &to_sha).context(format!(
                "`{default}` and `{}` share no history, so there is no base to import against",
                request.to
            ))?;
            (format!("merge-base({default}, {})", request.to), sha)
        }
    };

    Ok(Ends {
        from,
        from_sha,
        to: request.to.clone(),
        to_sha,
    })
}

/// Resolve one end of the range.
///
/// In `--repo` mode the fetched namespace is tried first, because the name was written
/// against that repository: `skot import --repo ../fork main..feat` means *that* repo's
/// `main` and `feat`. The mirror is the fallback, which is how the upstream end of a range
/// resolves when the fork does not carry it.
fn resolve_named(mirror: &Mirror, source: Option<&str>, name: &str) -> Result<Sha> {
    if let Some(source) = source {
        for namespace in [format!("{NS}/heads/"), format!("{NS}/tags/")] {
            if let Some(sha) = mirror.try_resolve(&format!("{namespace}{name}")) {
                return Ok(sha);
            }
        }
        if let Some(sha) = mirror.try_resolve(name) {
            return Ok(sha);
        }
        bail!(
            "`{name}` is neither a branch or tag in {source}, nor in the mirror.\n\
             With `--repo`, give a branch or tag name from that repository."
        );
    }
    mirror.resolve(name)
}

/// The common ancestor of two commits already in the mirror.
fn merge_base(mirror: &Mirror, a: &Sha, b: &Sha) -> Result<Sha> {
    let a = git2::Oid::from_str(a).context(format!("{a} is not a commit id"))?;
    let b = git2::Oid::from_str(b).context(format!("{b} is not a commit id"))?;
    mirror
        .repo()
        .merge_base(a, b)
        .map(|oid| oid.to_string())
        .context("the two refs share no history")
}

/// How many commits `FROM..TO` contains: the same count `git rev-list FROM..TO` prints.
fn count_between(repo: &git2::Repository, to: &Sha, from: &Sha) -> Result<usize> {
    let to = git2::Oid::from_str(to).context(format!("{to} is not a commit id"))?;
    let from = git2::Oid::from_str(from).context(format!("{from} is not a commit id"))?;
    let mut walk = repo.revwalk().context("counting the imported commits")?;
    walk.push(to).context(format!("walking {to}"))?;
    walk.hide(from).context(format!("excluding {from}"))?;
    let mut commits = 0;
    for commit in walk {
        commit.context("walking the imported commits")?;
        commits += 1;
    }
    Ok(commits)
}

/// A filename from text no one typed by hand: a commit subject, an import expression.
///
/// [`crate::commands::new::slug`] *rejects* what it cannot use, because a person picking a
/// name can always pick another one — a commit subject cannot, so anything unnameable here
/// becomes a dash instead of an error.
fn filename_slug(text: &str) -> String {
    let mut slug = String::with_capacity(text.len().min(60));
    let mut dashed = false;
    for c in text.chars() {
        if c.is_alphanumeric() {
            slug.extend(c.to_lowercase());
            dashed = false;
        } else if !dashed && !slug.is_empty() {
            slug.push('-');
            dashed = true;
        }
        if slug.len() >= 60 {
            break;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug.push_str("imported");
    }
    slug
}

#[cfg(test)]
mod slug_tests {
    use super::filename_slug;

    #[test]
    fn separators_collapse_into_one_dash() {
        assert_eq!(
            filename_slug("Fix: parser/handler (take 2)"),
            "fix-parser-handler-take-2"
        );
    }

    #[test]
    fn nothing_usable_becomes_a_placeholder() {
        assert_eq!(filename_slug("..."), "imported");
        assert_eq!(filename_slug("   "), "imported");
    }

    #[test]
    fn a_long_subject_stops_short_of_the_filename_limit() {
        let slug = filename_slug(&"word ".repeat(100));
        assert!(slug.len() <= 60, "got {} bytes: {slug}", slug.len());
        assert!(!slug.ends_with('-'));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testsupport;

    fn args(range: Option<&str>) -> ImportArgs {
        ImportArgs {
            range: range.map(str::to_owned),
            from: None,
            pr: None,
            repo: None,
            name: None,
            no_register: false,
        }
    }

    /// One commit on `ref_name` setting `f.txt` to `content`.
    ///
    /// The treebuilder is dropped here rather than in the caller, so the caller can drop
    /// the repository after committing without a borrow outliving it.
    fn commit_file(
        repo: &git2::Repository,
        ref_name: &str,
        message: &str,
        content: &[u8],
        parents: &[&git2::Commit<'_>],
    ) -> git2::Oid {
        let mut builder = repo.treebuilder(None).unwrap();
        builder
            .insert("f.txt", repo.blob(content).unwrap(), 0o100644)
            .unwrap();
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let who = git2::Signature::now("t", "t@e").unwrap();
        repo.commit(Some(ref_name), &who, &who, message, &tree, parents)
            .unwrap()
    }

    /// A project whose mirror holds `main` (f.txt = base) and, one commit later, `feat`
    /// (f.txt = second). `upstream.default-ref` is `main`.
    fn fixture() -> (tempfile::TempDir, Layout, String) {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, main) = testsupport::mirror_with_one_commit(tmp.path());
        std::fs::write(
            layout.config_file(),
            "[upstream]\nurl = \"unused\"\ndefault-ref = \"main\"\n",
        )
        .unwrap();

        let repo = git2::Repository::open_bare(layout.mirror_dir()).unwrap();
        let parent = repo
            .find_commit(git2::Oid::from_str(&main).unwrap())
            .unwrap();
        commit_file(
            &repo,
            "refs/heads/feat",
            "add the second line",
            b"second\n",
            &[&parent],
        );

        (tmp, layout, main)
    }

    #[test]
    fn a_range_becomes_a_patch_and_a_series_line() {
        let (_tmp, layout, main) = fixture();

        let done = import(&layout, &args(Some("main..feat"))).unwrap();

        assert_eq!(done.from, "main");
        assert_eq!(done.from_sha, main);
        assert_eq!(done.to, "feat");
        assert_eq!(done.commits, 1);
        assert_eq!(done.series_line, Some(1));

        let series = std::fs::read_to_string(layout.series_file()).unwrap();
        assert_eq!(series.trim(), done.patch);
        let text = std::fs::read_to_string(&done.path).unwrap();
        assert!(text.contains("Subject:"), "header missing:\n{text}");
        assert!(text.contains("+second"), "addition missing:\n{text}");
        assert!(text.contains("-base"), "removal missing:\n{text}");
    }

    #[test]
    fn a_bare_ref_takes_the_merge_base_with_the_default_ref() {
        let (_tmp, layout, main) = fixture();

        let done = import(&layout, &args(Some("feat"))).unwrap();

        assert_eq!(done.from, "merge-base(main, feat)");
        assert_eq!(
            done.from_sha, main,
            "the merge base of main and feat is main"
        );
        assert_eq!(done.commits, 1);
    }

    #[test]
    fn the_commit_subject_names_the_patch() {
        let (_tmp, layout, _main) = fixture();

        let done = import(&layout, &args(Some("feat"))).unwrap();

        assert_eq!(done.patch, "0001-add-the-second-line.patch");
        let text = std::fs::read_to_string(&done.path).unwrap();
        assert!(
            text.contains("Subject: [PATCH] add the second line"),
            "the imported commit should speak for itself:\n{text}"
        );
        assert!(
            text.contains("From: t <t@e>"),
            "a single-commit import keeps the commit's own author:\n{text}"
        );
    }

    #[test]
    fn three_dots_takes_the_merge_base_of_both_refs() {
        let (_tmp, layout, main) = fixture();

        let done = import(&layout, &args(Some("main...feat"))).unwrap();

        assert_eq!(done.from, "merge-base(main, feat)");
        assert_eq!(done.from_sha, main);
    }

    #[test]
    fn an_explicit_base_and_from_disagree_is_refused() {
        let (_tmp, layout, _main) = fixture();

        let err = import(
            &layout,
            &ImportArgs {
                from: Some("main".into()),
                ..args(Some("main..feat"))
            },
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("say the same thing"), "got {err}");
    }

    #[test]
    fn an_empty_range_is_refused() {
        let (_tmp, layout, _main) = fixture();

        let err = import(&layout, &args(Some("main..main")))
            .unwrap_err()
            .to_string();

        assert!(err.contains("same tree"), "got {err}");
    }

    #[test]
    fn with_nothing_to_import_it_says_what_would_work() {
        let (_tmp, layout, _main) = fixture();

        let err = import(&layout, &args(None)).unwrap_err().to_string();

        assert!(err.contains("skot import --pr"), "no remedy offered: {err}");
        assert!(err.contains("main..my-branch"), "no example offered: {err}");
    }

    #[test]
    fn a_half_range_is_refused() {
        let (_tmp, layout, _main) = fixture();

        for text in ["main..", "..feat"] {
            let err = import(&layout, &args(Some(text))).unwrap_err().to_string();
            assert!(err.contains("both sides"), "`{text}`: got {err}");
        }
    }

    #[test]
    fn no_register_writes_the_file_and_leaves_the_series_alone() {
        let (_tmp, layout, _main) = fixture();

        let done = import(
            &layout,
            &ImportArgs {
                no_register: true,
                ..args(Some("feat"))
            },
        )
        .unwrap();

        assert!(done.series_line.is_none());
        assert!(done.path.is_file(), "the patch file must still be written");
        assert!(
            !layout.series_file().exists(),
            "the series must be untouched"
        );
    }

    #[test]
    fn a_name_the_user_gives_wins_over_the_commit_subject() {
        let (_tmp, layout, _main) = fixture();

        let done = import(
            &layout,
            &ImportArgs {
                name: Some("the second line".into()),
                ..args(Some("feat"))
            },
        )
        .unwrap();

        assert_eq!(done.patch, "0001-the-second-line.patch");
        let text = std::fs::read_to_string(&done.path).unwrap();
        assert!(
            text.contains("Subject: [PATCH] the second line"),
            "a chosen name is the subject:\n{text}"
        );
    }

    /// A local repository standing in for a fork: `main` holds `f.txt = base`, `feat` is
    /// one commit ahead of it.
    fn fork(root: &std::path::Path) -> PathBuf {
        let path = root.join("fork");
        let repo = git2::Repository::init(&path).unwrap();
        let base = commit_file(&repo, "refs/heads/main", "base", b"base\n", &[]);
        let parent = repo.find_commit(base).unwrap();
        commit_file(
            &repo,
            "refs/heads/feat",
            "forked line",
            b"forked\n",
            &[&parent],
        );
        path
    }

    #[test]
    fn a_repo_fetch_lands_in_the_namespace_and_the_range_comes_from_there() {
        let (tmp, layout, _main) = fixture();
        let fork = fork(tmp.path());

        let done = import(
            &layout,
            &ImportArgs {
                repo: Some(fork.to_str().unwrap().to_owned()),
                ..args(Some("main..feat"))
            },
        )
        .unwrap();

        let text = std::fs::read_to_string(&done.path).unwrap();
        assert!(text.contains("+forked"), "the fork's own change:\n{text}");

        let mirror = Mirror::open(&layout).unwrap();
        assert!(
            mirror.try_resolve(&format!("{NS}/heads/feat")).is_some(),
            "the fetched branch must be reachable in the mirror"
        );
        assert!(
            mirror.repo().find_remote("import").is_ok(),
            "the source repository is the `import` remote"
        );
        assert!(
            mirror.repo().find_remote("origin").is_err(),
            "origin is `sync`'s remote and import must not invent one"
        );
    }

    /// A source repository with `main` and, on top of it, the head of pull request 7.
    fn pull_request_source(root: &std::path::Path) -> (PathBuf, String) {
        let path = root.join("upstream");
        let repo = git2::Repository::init(&path).unwrap();
        let base = commit_file(&repo, "refs/heads/main", "base", b"base\n", &[]);
        let parent = repo.find_commit(base).unwrap();
        let sha = commit_file(
            &repo,
            "refs/pull/7/head",
            "the pull request",
            b"pr\n",
            &[&parent],
        );
        (path, sha.to_string())
    }

    #[test]
    fn a_pull_request_is_fetched_from_origin_and_imported() {
        let tmp = tempfile::tempdir().unwrap();
        let (source, pr_sha) = pull_request_source(tmp.path());

        // The mirror as `sync` would have left it: bare, with `origin` at the source and
        // `main` fetched. The pull request ref is deliberately absent — `sync` never
        // fetches it, which is why `import` does.
        let layout = Layout::new(tmp.path().join("proj"));
        std::fs::create_dir_all(layout.mirror_dir()).unwrap();
        std::fs::write(
            layout.config_file(),
            format!(
                "[upstream]\nurl = \"{}\"\ndefault-ref = \"main\"\n",
                source.display()
            ),
        )
        .unwrap();
        std::fs::create_dir_all(layout.patches_dir()).unwrap();
        let mirror_repo = git2::Repository::init_bare(layout.mirror_dir()).unwrap();
        let mut remote = mirror_repo
            .remote("origin", source.to_str().unwrap())
            .unwrap();
        remote
            .fetch(&["+refs/heads/*:refs/heads/*"], None, None)
            .unwrap();
        drop(remote);
        drop(mirror_repo);

        let done = import(
            &layout,
            &ImportArgs {
                pr: Some(7),
                ..args(None)
            },
        )
        .unwrap();

        assert_eq!(done.to, "refs/pull/7/head");
        assert_eq!(done.to_sha, pr_sha);
        assert_eq!(done.commits, 1);
        assert_eq!(done.from_sha, {
            let mirror = Mirror::open(&layout).unwrap();
            mirror.resolve("main").unwrap()
        });
        let text = std::fs::read_to_string(&done.path).unwrap();
        assert!(text.contains("+pr"), "the pull request's change:\n{text}");
    }
}
