//! `patch import`: turn commits from somewhere else into a patch in the series.
//!
//! The range can be refs already in the mirror, a repository skot does not track (a fork,
//! a local clone, an unrelated project), or a pull request. All three end the same way:
//! two trees, one diff between them, one patch file, one line in the series.
//!
//! A source named with `--repo` is read where it lies. A local tree is opened and
//! resolved in place, like a working directory: its refs, its commits, and its own
//! history are all that matter, nothing shares history with anything, and nothing is
//! copied. A URL cannot be opened, so the range's refs are fetched into a store of that
//! one source under `.skot/sources/`, never into the mirror. With no `--repo`, the
//! mirror is where everything is read from, as everywhere else: the range's own refs
//! come from the origin through [`Mirror::fetch_refspecs`], and `check` verifies the
//! imported patch like any other.
//!
//! What import does *not* do is apply anything. The patch joins the series at the end (or
//! at `--at`), and the sandbox is untouched, so the next step is the same as after
//! `new`: verify with `skot check`, or take the whole run with `skot build`.
//!
//! Import is also repeatable. Re-running the same request is not a second copy: a patch
//! whose diff already matches is reported, not rewritten, and a patch recorded for the
//! same request with new ends — the pull request gained commits, or the base moved under
//! it — is rewritten in place, keeping its name and its series line. A patch you have
//! edited since is refused rather than overwritten, because the new version would
//! silently discard your work. Where a patch came from lives in an `X-Skot-Import` mail
//! header, which the header parser keeps along with everything else it does not model.
//! That line is how a later run recognizes the same request, and it never names a local
//! path: a source is recorded by the network origin it answers to when it has one, by a
//! digest of where it lies when it does not, and `X-Skot-Source` carries that origin
//! alongside when there is one worth carrying.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use log::{debug, info, warn};
use serde_json::json;

use crate::cli::ImportArgs;
use crate::commands::{Ctx, Exit, authoring, emit_json};
use crate::git::diff;
use crate::git::mirror::{Mirror, is_commit_id, is_local_url};
use crate::git::{self, Sha};
use crate::hints::Hints;
use crate::invocation::{skot, skot_args};
use crate::layout::Layout;
use crate::series;

/// What to say when a local `--repo` tree cannot answer for something the range reads
/// and its own `origin` might: the flag that grants that lookup, never assumed.
const TREE_UPSTREAM_HINT: &str =
    "If this tree's upstream carries what is missing, `--refresh` checks it there on your behalf.";

/// What `import` did, for whichever layer presents it.
#[derive(Debug)]
struct Imported {
    patch: String,
    path: PathBuf,
    /// The line the patch sits at, or `None` with `--no-register`.
    series_line: Option<usize>,
    from: String,
    from_sha: Sha,
    to: String,
    to_sha: Sha,
    /// True when the patch was extracted with `--reverse`: its diff runs from `to`
    /// back to `from`.
    reverse: bool,
    commits: usize,
    summary: String,
    /// How the run ended: a patch was written, an existing one was replaced, or none was.
    outcome: Outcome,
    /// Set only by a no-op that found the existing file missing from the series and put
    /// it back, so the report can say so.
    registered_now: bool,
    /// For an update: how many commits the range held before, when that is still known.
    previous_commits: Option<usize>,
}

/// How an import ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Created,
    Updated,
    Unchanged,
}

impl Outcome {
    /// The word for the layers that serialise it.
    fn as_str(self) -> &'static str {
        match self {
            Outcome::Created => "created",
            Outcome::Updated => "updated",
            Outcome::Unchanged => "unchanged",
        }
    }
}

/// The two ends as written, before anything is resolved against the mirror.
struct Request {
    /// The `to` end: a ref name, or the pull request's ref for `--pr` (the forge's
    /// namespace resolved from a number, or the full ref as written).
    to: String,
    /// Where the patch's diff starts.
    base: Base,
    /// The pull request to fetch, if that is what was asked for: the number as given,
    /// or the full ref name for a forge the tables do not name.
    pull: Option<String>,
}

/// How the `from` end is chosen.
enum Base {
    /// A ref, used as written.
    Named(String),
    /// The merge base of two refs, from `A...B`.
    MergeBaseOf(String, String),
    /// The merge base with `upstream.default-ref`, when nothing specified a base. A
    /// source named with `--repo` does not read the mirror's default for this: the
    /// coordinate is taken against its own repository, as its parent commit.
    MergeWithDefault,
}

/// The two ends, resolved to commit ids in the repository the range was read from: the
/// mirror, a `--repo` tree, or a `--repo` URL's store.
struct Ends {
    from: String,
    from_sha: Sha,
    to: String,
    to_sha: Sha,
}

/// Where the range is read from: a repository skot fills itself, or one the user named
/// with `--repo`, which is read where it lies.
enum Ground {
    /// A bare repository skot maintains: the project's mirror, or a `--repo` URL's
    /// store under `.skot/sources/`. Read the same way either way.
    Mirror(Mirror),
    /// A `--repo` local tree, opened at the path given and never written to.
    Tree(git2::Repository),
}

impl Ground {
    /// The repository the ends and the diff are read from.
    fn repo(&self) -> &git2::Repository {
        match self {
            Ground::Mirror(mirror) => mirror.repo(),
            Ground::Tree(repo) => repo,
        }
    }
}

/// `skot patch import`: resolve the range, fetch whatever it is missing, write the patch.
pub fn run(ctx: &Ctx, args: &ImportArgs) -> Result<Exit> {
    let done = import(&ctx.layout, args)?;

    if ctx.cli.json {
        emit_json(&json!({
            "series_name": ctx.layout.series(),
            "patch": done.patch,
            "path": done.path.display().to_string(),
            "series_line": done.series_line,
            "from": done.from,
            "from_sha": done.from_sha,
            "to": done.to,
            "to_sha": done.to_sha,
            "reverse": done.reverse,
            "commits": done.commits,
            "summary": done.summary,
            "outcome": done.outcome.as_str(),
            "previous_commits": done.previous_commits,
        }))?;
        return Ok(Exit::Success);
    }

    match done.outcome {
        Outcome::Created => info!(
            target: "import",
            "imported {} into {}",
            super::push::count(done.commits, "commit", "commits"),
            done.patch
        ),
        Outcome::Updated => match done.previous_commits {
            Some(before) => info!(
                target: "import",
                "updated {} ({} to {})",
                done.patch,
                super::push::count(before, "commit", "commits"),
                super::push::count(done.commits, "commit", "commits")
            ),
            None => info!(
                target: "import",
                "updated {} to {}",
                done.patch,
                super::push::count(done.commits, "commit", "commits")
            ),
        },
        Outcome::Unchanged => {
            info!(target: "import", "{} already imports this; nothing to do.", done.patch)
        }
    }
    if done.registered_now {
        info!(target: "import", "Added it to the series.");
    }
    debug!(target: "import", "  {} -> {}", done.from, done.to);
    debug!(target: "import", "  {}", done.summary);
    if done.series_line.is_none() {
        info!(target: "import", "Not added to the series (--no-register).");
    }
    Hints::new("next")
        .entry(skot("check"), "verify the series applies")
        .entry(skot("build"), "the whole run: fetch, apply, build")
        .print();
    Ok(Exit::Success)
}

/// The plain work behind [`run`]: no clap, no stdout, no verdict.
///
/// Everything that can be rejected is rejected before the first fetch: a mistyped range
/// should cost no network.
fn import(layout: &Layout, args: &ImportArgs) -> Result<Imported> {
    let (config, mut series) = layout.project()?;

    // Opening the mirror is local, and the URL its `origin` records decides which forge
    // a `--pr` number is read against, so the request is built after it. With no mirror
    // yet, the origin the first `ensure` sets is the one in the config.
    let existing = Mirror::open(layout).ok();
    let pr_url = args
        .repo
        .clone()
        .or_else(|| {
            existing
                .as_ref()
                .and_then(|mirror| mirror.remote_url().ok())
        })
        .or_else(|| config.upstream.url.clone());
    let request = request(args, &config, pr_url.as_deref())?;
    // What a failure says when a pull request number was read against a forge nothing
    // knows: only a number can be misread that way, and only an unknown domain ends up
    // assumed.
    let teach = request
        .pull
        .as_deref()
        .filter(|literal| literal.parse::<u64>().is_ok())
        .and_then(|_| pr_teach_note(&config, pr_url.as_deref(), layout));

    // Where the range is read from. `--repo` names a source of its own: a local tree,
    // opened and read in place, or a URL, whose store is filled on the same cache terms
    // the mirror gets. Without `--repo`, the mirror is the ground, created and filled
    // as every other command finds it.
    let source = args.repo.as_deref();
    let (ground, ends) = match source {
        Some(source) => {
            let ground = open_source(
                layout,
                &request,
                source,
                config.upstream.default_ref.as_deref(),
                teach.clone(),
            )?;
            let default = config.upstream.default_ref.clone();
            let (origin, hint) = match &ground {
                Ground::Tree(repo) => {
                    let url = repo
                        .find_remote("origin")
                        .ok()
                        .and_then(|remote| remote.url().map(str::to_owned).ok());
                    let hint = (!layout.refresh() && url.is_some()).then_some(TREE_UPSTREAM_HINT);
                    (url, hint)
                }
                Ground::Mirror(_) => (None, None),
            };
            match ends_in_tree(ground.repo(), &request, source, default.as_deref(), hint) {
                Ok(ends) => (ground, ends),
                Err(err) => {
                    // `--refresh` against a local tree is the permission to look
                    // further: the tree cannot answer, so the `origin` it records is
                    // asked, into the store this source keys.
                    if layout.refresh()
                        && let Some(url) = origin
                    {
                        info!(
                            target: "import",
                            "the tree does not carry what the range reads; \
                             checking its upstream at {url}"
                        );
                        let store =
                            open_store(layout, &request, source, &url, default.as_deref(), teach)?;
                        let ends = ends_in_tree(
                            store.repo(),
                            &request,
                            source,
                            default.as_deref(),
                            Some("This tree's upstream does not carry it either."),
                        )?;
                        (Ground::Mirror(store), ends)
                    } else {
                        return Err(err);
                    }
                }
            }
        }
        None => {
            // Before any network: a base that can be neither read from the config nor taken
            // from the mirror's own HEAD is known now, not after three minutes of fetches. A
            // mirror that does not exist yet is created below and answers the same question
            // from its own HEAD.
            if matches!(request.base, Base::MergeWithDefault)
                && config.upstream.default_ref.is_none()
                && let Some(mirror) = &existing
                && mirror.head_branch().is_none()
            {
                bail!("{}", base_sentence(&request.to, layout));
            }

            // No mirror yet: start one, like every other command does, rather than sending the
            // user away to run `skot sync` and come back. The fetch takes the range's first
            // fetchable ref; the blocks below fill in whatever else the range reads.
            let mirror = match existing {
                Some(mirror) => mirror,
                None => {
                    let needed = needed_refs(&request, config.upstream.default_ref.as_deref());
                    let wanted = needed
                        .iter()
                        .find(|n| !is_commit_id(n))
                        .map(String::as_str)
                        .or(config.upstream.default_ref.as_deref());
                    super::sync::ensure(layout, wanted)?;
                    Mirror::open(layout).context(format!(
                        "opening the mirror at {}",
                        layout.mirror_dir().display()
                    ))?
                }
            };

            // The base for a range that left it to be worked out: the setting when there is
            // one, else the branch the mirror itself points at, which is what a clone of this
            // project would call its main line and the answer a person gives when asked. Said
            // out loud, because it is a fallback and not a setting written anywhere.
            let default_ref: Option<String> = if matches!(request.base, Base::MergeWithDefault)
                && config.upstream.default_ref.is_none()
            {
                mirror.head_branch().map(|name| {
                    info!(
                        target: "import",
                        "upstream.default-ref is not set; using the mirror's `{name}` as the base"
                    );
                    name
                })
            } else {
                config.upstream.default_ref.clone()
            };

            // Fill the mirror for what this range actually reads, and only that. The mirror is
            // a cache: a range it can already walk is answered from it, with no connection
            // made; a gap (a ref it lacks, history a shallow fetch never took, a commit it
            // does not hold) is the only reason to go to the network, and `--refresh` is the
            // way to ask for freshness even when there is no gap. A pull request is the
            // exception: a moving pointer whose head means whatever the forge last published,
            // so it is always asked. A mirror with no `origin` has nothing to ask; `resolve`
            // gives the remedy for a ref that is still missing afterwards.
            let needed = needed_refs(&request, default_ref.as_deref());
            let fetchable: Vec<&str> = needed
                .iter()
                .map(String::as_str)
                .filter(|n| !is_commit_id(n))
                .collect();
            let by_id: Vec<&str> = needed
                .iter()
                .map(String::as_str)
                .filter(|n| is_commit_id(n))
                .collect();
            // A raw commit id names no ref: only the history that contains it can be fetched,
            // and which branch that is is not something an id says. A commit the mirror does
            // not hold therefore takes the whole upstream at full depth; an id already here is
            // no reason to ask for anything under it.
            let wildcard =
                !by_id.is_empty() && by_id.iter().any(|n| mirror.try_resolve(n).is_none());
            // The origin is asked once for everything this run reads from it: the range's own
            // refs, and the pull request head when there is one. One fetch, so an origin that
            // cannot be reached is an answer said once for the run instead of once per ref.
            let pull_ref = request
                .pull
                .is_some()
                .then(|| format!("+{}:{}", request.to, request.to));
            match mirror.remote_url() {
                Ok(url) => {
                    let mut refspecs: Vec<String> = if wildcard {
                        vec![
                            "+refs/heads/*:refs/heads/*".into(),
                            "+refs/tags/*:refs/tags/*".into(),
                        ]
                    } else {
                        fetchable
                            .iter()
                            .flat_map(|name| refspecs_for(name))
                            .collect()
                    };
                    let mut held = needed.iter().all(|n| mirror.try_resolve(n).is_some());
                    if let Some(spec) = &pull_ref {
                        refspecs.push(spec.clone());
                        held = held && mirror.try_resolve(&request.to).is_some();
                    }
                    // Held refs are not the question: history behind them a shallow fetch
                    // never took fails the walk just the same, so the walk itself decides.
                    // When it completes from what is here, the mirror answers and no
                    // connection is made; a pull request or `--refresh` asks anyway.
                    let walks = held && range_walks(&mirror, &request, default_ref.as_deref());
                    if !refspecs.is_empty()
                        && (layout.refresh() || request.pull.is_some() || !walks)
                    {
                        let extra = teach
                            .as_ref()
                            .map(|note| (request.to.as_str(), note.clone()));
                        fetch_or_fall_back(
                            &mirror,
                            &url,
                            "origin",
                            &refspecs,
                            "fetching what the range needs",
                            held,
                            "mirror",
                            format!("fetching the range's refs from {url}"),
                            extra,
                        )?;
                    } else if !refspecs.is_empty() {
                        debug!(
                            target: "import",
                            "the mirror already walks this range; not asking {url}"
                        );
                    }
                }
                // No `origin` and a pull request means nowhere to fetch it from. A range
                // alone keeps reading the mirror as it stands, which is what it has always
                // done.
                Err(err) if pull_ref.is_some() => {
                    return Err(err).context(format!(
                    "the mirror records no `origin`, so there is nowhere to fetch the pull request \
                 from.\nRun `{}` first.",
                    skot("sync")
                ));
                }
                Err(_) => {}
            }

            // After every fetch has had its chance: a ref the upstream does not have is said so
            // here, with the fetch already part of the story, rather than surfacing later as
            // advice to go run the sync that just happened.
            let mut check: Vec<&str> = needed.iter().map(String::as_str).collect();
            if request.pull.is_some() {
                check.push(&request.to);
            }
            if let Some(missing) = check
                .into_iter()
                .find(|n| !is_commit_id(n) && mirror.try_resolve(n).is_none())
            {
                let mut message = format!(
                    "`{missing}` is not in the mirror, and the upstream does not have it either.\n\
             Check the spelling; a ref published only just may need a moment."
                );
                if let Some(note) = &teach
                    && missing == request.to
                {
                    message.push('\n');
                    message.push_str(note);
                }
                bail!("{message}");
            }

            let ends = resolve(&mirror, layout, &request, default_ref.as_deref())?;
            (Ground::Mirror(mirror), ends)
        }
    };
    let repo = ground.repo();

    // How the source is named in the patch's identity, and what origin is worth saying
    // out loud: a network origin the source answers to when it has one, a digest of
    // where it lies when it does not, and never the path itself. Patches get committed,
    // and a path there says where the files on this machine sat.
    let origin = match &ground {
        Ground::Tree(repo) => repo
            .find_remote("origin")
            .ok()
            .and_then(|remote| remote.url().map(str::to_owned).ok()),
        Ground::Mirror(mirror) => mirror.remote_url().ok(),
    };
    let source_url = origin
        .as_deref()
        .filter(|url| !is_local_url(url))
        .map(str::to_owned);
    let token = match source {
        Some(raw) => Some(source_token(raw, origin.as_deref())?),
        None => None,
    };
    let identity = identity_of(&request, token.as_deref(), args.reverse);

    // The lock authoring holds now covers import's local work too: deciding whether this
    // change already exists, choosing the next free prefix, and writing. The fetch stays
    // outside it (network is not what two imports racing over the series need
    // serialising), but nothing below runs without it.
    let lock = layout.lock()?;
    let from_tree = git::commit_tree(repo, &ends.from_sha)
        .context(format!("reading the tree of {}", ends.from))?;
    let to_tree =
        git::commit_tree(repo, &ends.to_sha).context(format!("reading the tree of {}", ends.to))?;
    if from_tree.id() == to_tree.id() {
        bail!(
            "`{}` and `{}` are the same tree, so there is nothing to import.",
            ends.from,
            ends.to
        );
    }
    // `--reverse` extracts the same range the other way round: the patch's diff runs
    // from `to` back to `from`, undoing what the range did. The provenance line and the
    // duplicate-scan identity record the direction, so the forward and the reversed
    // patch of one commit are two different things, and a second run of the same flag
    // answers to the first.
    let (old, new) = if args.reverse {
        (to_tree, from_tree)
    } else {
        (from_tree, to_tree)
    };
    let (line_from, line_to) = if args.reverse {
        (ends.to_sha.clone(), ends.from_sha.clone())
    } else {
        (ends.from_sha.clone(), ends.to_sha.clone())
    };
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

    // What to call it when the imported work has no single commit to speak for it: the
    // request as the user made it, not what it resolved to. `--pr 125` becomes
    // `Import PR 125`; a range stays the range as written.
    let fallback = match request.pull.as_deref() {
        Some(literal) if literal.parse::<u64>().is_ok() => format!("Import PR {literal}"),
        Some(literal) => format!("Import {literal}"),
        None => format!("Import {}", request_expression(&request)),
    };
    let derived = if args.reverse {
        // A reversed single commit is named for what it does: undo that change. The
        // original subject alone would read as the forward patch in `list`.
        match tip_subject.as_deref().filter(|s| !s.is_empty()) {
            Some(subject) if commits == 1 => format!("Revert \"{subject}\""),
            _ => format!("Revert {}", request_expression(&request)),
        }
    } else {
        tip_subject
            .filter(|s| !s.is_empty() && commits == 1)
            .unwrap_or(fallback)
    };
    let slug = match &args.name {
        Some(name) => super::new::slug(name)?,
        None => filename_slug(&derived),
    };

    // A name the user gave is the subject the same way it is for `new`: read back from the
    // filename, so `skot patch list` and the patch header say the same words.
    let subject = if args.name.is_some() {
        authoring::subject_from_name(&series::default_patch_name(&slug))
    } else {
        derived
    };
    let own = authoring::signature(repo);
    let signature = match tip.as_ref() {
        Some(commit) if commits == 1 => commit.author(),
        _ => own,
    };

    // Two renders: the provenance line records a fingerprint of the change, and the
    // fingerprint is over the body — so hash the render as it stands, then stamp the line
    // naming it and render once more for real.
    let mut header = authoring::header_for(None, &subject, &signature);
    if commits == 1 {
        header.body = tip_body.clone();
    }
    let bare =
        diff::tree_to_tree_patch(repo, &old, &new, &header).context("rendering the import")?;
    let change = change_id(&bare)?;
    header
        .extra
        .push(provenance_line(&identity, &line_from, &line_to, &change));
    if let Some(url) = &source_url {
        header.extra.push(format!("X-Skot-Source: {url}"));
    }
    let text =
        diff::tree_to_tree_patch(repo, &old, &new, &header).context("rendering the import")?;
    let summary = diff::summarise(&text);

    // Does any existing patch already answer to this import? One pass over the directory:
    // an identical diff under any name at all, and a patch recorded for this same request.
    let scan = scan_existing(layout, patch_body(&text), &identity)?;

    // The same change, already in the series — under a different name, with or without
    // provenance, registered or not. This is what makes a second run of the same command
    // a report rather than a second copy.
    if let Some(found) = scan.identical {
        let (series_line, registered_now) =
            register_existing(layout, &mut series, &found, args.no_register)?;
        return Ok(Imported {
            patch: found.clone(),
            path: layout.patches_dir().join(&found),
            series_line,
            from: ends.from.clone(),
            from_sha: ends.from_sha.clone(),
            to: ends.to.clone(),
            to_sha: ends.to_sha.clone(),
            reverse: args.reverse,
            commits,
            summary,
            outcome: Outcome::Unchanged,
            registered_now,
            previous_commits: None,
        });
    }

    // The same request with different ends: the pull request gained commits, or the base
    // moved under it. The patch is rewritten where it is — same name, same line — unless
    // you have edited it since, which is refused rather than overwritten.
    if let Some((name, provenance)) = scan.provenance {
        if provenance.from == line_from && provenance.to == line_to {
            // Same ends and yet not byte-identical — `scan` just proved that — which
            // means the file has been edited. Nothing new to import, nothing of yours
            // to touch.
            let (series_line, registered_now) =
                register_existing(layout, &mut series, &name, args.no_register)?;
            return Ok(Imported {
                patch: name.clone(),
                path: layout.patches_dir().join(&name),
                series_line,
                from: ends.from.clone(),
                from_sha: ends.from_sha.clone(),
                to: ends.to.clone(),
                to_sha: ends.to_sha.clone(),
                reverse: args.reverse,
                commits,
                summary,
                outcome: Outcome::Unchanged,
                registered_now,
                previous_commits: None,
            });
        }

        let path = layout.patches_dir().join(&name);
        let existing =
            std::fs::read_to_string(&path).context(format!("reading {}", path.display()))?;
        if change_id(&existing)? != provenance.change {
            bail!(
                "{} records this import of {}, but you have edited it since.\n\
                 The new version would overwrite your edits, so it was not written.\n\
                 Delete {} to take the new version, or keep your edits and drop the import.",
                name,
                describe_identity(&identity),
                name
            );
        }

        // The existing header speaks for the patch — its subject, its author, the words in
        // its filename — so an update takes none of that from the request: only the ends
        // and the provenance change. `--name` names creations, not updates.
        let mut header = authoring::header_for(Some(&existing), &subject, &signature);
        header.extra.retain(|line| {
            !line.starts_with("X-Skot-Import: ") && !line.starts_with("X-Skot-Source: ")
        });
        header
            .extra
            .push(provenance_line(&identity, &line_from, &line_to, &change));
        if let Some(url) = &source_url {
            header.extra.push(format!("X-Skot-Source: {url}"));
        }
        if commits == 1 {
            header.body = tip_body;
        } else {
            header.body.clear();
        }
        let text = diff::tree_to_tree_patch(repo, &old, &new, &header)
            .context(format!("re-rendering {}", name))?;
        let summary = diff::summarise(&text);
        std::fs::write(&path, &text).context(format!("writing {}", path.display()))?;
        let (series_line, registered_now) =
            register_existing(layout, &mut series, &name, args.no_register)?;
        // How many commits the range held before: the provenance records the ends in
        // the patch's own direction, which for a reversed patch is the range reversed.
        let previous_commits = if args.reverse {
            count_between(repo, &provenance.from, &provenance.to).ok()
        } else {
            count_between(repo, &provenance.to, &provenance.from).ok()
        };
        return Ok(Imported {
            patch: name,
            path,
            series_line,
            from: ends.from,
            from_sha: ends.from_sha,
            to: ends.to,
            to_sha: ends.to_sha,
            reverse: args.reverse,
            commits,
            summary,
            outcome: Outcome::Updated,
            registered_now,
            previous_commits,
        });
    }

    // Nothing has this change: a new patch at the position asked for (the end, unless
    // `--at` says otherwise). Chosen here rather than earlier because a run that turns
    // out to be a no-op never names a file.
    let index = series::position_at(&series, args.at.as_deref())?;
    let patch_name = series::default_patch_name(&slug);
    let path = layout.patches_dir().join(&patch_name);
    if path.exists() {
        bail!(
            "{} already exists.\n\
             Choose another name with `--name`, or remove it if it is stale.",
            path.display()
        );
    }
    std::fs::create_dir_all(layout.patches_dir()).context("creating the patch directory")?;
    let series_line = if args.no_register {
        std::fs::write(&path, &text).context(format!("writing {}", path.display()))?;
        None
    } else {
        // The line goes in, the series is saved to match, and only then is the file
        // written: a failure anywhere before the write leaves at worst a listed patch with
        // no file, which `skot patch list` calls out. Nothing else in the series moves, so
        // there is no record to carry behind it.
        let line = series.insert(&patch_name, index)?;
        series.save(layout)?;
        std::fs::write(&path, &text).context(format!("writing {}", path.display()))?;
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
        reverse: args.reverse,
        commits,
        summary,
        outcome: Outcome::Created,
        registered_now: false,
        previous_commits: None,
    })
}

/// Read the command line as a [`Request`], rejecting every shape that cannot become one.
///
/// `pr_url` is the repository a `--pr` number is read against: the `--repo` source when
/// there is one, else the mirror's origin. The domain of that URL picks the forge's
/// namespace; a full ref as written needs no URL at all.
fn request(
    args: &ImportArgs,
    config: &crate::config::Config,
    pr_url: Option<&str>,
) -> Result<Request> {
    if let Some(literal) = &args.pr {
        let base = match &args.from {
            Some(from) => Base::Named(from.clone()),
            None => Base::MergeWithDefault,
        };
        // A number is the forge's own ref for that request; a full ref is used as
        // written, which is how a forge the tables do not name is asked (Gerrit's
        // `refs/changes/…`, a private namespace).
        let (to, pull) = match literal.parse::<u64>() {
            Ok(number) => (pr_ref_for(pr_url, config, number), number.to_string()),
            Err(_) if literal.starts_with("refs/") => (literal.clone(), literal.clone()),
            Err(_) => bail!(
                "`--pr {literal}` is neither a pull request number nor a ref name.\n\
                 Give a number (`--pr 42`) or the full ref (`--pr refs/changes/12/345/1`)."
            ),
        };
        return Ok(Request {
            to,
            base,
            pull: Some(pull),
        });
    }

    let Some(range) = args.range.as_deref() else {
        bail!(
            "nothing to import.\n\
             Give a range (`{}`), a ref (`{}`), \
             or a pull request (`{}`).",
            skot_args("import", "main..my-branch"),
            skot_args("import", "my-branch"),
            skot_args("import", "--pr 42")
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

/// The namespace a pull request number is read against when nothing names one: the
/// pattern GitHub publishes under, which most forges copied.
const PR_TEMPLATE_DEFAULT: &str = "refs/pull/{n}/head";

/// Where a template came from, so a failure can say whether the table needs a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrTemplateSource {
    /// The `[pr-refs]` table in `skot.toml`.
    Config,
    /// One of the forges built into skot.
    Builtin,
    /// Nothing matched; the default was assumed.
    Assumed,
}

/// The ref namespaces of the forges skot knows by name.
fn builtin_pr_template(domain: &str) -> Option<&'static str> {
    match domain {
        "github.com" | "codeberg.org" | "gitea.com" => Some("refs/pull/{n}/head"),
        "gitlab.com" => Some("refs/merge-requests/{n}/head"),
        "bitbucket.org" => Some("refs/pull-requests/{n}/from"),
        _ => None,
    }
}

/// The host a fetch URL speaks to, or `None` for a location with none: a local path,
/// a file URL, a remote helper, or anything git's own grammar rejects, which reads as
/// "no host" rather than a failure, so the caller assumes the default namespace and
/// no valid remote can choke here.
///
/// The grammar is git's, so the parsing is [`gix_url`]: scheme URLs with ports and
/// bracketed IPv6, scp-like `host:path` with or without `user@`, local paths, remote
/// helpers. Hosts come back in lookup form (DNS-like ASCII already lowercased); the
/// last lowercasing keeps IPv6 hex and typed-in hosts on the same key either way.
fn url_domain(url: &str) -> Option<String> {
    let parsed = gix_url::parse(url).ok()?;
    if parsed.scheme == gix_url::Scheme::File {
        return None;
    }
    let host = parsed.host?.to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

/// The ref template for the forge behind `url`: the `[pr-refs]` table first (an exact
/// host, then wildcards, the longest suffix winning), then the forges skot knows, then
/// the default. Which URL is consulted is the caller's: `--repo` when given, else the
/// origin, so the namespace and the fetch can never disagree.
fn pr_template<'a>(
    url: Option<&str>,
    config: &'a crate::config::Config,
) -> (Option<String>, &'a str, PrTemplateSource) {
    let Some(domain) = url.and_then(url_domain) else {
        return (None, PR_TEMPLATE_DEFAULT, PrTemplateSource::Assumed);
    };
    if let Some(template) = config.pr_refs.get(&domain) {
        return (Some(domain), template, PrTemplateSource::Config);
    }
    let wildcard = config
        .pr_refs
        .iter()
        .filter_map(|(key, template)| {
            let base = key.strip_prefix("*.")?;
            // Subdomains only: `*.corp.example` covers `git.corp.example`, never
            // `corp.example` itself.
            let matched = domain
                .strip_suffix(base)
                .is_some_and(|sub| sub.ends_with('.'));
            matched.then_some((base.len(), template))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, template)| template);
    if let Some(template) = wildcard {
        return (Some(domain), template, PrTemplateSource::Config);
    }
    if let Some(template) = builtin_pr_template(&domain) {
        return (Some(domain), template, PrTemplateSource::Builtin);
    }
    (Some(domain), PR_TEMPLATE_DEFAULT, PrTemplateSource::Assumed)
}

/// `--pr N` as the ref the forge publishes it under.
fn pr_ref_for(url: Option<&str>, config: &crate::config::Config, number: u64) -> String {
    let (_, template, _) = pr_template(url, config);
    template.replace("{n}", &number.to_string())
}

/// What a failed pull request import says when its number was read against a forge
/// nothing knows: which host, which namespace was assumed, and the two ways to fix it.
fn pr_teach_note(
    config: &crate::config::Config,
    url: Option<&str>,
    layout: &Layout,
) -> Option<String> {
    let (domain, template, source) = pr_template(url, config);
    if source != PrTemplateSource::Assumed {
        return None;
    }
    let domain = domain?;
    Some(format!(
        "`{domain}` was matched to no known forge, so `{template}` was assumed.\n\
         If it is a different forge, add its namespace to {}:\n  \
         [pr-refs]\n  \"{domain}\" = \"{template}\"\n\
         or pass the full ref: `{}`.",
        layout.config_file().display(),
        skot_args("import", "--pr refs/...")
    ))
}

/// What the range reads out of a repository: the `to` end, whatever the base names, and
/// the default ref only when the base *is* the default, the one case a caller that
/// resolves against a default reads it in. `default` is that ref, or `None` when the
/// base needs no default: a bare coordinate from a `--repo` source takes its parent.
///
/// A pull request's own ref is excluded: its fetch brings it.
fn needed_refs(request: &Request, default: Option<&str>) -> Vec<String> {
    let mut names = Vec::new();
    if request.pull.is_none() {
        names.push(request.to.clone());
    }
    match &request.base {
        Base::Named(name) => names.push(name.clone()),
        Base::MergeBaseOf(a, b) => names.extend([a.clone(), b.clone()]),
        Base::MergeWithDefault => {
            if let Some(default) = default {
                names.push(default.to_owned());
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// How one name is fetched by name: a short name as both a branch and a tag (libgit2
/// takes whichever the remote has), a full ref as written. A commit id names no ref and
/// produces no refspec.
fn refspecs_for(name: &str) -> Vec<String> {
    if is_commit_id(name) {
        return Vec::new();
    }
    if name.starts_with("refs/") {
        return vec![format!("+{name}:{name}")];
    }
    vec![
        format!("+refs/heads/{name}:refs/heads/{name}"),
        format!("+refs/tags/{name}:refs/tags/{name}"),
    ]
}

/// One of import's fetches, called when the walk probe found a gap, `--refresh` asked,
/// or a pull request (a moving pointer, always asked). When the upstream cannot be
/// reached but a mirror that already holds every destination (`held`) is enough to
/// import from, that is said out loud, and when it does not, the failure is the answer.
///
/// `extra` is a note to append when the failure lands on `extra.0`'s needle: the
/// diagnosis of a ref the forge does not publish, or of a run not allowed to ask.
/// `place` names what is being kept when that happens: the mirror, or a `--repo` URL's
/// store.
#[allow(clippy::too_many_arguments)]
fn fetch_or_fall_back(
    mirror: &Mirror,
    url: &str,
    remote_name: &str,
    refspecs: &[String],
    message: &str,
    held: bool,
    place: &str,
    context: String,
    extra: Option<(&str, String)>,
) -> Result<()> {
    match mirror.fetch_refspecs(url, remote_name, refspecs, message, i32::MAX) {
        Ok(()) => Ok(()),
        Err(fetch_err) if held => {
            // Two facts, two lines: the upstream is unreachable, and what skot does
            // about it. The fetch's own diagnosis names the failed fetch, so the
            // root cause is quoted rather than that wrapper.
            warn!(
                target: "import",
                "the upstream at {url} could not be reached: {}",
                fetch_err.root_cause()
            );
            warn!(target: "import", "using the local {place} as it stands");
            Ok(())
        }
        Err(fetch_err) => {
            let mut err = fetch_err.context(context);
            if let Some((needle, note)) = extra {
                let names_the_ref = err.root_cause().to_string().contains(needle);
                if names_the_ref || mirror.offline() {
                    err = err.context(note);
                }
            }
            Err(err)
        }
    }
}

/// What a range with no base says when there is nothing to take one from: the flag,
/// or the setting, before any network is spent proving it.
fn base_sentence(to: &str, layout: &Layout) -> String {
    format!(
        "cannot work out what `{to}` is based on.\n\
         Pass a base (`{}`), or set `upstream.default-ref` in {}.",
        skot_args("import", &format!("--from <ref> {to}")),
        layout.config_file().display()
    )
}

/// Whether the history behind the range is already here: every end resolves, and the
/// walk the range reads (a merge base, or the commits between the ends) completes from
/// what is on disk. Purely local, so a miss costs a revwalk; the fetch this decides
/// whether to skip fills exactly the gap the walk found.
fn range_walks(mirror: &Mirror, request: &Request, default: Option<&str>) -> bool {
    let resolve = |name: &str| mirror.try_resolve(name);
    match &request.base {
        Base::Named(from) => match (resolve(&request.to), resolve(from)) {
            (Some(to), Some(from)) => count_between(mirror.repo(), &to, &from).is_ok(),
            _ => false,
        },
        Base::MergeBaseOf(a, b) => match (resolve(a), resolve(b)) {
            (Some(a), Some(b)) => merge_base(mirror.repo(), &a, &b).is_ok(),
            _ => false,
        },
        Base::MergeWithDefault => {
            let Some(default) = default else {
                return false;
            };
            match (resolve(default), resolve(&request.to)) {
                (Some(default), Some(to)) => merge_base(mirror.repo(), &default, &to).is_ok(),
                _ => false,
            }
        }
    }
}

/// The two ends as commit ids, after any fetch has put them in the mirror.
fn resolve(
    mirror: &Mirror,
    layout: &Layout,
    request: &Request,
    default_ref: Option<&str>,
) -> Result<Ends> {
    let to_sha = mirror.resolve(&request.to)?;

    let (from, from_sha) = match &request.base {
        Base::Named(name) => (name.clone(), mirror.resolve(name)?),
        Base::MergeBaseOf(a, b) => {
            let sha_a = mirror.resolve(a)?;
            let sha_b = mirror.resolve(b)?;
            let sha = merge_base(mirror.repo(), &sha_a, &sha_b)
                .context(format!("`{a}` and `{b}` share no history"))?;
            (format!("merge-base({a}, {b})"), sha)
        }
        Base::MergeWithDefault => {
            let default = default_ref
                .context(base_sentence(&request.to, layout))?
                .to_owned();
            let default_sha = mirror.resolve(&default).with_context(|| {
                format!("cannot resolve `upstream.default-ref` (`{default}`) in the mirror")
            })?;
            let sha = merge_base(mirror.repo(), &default_sha, &to_sha).context(format!(
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

/// The common ancestor of two commits in one repository.
fn merge_base(repo: &git2::Repository, a: &Sha, b: &Sha) -> Result<Sha> {
    let a = git2::Oid::from_str(a).context(format!("{a} is not a commit id"))?;
    let b = git2::Oid::from_str(b).context(format!("{b} is not a commit id"))?;
    repo.merge_base(a, b)
        .map(|oid| oid.to_string())
        .context("the two refs share no history")
}

/// The ground for a `--repo` source: a local tree read in place, or a URL's store.
///
/// What decides is what can be *opened*: a path that exists is a tree, wherever its
/// repository root sits, and skot reads it directly the way `format-patch` reads a
/// working tree. A URL cannot be opened, so the range's refs are fetched into the store
/// keyed by that location and read from there. Anything else is neither, and says so
/// before anything is fetched.
fn open_source(
    layout: &Layout,
    request: &Request,
    source: &str,
    default_ref: Option<&str>,
    teach: Option<String>,
) -> Result<Ground> {
    if Path::new(source).exists() {
        let repo = git2::Repository::discover(source)
            .context(format!("`{source}` is not inside a git repository"))?;
        return Ok(Ground::Tree(repo));
    }
    if looks_like_url(source) {
        let store = open_store(layout, request, source, source, default_ref, teach)?;
        return Ok(Ground::Mirror(store));
    }
    bail!("`{source}` is not a git repository or a URL.")
}

/// Whether a location that does not exist on disk names a remote: a scheme, a user, or
/// a host with a dot in it. Anything else typed in that is not a path is refused as one
/// rather than fetched from.
fn looks_like_url(source: &str) -> bool {
    source.contains("://")
        || source.contains('@')
        || url_domain(source).is_some_and(|host| host.contains('.'))
}

/// A `--repo` URL's store: the range's refs fetched from `url` into
/// `.skot/sources/<key>`, on the same cache terms the mirror gets. The store is keyed
/// by the location the user gave, so the same source reuses one store and two different
/// sources can never read each other's history.
///
/// The store is filled with what a ref can carry: a commit id names no ref, so an id the
/// store lacks takes every ref at full depth, which is what brings the history a ref
/// points at. A commit no ref points at cannot cross a fetch at all; a local tree holds
/// it by lying on disk, which is what `--repo <path>` reads.
fn open_store(
    layout: &Layout,
    request: &Request,
    source: &str,
    url: &str,
    default_ref: Option<&str>,
    teach: Option<String>,
) -> Result<Mirror> {
    let path = layout.source_store(&source_key(source)?);
    let existing = Mirror::at(path.clone(), layout.offline()).ok();
    // A pull request's head is its own ref, fetched alongside; its base may read the
    // default ref from the store too. Anything else needs only what the range names, so
    // a bare commit asks for one thing.
    let fetch_default = request.pull.is_some().then_some(default_ref).flatten();
    let needed = needed_refs(request, fetch_default);
    let walks = existing.as_ref().is_some_and(|mirror| {
        ends_in_tree(mirror.repo(), request, source, default_ref, None).is_ok()
    });
    if layout.refresh() || request.pull.is_some() || !walks {
        let mut refspecs = store_refspecs(existing.as_ref().map(Mirror::repo), &needed);
        if request.pull.is_some() {
            refspecs.push(format!("+{}:{}", request.to, request.to));
        }
        if !refspecs.is_empty() {
            let message = "fetching the source repository";
            let context = format!("fetching what the range needs from {url}");
            let extra = teach.map(|note| (request.to.as_str(), note));
            match &existing {
                Some(mirror) => fetch_or_fall_back(
                    mirror, url, "origin", &refspecs, message, walks, "store", context, extra,
                )?,
                None => {
                    if let Err(err) =
                        Mirror::fetch_at(url, &path, &refspecs, message, layout.offline())
                    {
                        let mut err = err.context(context);
                        if let Some((needle, note)) = extra {
                            let names_the_ref = err.root_cause().to_string().contains(needle);
                            if names_the_ref || layout.offline() {
                                err = err.context(note);
                            }
                        }
                        return Err(err);
                    }
                }
            }
        }
    } else {
        debug!(
            target: "import",
            "the store already walks this range; not asking {url}"
        );
    }
    Mirror::at(path, layout.offline())
}

/// What a store asks of its source: the range's own refs by name, or, when a bare
/// commit id the store does not hold needs history behind it, every ref at full depth
/// (the only way an id can arrive, since only a ref can carry it across a fetch).
fn store_refspecs(repo: Option<&git2::Repository>, needed: &[String]) -> Vec<String> {
    let missing_id = needed
        .iter()
        .filter(|name| is_commit_id(name))
        .any(|id| repo.is_none_or(|repo| git::try_commit(repo, id).is_none()));
    if missing_id {
        return vec![
            "+refs/heads/*:refs/heads/*".into(),
            "+refs/tags/*:refs/tags/*".into(),
        ];
    }
    needed
        .iter()
        .filter(|name| !is_commit_id(name))
        .flat_map(|name| refspecs_for(name))
        .collect()
}

/// The store key for a source location: a digest of where it is, canonicalised when it
/// is a path, so the same location reuses one store however it was spelled.
fn source_key(source: &str) -> Result<String> {
    let canonical = std::fs::canonicalize(source)
        .ok()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| source.to_owned());
    let digest = git2::Oid::hash_object(git2::ObjectType::Blob, canonical.as_bytes())
        .context("keying the source location")?
        .to_string();
    Ok(digest.chars().take(16).collect())
}

/// How a `--repo` source is named in a patch's identity: the network origin it answers
/// to when it has one, a digest of where it lies when it does not. Never the path
/// itself: patches get committed, and a path there says where the files on this machine
/// sat. A local origin is worth no more than the path beside it, so it digests too.
fn source_token(source: &str, origin: Option<&str>) -> Result<String> {
    match origin {
        Some(url) if !is_local_url(url) => Ok(url.to_owned()),
        _ => source_key(source),
    }
}

/// The two ends of the request, read in the repository the source provides.
///
/// Every name resolves against that repository alone: its refs, its commits, even a
/// commit expression, and a commit id that exists there as an object (on a branch, on
/// no branch at all, it does not matter; the object is what is read). The base is that
/// repository's business and never the mirror's: an explicit range as written, and for
/// a bare coordinate the parent of the commit, which is the change the commit itself
/// introduced, the way `git format-patch -1` reads it. A pull request is the one
/// coordinate that means everything it brings: its head against the source's own
/// default line when that exists there, the head's parent when it does not.
fn ends_in_tree(
    repo: &git2::Repository,
    request: &Request,
    source: &str,
    default_ref: Option<&str>,
    miss_hint: Option<&str>,
) -> Result<Ends> {
    let to = resolve_in(repo, &request.to, source, miss_hint)?;
    let (from, from_sha) = match &request.base {
        Base::Named(name) => (name.clone(), resolve_in(repo, name, source, miss_hint)?),
        Base::MergeBaseOf(a, b) => {
            let sha_a = resolve_in(repo, a, source, miss_hint)?;
            let sha_b = resolve_in(repo, b, source, miss_hint)?;
            let sha = merge_base(repo, &sha_a, &sha_b)
                .context(format!("`{a}` and `{b}` share no history"))?;
            (format!("merge-base({a}, {b})"), sha)
        }
        Base::MergeWithDefault => {
            let shared = request
                .pull
                .is_some()
                .then(|| default_ref.and_then(|name| git::try_commit(repo, name)))
                .flatten()
                .and_then(|default_sha| merge_base(repo, &default_sha, &to).ok());
            match (shared, default_ref) {
                (Some(sha), Some(name)) => (format!("merge-base({name}, {})", request.to), sha),
                _ => parent_of(repo, &request.to, &to)?,
            }
        }
    };

    Ok(Ends {
        from,
        from_sha,
        to: request.to.clone(),
        to_sha: to,
    })
}

/// One coordinate against the source: a commit id looked up as an object, anything
/// else read the way git reads a revision in that repository.
///
/// The mirror is not consulted. A name the source does not have is a fact about the
/// source, and `--repo` said that is the tree that matters.
fn resolve_in(
    repo: &git2::Repository,
    name: &str,
    source: &str,
    miss_hint: Option<&str>,
) -> Result<Sha> {
    if is_commit_id(name) {
        let oid = git2::Oid::from_str(name).context(format!("`{name}` is not a commit id"))?;
        if repo.find_object(oid, None).is_err() {
            let mut message = format!(
                "the commit `{name}` is not in {source}.\n\
                 `--repo` names that tree, and it holds no such object: check the \
                 sha, or point `--repo` at the tree that holds it."
            );
            if let Some(hint) = miss_hint {
                message.push('\n');
                message.push_str(hint);
            }
            bail!("{message}");
        }
        return git::try_commit(repo, name)
            .context(format!("`{name}` is in {source}, but is not a commit"));
    }
    git::try_commit(repo, name).with_context(|| {
        let mut message = format!("`{name}` is neither a branch, tag, nor commit in {source}.");
        if let Some(hint) = miss_hint {
            message.push('\n');
            message.push_str(hint);
        }
        message
    })
}

/// The base a bare coordinate takes in its own repository: the commit's parent, the
/// change the commit itself introduced. A root commit has no parent and no history it
/// came from, so there is nothing to diff it against.
fn parent_of(repo: &git2::Repository, coord: &str, sha: &Sha) -> Result<(String, Sha)> {
    let oid = git2::Oid::from_str(sha).context(format!("{sha} is not a commit id"))?;
    let commit = repo
        .find_commit(oid)
        .context(format!("reading `{coord}`"))?;
    match commit.parent_id(0) {
        Ok(parent) => Ok((format!("{coord}^"), parent.to_string())),
        Err(_) => bail!(
            "`{coord}` is the root commit of the repository, so it has no parent to \
             diff it against.\nGive a base explicitly (`{}`), or import a commit that \
             has one.",
            skot_args("import", &format!("--from <ref> {coord}"))
        ),
    }
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

/// The request as the user wrote it, for a name or a message: `main..feat`,
/// `main...feat`, or just `feat` when the base was left to be worked out.
fn request_expression(request: &Request) -> String {
    match &request.base {
        Base::Named(from) => format!("{from}..{}", request.to),
        Base::MergeBaseOf(a, b) => format!("{a}...{b}"),
        Base::MergeWithDefault => request.to.clone(),
    }
}

/// What makes a later run the same request: how the user asked for it, not what it
/// resolved to.
///
/// The ends are recorded separately from this, so a pull request that gained commits or
/// a base that moved still matches — only a different way of asking (another source
/// repository, another base flag, the other direction) is a different import: the
/// forward and the reversed patch of one commit are two different things in the series.
fn identity_of(request: &Request, source: Option<&str>, reverse: bool) -> String {
    let kind = if reverse { "reversed:" } else { "" };
    if let Some(literal) = &request.pull {
        return format!("{kind}pr:{literal}");
    }
    let expression = request_expression(request);
    match source {
        Some(url) => format!("{kind}repo:{url}#{expression}"),
        None => format!("{kind}range:{expression}"),
    }
}

/// The identity as a person would say it, for an error message.
fn describe_identity(identity: &str) -> String {
    if let Some(inner) = identity.strip_prefix("reversed:") {
        return format!("the reverse of {}", describe_identity(inner));
    }
    match identity.strip_prefix("pr:") {
        Some(number) => format!("pull request {number}"),
        None => identity.to_owned(),
    }
}

/// A patch's provenance: the request it answers to, the two ends it diffed, and a
/// fingerprint of the change it carried at the time.
struct Provenance {
    identity: String,
    from: Sha,
    to: Sha,
    change: String,
}

/// The mail header line carrying it.
///
/// Read back from the right — the last three fields are the two ends and the
/// fingerprint — so an identity containing spaces (`repo:../fork main..feat`) parses as
/// one field rather than several.
fn provenance_line(identity: &str, from: &str, to: &str, change: &str) -> String {
    format!("X-Skot-Import: {identity} {from} {to} {change}")
}

/// What an `X-Skot-Import` line says, or `None` for a line that is not one, or is
/// malformed. A malformed line is skipped rather than guessed at: the worst case is the
/// next import being treated as new, which is the duplication, not a wrong update.
fn parse_provenance(line: &str) -> Option<Provenance> {
    let rest = line.strip_prefix("X-Skot-Import: ")?;
    let mut fields = rest.rsplitn(4, ' ');
    let change = fields.next()?.to_owned();
    let to = fields.next()?.to_owned();
    let from = fields.next()?.to_owned();
    let identity = fields.next()?.to_owned();
    if identity.is_empty() || from.is_empty() || to.is_empty() || change.is_empty() {
        return None;
    }
    Some(Provenance {
        identity,
        from,
        to,
        change,
    })
}

/// What the existing patches say about the import being attempted.
struct Scan {
    /// A patch whose diff is byte-for-byte this one, under any name at all.
    identical: Option<String>,
    /// A patch recorded as an import of this same request.
    provenance: Option<(String, Provenance)>,
}

/// Look at every patch once and answer both questions.
///
/// A file that cannot be read or named is skipped: it cannot be shown to be this change,
/// and refusing to import because one patch in the directory is unreadable would be
/// worse than the duplication this exists to prevent.
fn scan_existing(layout: &Layout, ours: &str, identity: &str) -> Result<Scan> {
    let mut scan = Scan {
        identical: None,
        provenance: None,
    };
    let Ok(entries) = std::fs::read_dir(layout.patches_dir()) else {
        return Ok(scan);
    };
    for entry in entries {
        let entry = entry.context("reading the patch directory")?;
        let name = entry.file_name().into_string().unwrap_or_default();
        if !name.ends_with(".patch") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        if scan.identical.is_none() && patch_body(&text) == ours {
            scan.identical = Some(name.clone());
        }
        if scan.provenance.is_none() {
            let found = diff::Header::parse(&text)
                .and_then(|header| {
                    header
                        .extra
                        .iter()
                        .find_map(|line| parse_provenance(line))
                        .filter(|p| p.identity == identity)
                })
                .map(|provenance| (name, provenance));
            scan.provenance = found;
        }
        if scan.identical.is_some() && scan.provenance.is_some() {
            break;
        }
    }
    Ok(scan)
}

/// The diff itself: everything from the first `diff --git` on, headers excluded.
fn patch_body(text: &str) -> &str {
    match text.find("diff --git ") {
        Some(at) => &text[at..],
        None => "",
    }
}

/// A fingerprint of what a patch changes: git's own blob id of its body.
///
/// Recorded so a later update can tell a patch nobody has touched from one you have
/// edited since. git's hash, not ours: stable across skot upgrades, and over the body —
/// not the header — because that is what an edit changes.
fn change_id(patch: &str) -> Result<String> {
    let oid = git2::Oid::hash_object(git2::ObjectType::Blob, patch_body(patch).as_bytes())
        .context("hashing the patch body")?;
    Ok(oid.to_string())
}

/// Where an already-written patch sits in the series, adding it when it is not there.
///
/// A no-op that finds the file unregistered — imported once with `--no-register`, or
/// dropped from the series by hand — should fix that rather than write a second copy.
fn register_existing(
    layout: &Layout,
    series: &mut series::Series,
    name: &str,
    no_register: bool,
) -> Result<(Option<usize>, bool)> {
    if let Some(entry) = series
        .entries(layout)?
        .into_iter()
        .find(|entry| entry.name == name)
    {
        return Ok((Some(entry.line), false));
    }
    if no_register {
        return Ok((None, false));
    }
    let line = series.insert(name, series.len())?;
    series.save(layout)?;
    Ok((Some(line), true))
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
            at: None,
            range: range.map(str::to_owned),
            from: None,
            pr: None,
            repo: None,
            name: None,
            no_register: false,
            reverse: false,
        }
    }

    fn reversed(range: Option<&str>) -> ImportArgs {
        ImportArgs {
            reverse: true,
            ..args(range)
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
        assert!(
            text.contains("X-Skot-Import: range:main..feat "),
            "the patch must record where it came from:\n{text}"
        );
    }

    #[test]
    fn a_reversed_range_is_the_inverse_patch() {
        // The use case: upstream deleted something in `feat`; the patch extracted from
        // `main..feat` removes it, and `--reverse` extracts the same change the other
        // way round, putting it back.
        let (_tmp, layout, _main) = fixture();

        let done = import(&layout, &reversed(Some("main..feat"))).unwrap();

        assert!(done.reverse);
        assert_eq!(done.outcome, Outcome::Created);
        let text = std::fs::read_to_string(&done.path).unwrap();
        assert!(text.contains("+base"), "the undone change back:\n{text}");
        assert!(text.contains("-second"), "the removal:\n{text}");
        assert!(
            text.contains("Revert \"add the second line\""),
            "the subject says what the patch does, not what it undoes:\n{text}"
        );
        assert!(
            text.contains(&format!(
                "X-Skot-Import: reversed:range:main..feat {} {} ",
                done.to_sha, done.from_sha
            )),
            "the direction is recorded, ends swapped:\n{text}"
        );
    }

    #[test]
    fn the_forward_and_reversed_import_of_one_commit_are_two_patches() {
        let (_tmp, layout, _main) = fixture();

        let forward = import(&layout, &args(Some("main..feat"))).unwrap();
        let inverse = import(&layout, &reversed(Some("main..feat"))).unwrap();

        assert_eq!(forward.outcome, Outcome::Created);
        assert_eq!(inverse.outcome, Outcome::Created);
        assert_ne!(forward.patch, inverse.patch);
        let series = std::fs::read_to_string(layout.series_file()).unwrap();
        assert_eq!(series.lines().count(), 2, "both stay:\n{series}");
    }

    #[test]
    fn reimporting_the_same_reversed_range_is_a_no_op() {
        let (_tmp, layout, _main) = fixture();

        let first = import(&layout, &reversed(Some("main..feat"))).unwrap();
        let second = import(&layout, &reversed(Some("main..feat"))).unwrap();

        assert_eq!(second.outcome, Outcome::Unchanged);
        assert_eq!(second.patch, first.patch);
        let series = std::fs::read_to_string(layout.series_file()).unwrap();
        assert_eq!(series.lines().count(), 1, "the series must not grow");
    }

    #[test]
    fn reimporting_the_same_range_is_a_no_op() {
        let (_tmp, layout, _main) = fixture();

        let first = import(&layout, &args(Some("main..feat"))).unwrap();
        let second = import(&layout, &args(Some("main..feat"))).unwrap();

        assert_eq!(second.outcome, Outcome::Unchanged);
        assert_eq!(second.patch, first.patch);
        assert_eq!(second.series_line, Some(1));
        let series = std::fs::read_to_string(layout.series_file()).unwrap();
        assert_eq!(
            series.lines().count(),
            1,
            "the series must not grow:\n{series}"
        );
    }

    #[test]
    fn reimporting_a_bare_ref_is_a_no_op_too() {
        let (_tmp, layout, _main) = fixture();

        import(&layout, &args(Some("feat"))).unwrap();
        let again = import(&layout, &args(Some("feat"))).unwrap();

        assert_eq!(again.outcome, Outcome::Unchanged);
        let series = std::fs::read_to_string(layout.series_file()).unwrap();
        assert_eq!(series.lines().count(), 1);
    }

    #[test]
    fn an_unregistered_import_is_registered_instead_of_duplicated() {
        let (_tmp, layout, _main) = fixture();

        let first = import(
            &layout,
            &ImportArgs {
                at: None,
                no_register: true,
                ..args(Some("feat"))
            },
        )
        .unwrap();
        assert!(first.series_line.is_none());

        let second = import(&layout, &args(Some("feat"))).unwrap();

        assert_eq!(second.outcome, Outcome::Unchanged);
        assert_eq!(second.patch, first.patch);
        assert!(
            second.registered_now,
            "the file exists; the series was the gap"
        );
        let series = std::fs::read_to_string(layout.series_file()).unwrap();
        assert_eq!(series.trim(), first.patch);
    }

    #[test]
    fn importing_at_a_position_inserts_the_line() {
        let (_tmp, layout, _main) = fixture();
        testsupport::write_series(&layout, &[("a.patch", ""), ("b.patch", "")]);

        let done = import(
            &layout,
            &ImportArgs {
                at: Some("2".to_owned()),
                ..args(Some("main..feat"))
            },
        )
        .unwrap();

        assert_eq!(done.series_line, Some(2), "it occupies the second slot");
        let series = std::fs::read_to_string(layout.series_file()).unwrap();
        assert_eq!(
            series.trim_end().lines().collect::<Vec<_>>(),
            vec![
                "a.patch".to_owned(),
                done.patch.clone(),
                "b.patch".to_owned()
            ],
            "the line it took is the second one, and b.patch kept its own:\n{series}"
        );
        assert!(layout.patches_dir().join(&done.patch).is_file());
        assert!(layout.patches_dir().join("b.patch").is_file());
    }

    #[test]
    fn a_moved_tip_updates_the_patch_in_place() {
        let (_tmp, layout, _main) = fixture();
        let first = import(&layout, &args(Some("feat"))).unwrap();

        // The branch gains a commit, as a pull request would between two runs.
        let repo = git2::Repository::open_bare(layout.mirror_dir()).unwrap();
        let tip = repo
            .find_commit(git2::Oid::from_str(&first.to_sha).unwrap())
            .unwrap();
        commit_file(
            &repo,
            "refs/heads/feat",
            "add a third line",
            b"third\n",
            &[&tip],
        );

        let second = import(&layout, &args(Some("feat"))).unwrap();

        assert_eq!(second.outcome, Outcome::Updated);
        assert_eq!(
            second.patch, first.patch,
            "the same patch, not a second one"
        );
        assert_eq!(second.commits, 2);
        assert_eq!(second.previous_commits, Some(1));
        let text = std::fs::read_to_string(&second.path).unwrap();
        assert!(text.contains("+third"), "the new change:\n{text}");
        assert!(!text.contains("+second"), "the old one is gone:\n{text}");
        let series = std::fs::read_to_string(layout.series_file()).unwrap();
        assert_eq!(
            series.lines().count(),
            1,
            "an update rewrites, it does not add:\n{series}"
        );
    }

    #[test]
    fn a_moved_tip_updates_the_reversed_patch_in_place() {
        // The update path is direction-aware: the reversed patch answers to its own
        // identity, compares ends the way it recorded them, and reports the previous
        // commit count in the range's direction.
        let (_tmp, layout, _main) = fixture();
        let first = import(&layout, &reversed(Some("feat"))).unwrap();

        let repo = git2::Repository::open_bare(layout.mirror_dir()).unwrap();
        let tip = repo
            .find_commit(git2::Oid::from_str(&first.to_sha).unwrap())
            .unwrap();
        commit_file(
            &repo,
            "refs/heads/feat",
            "add a third line",
            b"third\n",
            &[&tip],
        );

        let second = import(&layout, &reversed(Some("feat"))).unwrap();

        assert_eq!(second.outcome, Outcome::Updated);
        assert_eq!(
            second.patch, first.patch,
            "the same patch, not a second one"
        );
        assert_eq!(second.commits, 2);
        assert_eq!(
            second.previous_commits,
            Some(1),
            "the count is the range's, not the reversed direction's"
        );
        let text = std::fs::read_to_string(&second.path).unwrap();
        assert!(text.contains("-third"), "the new tip undone:\n{text}");
        assert!(!text.contains("-second"), "the old one is gone:\n{text}");
        assert!(text.contains("+base"), "the whole range undone:\n{text}");
        let series = std::fs::read_to_string(layout.series_file()).unwrap();
        assert_eq!(series.lines().count(), 1, "an update rewrites:\n{series}");
    }

    #[test]
    fn an_edited_reversed_import_is_not_overwritten() {
        let (_tmp, layout, _main) = fixture();
        let first = import(&layout, &reversed(Some("feat"))).unwrap();

        let mut text = std::fs::read_to_string(&first.path).unwrap();
        text.push_str("# my own adjustment\n");
        std::fs::write(&first.path, text).unwrap();

        let repo = git2::Repository::open_bare(layout.mirror_dir()).unwrap();
        let tip = repo
            .find_commit(git2::Oid::from_str(&first.to_sha).unwrap())
            .unwrap();
        commit_file(
            &repo,
            "refs/heads/feat",
            "add a third line",
            b"third\n",
            &[&tip],
        );

        let err = import(&layout, &reversed(Some("feat")))
            .unwrap_err()
            .to_string();

        assert!(err.contains("edited it since"), "got {err}");
        assert!(err.contains(&first.patch), "names the patch: {err}");
        assert!(
            err.contains("the reverse of range:feat"),
            "the request is described as what it is: {err}"
        );
        let after = std::fs::read_to_string(&first.path).unwrap();
        assert!(
            after.contains("# my own adjustment"),
            "your edits must survive:\n{after}"
        );
    }

    #[test]
    fn a_reversed_range_of_several_commits_is_named_for_the_range() {
        // No single commit speaks for a span, so the name is the range reversed: the
        // same fallback as a forward import, with what it does in front.
        let (_tmp, layout, main) = fixture();
        let repo = git2::Repository::open_bare(layout.mirror_dir()).unwrap();
        let base = repo
            .find_commit(git2::Oid::from_str(&main).unwrap())
            .unwrap();
        let second = commit_file(
            &repo,
            "refs/heads/multi",
            "add the second line",
            b"second\n",
            &[&base],
        );
        let third = repo.find_commit(second).unwrap();
        commit_file(
            &repo,
            "refs/heads/multi",
            "add a third line",
            b"third\n",
            &[&third],
        );

        let done = import(&layout, &reversed(Some("main..multi"))).unwrap();

        assert_eq!(done.commits, 2);
        let text = std::fs::read_to_string(&done.path).unwrap();
        assert!(
            text.contains("Subject: [PATCH] Revert main..multi"),
            "the range is the subject:\n{text}"
        );
        assert!(text.contains("+base"), "the span undone:\n{text}");
        assert!(!text.contains("+second"), "net diff only:\n{text}");
    }

    /// A project synced from a local upstream: `main` and `feat` behind an `origin`
    /// that is free to move once the first import has run.
    fn moved_upstream_fixture() -> (tempfile::TempDir, Layout, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let upstream = tmp.path().join("up.git");
        let repo = git2::Repository::init_bare(&upstream).unwrap();
        let main = commit_file(&repo, "refs/heads/main", "base", b"base\n", &[]);
        {
            let parent = repo.find_commit(main).unwrap();
            commit_file(
                &repo,
                "refs/heads/feat",
                "add the second line",
                b"second\n",
                &[&parent],
            );
        }
        drop(repo);

        let layout = Layout::new(tmp.path().join("proj"));
        std::fs::create_dir_all(layout.patches_dir()).unwrap();
        std::fs::write(
            layout.config_file(),
            format!(
                "[upstream]\nurl = \"{}\"\ndefault-ref = \"main\"\n",
                upstream.display()
            ),
        )
        .unwrap();
        crate::commands::sync::fetch(&layout, Some("main")).unwrap();

        (tmp, layout, upstream)
    }

    /// Move upstream's `feat` one commit past `tip`, returning the new tip.
    fn move_feat_upstream(upstream: &std::path::Path, tip: &str) -> String {
        let repo = git2::Repository::open_bare(upstream).unwrap();
        let parent = repo.find_commit(git2::Oid::from_str(tip).unwrap()).unwrap();
        let moved = commit_file(
            &repo,
            "refs/heads/feat",
            "add a third line",
            b"third\n",
            &[&parent],
        );
        moved.to_string()
    }

    #[test]
    fn without_refresh_a_moved_upstream_is_not_chased() {
        // The mirror is a cache: what it holds answers the range, and only an explicit
        // `--refresh` asks upstream for more. The run that asks is the one that sees
        // the move.
        let (_tmp, layout, upstream) = moved_upstream_fixture();
        let first = import(&layout, &args(Some("main..feat"))).unwrap();
        let old_tip = first.to_sha.clone();
        let moved = move_feat_upstream(&upstream, &old_tip);

        let second = import(&layout, &args(Some("main..feat"))).unwrap();

        assert_eq!(
            second.outcome,
            Outcome::Unchanged,
            "the mirror's copy answers; --refresh is how a move is asked for"
        );
        assert_eq!(
            second.to_sha, old_tip,
            "the tip as the mirror holds it, not the one upstream moved to ({moved})"
        );
    }

    #[test]
    fn refresh_sees_a_moved_upstream() {
        let (_tmp, layout, upstream) = moved_upstream_fixture();
        let first = import(&layout, &args(Some("main..feat"))).unwrap();
        let old_tip = first.to_sha.clone();
        let moved = move_feat_upstream(&upstream, &old_tip);

        let second = import(&layout.with_refresh(true), &args(Some("main..feat"))).unwrap();

        assert_eq!(
            second.outcome,
            Outcome::Updated,
            "an explicit refresh asks upstream"
        );
        assert_eq!(second.to_sha, moved, "the move is seen");
    }

    #[test]
    fn an_unreachable_upstream_falls_back_to_the_mirror() {
        // Every ref the range needs is already in the mirror; the URL on its `origin`
        // cannot be reached. The import still runs, from what is held.
        let (tmp, layout, _main) = fixture();
        let repo = git2::Repository::open_bare(layout.mirror_dir()).unwrap();
        repo.remote("origin", tmp.path().join("gone.git").to_str().unwrap())
            .unwrap();
        drop(repo);

        let done = import(&layout, &args(Some("main..feat"))).unwrap();

        assert_eq!(done.outcome, Outcome::Created);
        let text = std::fs::read_to_string(&done.path).unwrap();
        assert!(text.contains("+second"), "the held change:\n{text}");
    }

    #[test]
    fn an_edited_import_is_not_overwritten() {
        let (_tmp, layout, _main) = fixture();
        let first = import(&layout, &args(Some("feat"))).unwrap();

        // The patch becomes yours: the recorded fingerprint no longer matches it.
        let mut text = std::fs::read_to_string(&first.path).unwrap();
        text.push_str("# my own adjustment\n");
        std::fs::write(&first.path, text).unwrap();

        // And the branch moves, so a fresh import would now overwrite those edits.
        let repo = git2::Repository::open_bare(layout.mirror_dir()).unwrap();
        let tip = repo
            .find_commit(git2::Oid::from_str(&first.to_sha).unwrap())
            .unwrap();
        commit_file(
            &repo,
            "refs/heads/feat",
            "add a third line",
            b"third\n",
            &[&tip],
        );

        let err = import(&layout, &args(Some("feat")))
            .unwrap_err()
            .to_string();

        assert!(err.contains("edited it since"), "got {err}");
        assert!(
            err.contains(&first.patch),
            "the message must name the patch: {err}"
        );
        let after = std::fs::read_to_string(&first.path).unwrap();
        assert!(
            after.contains("# my own adjustment"),
            "your edits must survive:\n{after}"
        );
        let series = std::fs::read_to_string(layout.series_file()).unwrap();
        assert_eq!(series.lines().count(), 1, "nothing was written");
    }

    #[test]
    fn a_multi_commit_range_is_named_after_the_range_as_written() {
        let (_tmp, layout, _main) = fixture();
        let repo = git2::Repository::open_bare(layout.mirror_dir()).unwrap();
        let tip = repo
            .find_reference("refs/heads/feat")
            .and_then(|reference| {
                reference
                    .target()
                    .ok_or_else(|| git2::Error::from_str("no target"))
            })
            .unwrap();
        let tip = repo.find_commit(tip).unwrap();
        commit_file(
            &repo,
            "refs/heads/feat",
            "a second change",
            b"third\n",
            &[&tip],
        );

        let done = import(&layout, &args(Some("main..feat"))).unwrap();

        assert_eq!(done.commits, 2);
        assert_eq!(
            done.patch, "import-main-feat.patch",
            "the range as written, not what it resolved to"
        );
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
    fn an_unset_default_ref_falls_back_to_the_mirrors_head() {
        // The config never named a base; the branch the mirror points at is the answer
        // a person would give, and the import proceeds without a failure over it.
        let (_tmp, layout, main) = fixture();
        std::fs::write(layout.config_file(), "[upstream]\nurl = \"unused\"\n").unwrap();

        let done = import(&layout, &args(Some("feat"))).unwrap();

        assert_eq!(done.from, "merge-base(main, feat)");
        assert_eq!(done.from_sha, main);
    }

    #[test]
    fn a_base_that_cannot_be_worked_out_fails_before_any_fetch() {
        // No `default-ref` in the config and no HEAD to stand in for one: known from
        // the config and the mirror alone, so refused before any network, proven here
        // by the run being offline (a fetch attempt would change the error).
        let (_tmp, layout, _main) = fixture();
        std::fs::write(layout.config_file(), "[upstream]\nurl = \"unused\"\n").unwrap();
        git2::Repository::open_bare(layout.mirror_dir())
            .unwrap()
            .set_head("refs/heads/ghost")
            .unwrap();

        let err = import(&layout.with_offline(true), &args(Some("feat")))
            .unwrap_err()
            .to_string();

        assert!(err.contains("cannot work out"), "got {err}");
        assert!(err.contains("--from"), "the remedy: {err}");
        assert_eq!(
            std::fs::read_dir(layout.patches_dir()).unwrap().count(),
            0,
            "nothing may be written before the refusal"
        );
    }

    #[test]
    fn a_store_asks_for_refs_by_name_and_widens_only_for_a_missing_commit_id() {
        assert_eq!(
            store_refspecs(None, &["main".to_owned()]),
            vec![
                "+refs/heads/main:refs/heads/main".to_owned(),
                "+refs/tags/main:refs/tags/main".to_owned(),
            ],
            "a named end is fetched by name"
        );
        assert_eq!(
            store_refspecs(
                None,
                &["deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_owned()]
            ),
            vec![
                "+refs/heads/*:refs/heads/*".to_owned(),
                "+refs/tags/*:refs/tags/*".to_owned(),
            ],
            "an id names no ref: only refs can carry what is behind it"
        );
        assert!(
            store_refspecs(None, &[]).is_empty(),
            "nothing needed, nothing asked"
        );
    }

    #[test]
    fn a_source_without_the_commit_fails_before_anything_is_copied() {
        // The local tree is read directly: a commit it does not hold is a fact known
        // from opening it, refused before any transfer runs.
        let (tmp, layout, _main) = fixture();
        let source = fork(tmp.path());
        let absent = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

        let err = import(
            &layout,
            &ImportArgs {
                at: None,
                repo: Some(source.to_str().unwrap().to_owned()),
                ..args(Some(absent))
            },
        )
        .unwrap_err();
        let msg = err.to_string();

        assert!(msg.contains("is not in"), "got {msg}");
        assert!(msg.contains(absent), "names the commit: {msg}");
        assert!(
            !msg.contains("target OID"),
            "the transport error this replaces must not surface: {msg}"
        );
        assert!(
            !layout.sources_dir().exists(),
            "a tree that cannot answer creates no store"
        );
        let mirror = Mirror::open(&layout).unwrap();
        assert!(
            mirror.repo().find_remote("import").is_err(),
            "nothing may have been fetched"
        );
    }

    #[test]
    fn a_local_tree_is_read_by_commit_id_without_touching_origin() {
        // The format-patch shape: a local clone holding the commit, a bare id on the
        // command line, an origin that must never be contacted. The commit comes from
        // the named source alone.
        let (tmp, layout, _main) = fixture();
        let repo = git2::Repository::open_bare(layout.mirror_dir()).unwrap();
        repo.remote("origin", tmp.path().join("gone.git").to_str().unwrap())
            .unwrap();
        drop(repo);
        let source = fork(tmp.path());
        let sha = {
            let source_repo = git2::Repository::open(&source).unwrap();
            source_repo
                .find_reference("refs/heads/feat")
                .unwrap()
                .target()
                .unwrap()
                .to_string()
        };

        let done = import(
            &layout,
            &ImportArgs {
                at: None,
                repo: Some(source.to_str().unwrap().to_owned()),
                ..args(Some(&sha))
            },
        )
        .unwrap();

        assert_eq!(done.to_sha, sha);
        let text = std::fs::read_to_string(&done.path).unwrap();
        assert!(
            text.contains("+forked"),
            "the local commit's change: {text}"
        );
        let mirror = Mirror::open(&layout).unwrap();
        assert!(
            mirror.try_resolve(&sha).is_none(),
            "the tree is read in place: nothing is copied into the mirror"
        );
        assert!(
            mirror.repo().find_remote("import").is_err(),
            "no remote is registered for a source read where it lies"
        );
        assert!(
            !layout.sources_dir().exists(),
            "a local tree needs no store"
        );
    }

    #[test]
    fn the_walk_probe_answers_from_what_the_mirror_holds() {
        let (_tmp, layout, _main) = fixture();
        let mirror = Mirror::open(&layout).unwrap();
        let config = layout.project().unwrap().0;
        let range = request(&args(Some("main..feat")), &config, None).unwrap();
        let bare = request(&args(Some("feat")), &config, None).unwrap();

        assert!(
            range_walks(&mirror, &range, None),
            "both ends held by name, history complete: the walk answers"
        );
        assert!(!range_walks(&mirror, &bare, None), "no base, no walk");
        assert!(
            !range_walks(&mirror, &bare, Some("ghost")),
            "a base the mirror lacks cannot walk"
        );
        assert!(
            range_walks(&mirror, &bare, Some("main")),
            "the default ref held: the merge base is walkable"
        );
    }

    #[test]
    fn the_commit_subject_names_the_patch() {
        let (_tmp, layout, _main) = fixture();

        let done = import(&layout, &args(Some("feat"))).unwrap();

        assert_eq!(done.patch, "add-the-second-line.patch");
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
                at: None,
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

        assert!(
            err.contains(&skot_args("import", "--pr")),
            "no remedy offered: {err}"
        );
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
                at: None,
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
                at: None,
                name: Some("the second line".into()),
                ..args(Some("feat"))
            },
        )
        .unwrap();

        assert_eq!(done.patch, "the-second-line.patch");
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
    fn a_source_tree_is_read_in_place_and_the_mirror_is_not_touched() {
        let (tmp, layout, _main) = fixture();
        let fork = fork(tmp.path());
        let mirror = Mirror::open(&layout).unwrap();
        let fixture_feat = mirror.try_resolve("refs/heads/feat").unwrap();
        drop(mirror);

        let done = import(
            &layout,
            &ImportArgs {
                at: None,
                repo: Some(fork.to_str().unwrap().to_owned()),
                ..args(Some("main..feat"))
            },
        )
        .unwrap();

        assert_eq!(done.commits, 1, "one commit between the named ends");
        let text = std::fs::read_to_string(&done.path).unwrap();
        assert!(text.contains("+forked"), "the fork's own change:\n{text}");

        let mirror = Mirror::open(&layout).unwrap();
        assert_eq!(
            mirror.try_resolve("refs/heads/feat"),
            Some(fixture_feat),
            "the mirror's own branch is left exactly as it was"
        );
        assert!(
            mirror.repo().find_remote("import").is_err(),
            "a source read in place never registers a remote"
        );
        assert!(
            mirror.repo().find_remote("origin").is_err(),
            "origin is `sync`'s remote and import must not invent one"
        );
        assert!(
            !layout.sources_dir().exists(),
            "a local tree needs no store"
        );
    }

    #[test]
    fn an_unrelated_tree_imports_a_bare_commit_by_its_parent() {
        // The commit's parent is the base, read from the commit itself: no history the
        // mirror and this tree could or could not share enters the question.
        let (_tmp, layout, _main) = fixture();
        let foreign = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(foreign.path()).unwrap();
        let root = commit_file(
            &repo,
            "refs/heads/main",
            "unrelated base",
            b"unrelated\n",
            &[],
        );
        let sha = {
            let parent = repo.find_commit(root).unwrap();
            commit_file(
                &repo,
                "refs/heads/feat",
                "foreign line",
                b"foreign\n",
                &[&parent],
            )
            .to_string()
        };
        drop(repo);

        let done = import(
            &layout,
            &ImportArgs {
                at: None,
                repo: Some(foreign.path().to_str().unwrap().to_owned()),
                ..args(Some(&sha))
            },
        )
        .unwrap();

        assert_eq!(done.from, format!("{sha}^"), "the commit's own parent");
        assert_eq!(done.from_sha, root.to_string(), "and nothing older");
        assert_eq!(done.commits, 1, "the one change the commit introduced");
        let text = std::fs::read_to_string(&done.path).unwrap();
        assert!(text.contains("+foreign"), "the foreign change:\n{text}");
    }

    #[test]
    fn a_commit_on_no_ref_imports_from_the_object_that_is_here() {
        // What a reset or a rebase leaves behind: the object exists in the tree and no
        // ref names it. `--repo` reads objects, not just branches, so it comes across.
        let (tmp, layout, _main) = fixture();
        let source = fork(tmp.path());
        let sha = {
            let repo = git2::Repository::open(&source).unwrap();
            let sha = repo
                .find_reference("refs/heads/feat")
                .unwrap()
                .target()
                .unwrap()
                .to_string();
            repo.find_branch("feat", git2::BranchType::Local)
                .unwrap()
                .delete()
                .unwrap();
            sha
        };

        let done = import(
            &layout,
            &ImportArgs {
                at: None,
                repo: Some(source.to_str().unwrap().to_owned()),
                ..args(Some(&sha))
            },
        )
        .unwrap();

        assert_eq!(done.to_sha, sha, "the object as it lies in the tree");
        let text = std::fs::read_to_string(&done.path).unwrap();
        assert!(text.contains("+forked"), "the unreachable commit:\n{text}");
        assert!(
            !layout.sources_dir().exists(),
            "no ref to fetch: nothing needed fetching"
        );
    }

    #[test]
    fn a_bare_ref_name_reads_the_source_trees_own_line() {
        let (tmp, layout, _main) = fixture();
        let source = fork(tmp.path());

        let done = import(
            &layout,
            &ImportArgs {
                at: None,
                repo: Some(source.to_str().unwrap().to_owned()),
                ..args(Some("feat"))
            },
        )
        .unwrap();

        assert_eq!(done.from, "feat^", "the change that line's tip introduced");
        assert_eq!(done.commits, 1);
        let text = std::fs::read_to_string(&done.path).unwrap();
        assert!(text.contains("+forked"), "the tree's own commit:\n{text}");
    }

    #[test]
    fn a_url_source_fetches_into_a_store_of_its_own() {
        let (tmp, layout, _main) = fixture();
        let source = fork(tmp.path());
        let url = format!("file://{}", source.display());

        let first = import(
            &layout,
            &ImportArgs {
                at: None,
                repo: Some(url.clone()),
                ..args(Some("main..feat"))
            },
        )
        .unwrap();

        let text = std::fs::read_to_string(&first.path).unwrap();
        assert!(text.contains("+forked"), "the source's change:\n{text}");
        assert!(
            !text.contains(source.to_str().unwrap()),
            "a file:// source is a local origin; the path stays out of the patch"
        );
        assert_eq!(
            count_header_lines(&text, "X-Skot-Source: "),
            0,
            "a file:// URL stands for this machine too"
        );
        let entries: Vec<_> = std::fs::read_dir(layout.sources_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            entries,
            vec![std::ffi::OsString::from(source_key(&url).unwrap())],
            "one store, keyed by the location as given"
        );
        let store = Mirror::at(
            layout.source_store(&source_key(&url).unwrap()),
            layout.offline(),
        )
        .unwrap();
        assert!(
            store.try_resolve("refs/heads/feat").is_some(),
            "the store holds what was fetched"
        );
        let mirror = Mirror::open(&layout).unwrap();
        assert!(
            mirror.repo().find_remote("import").is_err(),
            "the mirror is not where a URL is read"
        );
        assert!(
            mirror.repo().find_remote("origin").is_err(),
            "the mirror still belongs to `sync` alone"
        );

        let second = import(
            &layout,
            &ImportArgs {
                at: None,
                repo: Some(url),
                ..args(Some("main..feat"))
            },
        )
        .unwrap();
        assert_eq!(
            second.outcome,
            Outcome::Unchanged,
            "the same source and range is the same import"
        );
    }

    #[test]
    fn refresh_checks_a_local_trees_own_upstream_for_what_the_tree_lacks() {
        let tmp = tempfile::tempdir().unwrap();
        let upstream = tmp.path().join("upstream.git");
        let repo = git2::Repository::init_bare(&upstream).unwrap();
        let main = commit_file(&repo, "refs/heads/main", "base", b"base\n", &[]);
        let feat = {
            let parent = repo.find_commit(main).unwrap();
            commit_file(
                &repo,
                "refs/heads/feat",
                "add the second line",
                b"second\n",
                &[&parent],
            )
            .to_string()
        };
        repo.set_head("refs/heads/main").unwrap();
        drop(repo);
        let source_path = tmp.path().join("source");
        let source = git2::Repository::clone(upstream.to_str().unwrap(), &source_path).unwrap();
        for name in ["refs/heads/feat", "refs/remotes/origin/feat"] {
            if let Ok(mut reference) = source.find_reference(name) {
                reference.delete().unwrap();
            }
        }
        drop(source);

        let layout = Layout::new(tmp.path().join("proj"));
        std::fs::create_dir_all(layout.patches_dir()).unwrap();
        std::fs::write(
            layout.config_file(),
            "[upstream]\nurl = \"unused\"\ndefault-ref = \"main\"\n",
        )
        .unwrap();
        let args = ImportArgs {
            at: None,
            repo: Some(source_path.to_str().unwrap().to_owned()),
            ..args(Some("main..feat"))
        };

        let err = import(&layout, &args).unwrap_err().to_string();
        assert!(
            err.contains("neither a branch, tag, nor commit"),
            "the tree cannot answer: {err}"
        );
        assert!(err.contains("--refresh"), "the remedy: {err}");
        assert!(
            !layout.sources_dir().exists(),
            "without --refresh nothing is fetched from the tree's upstream"
        );

        let done = import(&layout.with_refresh(true), &args).unwrap();
        assert_eq!(
            done.to_sha, feat,
            "the tree's own upstream carries the missing line"
        );
        assert!(
            layout.sources_dir().exists(),
            "--refresh is the permission to ask, and the answer lands in the store"
        );
        assert!(
            Mirror::open(&layout).is_err(),
            "the main mirror is never involved in either run"
        );
        let text = std::fs::read_to_string(&done.path).unwrap();
        assert!(text.contains("+second"), "the fetched change:\n{text}");
    }

    #[test]
    fn a_root_commit_cannot_be_its_own_base() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(tmp.path()).unwrap();
        let root = commit_file(&repo, "refs/heads/main", "the start", b"start\n", &[]);
        drop(repo);
        let (_tmp2, layout, _main) = fixture();

        let err = import(
            &layout,
            &ImportArgs {
                at: None,
                repo: Some(tmp.path().to_str().unwrap().to_owned()),
                ..args(Some(&root.to_string()))
            },
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("root commit"), "the fact: {err}");
        assert!(err.contains("--from"), "the remedy: {err}");
    }

    #[test]
    fn a_source_tree_imports_without_a_mirror_at_all() {
        let tmp = tempfile::tempdir().unwrap();
        let source = fork(tmp.path());
        let layout = Layout::new(tmp.path().join("proj"));
        std::fs::create_dir_all(layout.patches_dir()).unwrap();
        std::fs::write(
            layout.config_file(),
            "[upstream]\nurl = \"unused\"\ndefault-ref = \"main\"\n",
        )
        .unwrap();

        let done = import(
            &layout,
            &ImportArgs {
                at: None,
                repo: Some(source.to_str().unwrap().to_owned()),
                ..args(Some("main..feat"))
            },
        )
        .unwrap();

        let text = std::fs::read_to_string(&done.path).unwrap();
        assert!(text.contains("+forked"), "the source's change:\n{text}");
        assert!(
            !layout.mirror_dir().exists(),
            "nothing may have tried to create a mirror for this run"
        );
    }

    /// The provenance a patch records, panicking for one that records none.
    fn provenance_of(text: &str) -> Provenance {
        text.lines()
            .find_map(parse_provenance)
            .expect("the patch records its import")
    }

    /// How many header lines of a kind a patch carries.
    fn count_header_lines(text: &str, prefix: &str) -> usize {
        text.lines().filter(|line| line.starts_with(prefix)).count()
    }

    #[test]
    fn a_source_is_recorded_by_where_it_lies_and_never_the_path() {
        let (tmp, layout, _main) = fixture();
        let source = fork(tmp.path());
        let source_str = source.to_str().unwrap().to_owned();

        let done = import(
            &layout,
            &ImportArgs {
                at: None,
                repo: Some(source_str.clone()),
                ..args(Some("main..feat"))
            },
        )
        .unwrap();

        let text = std::fs::read_to_string(&done.path).unwrap();
        assert!(
            !text.contains(&source_str),
            "a committed patch must not say where the files sat:\n{text}"
        );
        let provenance = provenance_of(&text);
        assert!(
            provenance.identity.ends_with("#main..feat"),
            "the request still speaks for itself: {}",
            provenance.identity
        );
        let token = provenance
            .identity
            .strip_prefix("repo:")
            .and_then(|rest| rest.split('#').next())
            .unwrap();
        assert_eq!(token.len(), 16, "a digest, not a location: {token}");
        assert!(
            token.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "a digest reads as hex: {token}"
        );
        assert_eq!(
            count_header_lines(&text, "X-Skot-Source: "),
            0,
            "no origin worth recording"
        );
    }

    #[test]
    fn a_source_with_a_network_origin_is_recorded_by_that_origin() {
        let (tmp, layout, _main) = fixture();
        let source = fork(tmp.path());
        let source_str = source.to_str().unwrap().to_owned();
        let origin = "https://example.com/me/fork.git";
        let repo = git2::Repository::open(&source).unwrap();
        repo.remote("origin", origin).unwrap();
        drop(repo);

        let first = import(
            &layout,
            &ImportArgs {
                at: None,
                repo: Some(source_str.clone()),
                ..args(Some("main..feat"))
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&first.path).unwrap();
        assert!(!text.contains(&source_str), "never the path:\n{text}");
        assert_eq!(
            provenance_of(&text).identity,
            format!("repo:{origin}#main..feat"),
            "the origin a person could clone from names the source"
        );
        assert!(
            text.contains(&format!("X-Skot-Source: {origin}")),
            "the origin is also said in its own line:\n{text}"
        );

        // Another commit on the line: the next run updates the same patch, and neither
        // header line may stack while it does.
        let repo = git2::Repository::open(&source).unwrap();
        {
            let tip = repo
                .find_reference("refs/heads/feat")
                .unwrap()
                .target()
                .unwrap();
            let parent = repo.find_commit(tip).unwrap();
            commit_file(
                &repo,
                "refs/heads/feat",
                "forked again",
                b"forked again\n",
                &[&parent],
            );
        }
        drop(repo);

        let second = import(
            &layout,
            &ImportArgs {
                at: None,
                repo: Some(source_str),
                ..args(Some("main..feat"))
            },
        )
        .unwrap();
        assert_eq!(second.outcome, Outcome::Updated, "the line moved on");
        let text = std::fs::read_to_string(&second.path).unwrap();
        assert_eq!(
            count_header_lines(&text, "X-Skot-Import: "),
            1,
            "one provenance line:\n{text}"
        );
        assert_eq!(
            count_header_lines(&text, "X-Skot-Source: "),
            1,
            "one source line:\n{text}"
        );
        assert!(text.contains(&format!("X-Skot-Source: {origin}")));
    }

    #[test]
    fn a_local_origin_is_not_written_into_a_patch() {
        let (tmp, layout, _main) = fixture();
        let source = fork(tmp.path());
        let source_str = source.to_str().unwrap().to_owned();
        let local = tmp.path().join("upstream.git");
        let repo = git2::Repository::open(&source).unwrap();
        repo.remote("origin", local.to_str().unwrap()).unwrap();
        drop(repo);

        let done = import(
            &layout,
            &ImportArgs {
                at: None,
                repo: Some(source_str.clone()),
                ..args(Some("main..feat"))
            },
        )
        .unwrap();

        let text = std::fs::read_to_string(&done.path).unwrap();
        assert!(
            !text.contains(&source_str) && !text.contains(local.to_str().unwrap()),
            "two local names for the same machine, neither written:\n{text}"
        );
        let token = provenance_of(&text)
            .identity
            .strip_prefix("repo:")
            .and_then(|rest| rest.split('#').next())
            .unwrap()
            .to_owned();
        assert_eq!(token.len(), 16, "a local origin is worth no more: {token}");
        assert_eq!(
            count_header_lines(&text, "X-Skot-Source: "),
            0,
            "a local origin would re-leak the path it stands for"
        );
    }

    #[test]
    fn a_source_token_is_its_network_origin_or_a_digest_of_where_it_lies() {
        let path = "/tmp/example/fork";
        let url = "https://example.com/me/fork.git";
        let digest = source_key(path).unwrap();
        assert_eq!(source_token(path, Some(url)).unwrap(), url);
        assert_eq!(
            source_token(path, Some("git@example.com:me/fork.git")).unwrap(),
            "git@example.com:me/fork.git"
        );
        assert_eq!(source_token(path, Some(path)).unwrap(), digest);
        assert_eq!(
            source_token(path, Some("file:///tmp/example/fork")).unwrap(),
            digest
        );
        assert_eq!(source_token(path, None).unwrap(), digest);
        let file_url_digest = source_token("file:///tmp/example/fork", None).unwrap();
        assert_eq!(file_url_digest.len(), 16);
        assert!(
            !file_url_digest.contains("example"),
            "the location never shows through: {file_url_digest}"
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

    /// A project whose mirror has been synced from `source`: bare mirror, `origin` set,
    /// `main` fetched, config pointing at the source. `sync` never fetches pull request
    /// refs, which is why `import` does.
    fn synced_project(root: &std::path::Path, source: &std::path::Path) -> Layout {
        let layout = Layout::new(root.join("proj"));
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
        layout
    }

    #[test]
    fn a_pull_request_is_fetched_from_origin_and_imported() {
        let tmp = tempfile::tempdir().unwrap();
        let (source, pr_sha) = pull_request_source(tmp.path());
        let layout = synced_project(tmp.path(), &source);

        let done = import(
            &layout,
            &ImportArgs {
                at: None,
                pr: Some("7".to_owned()),
                ..args(None)
            },
        )
        .unwrap();

        assert_eq!(done.to, "refs/pull/7/head");
        assert_eq!(done.to_sha, pr_sha);
        assert_eq!(done.commits, 1);
        assert_eq!(done.outcome, Outcome::Created);
        assert_eq!(done.from_sha, {
            let mirror = Mirror::open(&layout).unwrap();
            mirror.resolve("main").unwrap()
        });
        let text = std::fs::read_to_string(&done.path).unwrap();
        assert!(text.contains("+pr"), "the pull request's change:\n{text}");
        assert!(
            text.contains("X-Skot-Import: pr:7 "),
            "the patch must record where it came from:\n{text}"
        );
    }

    #[test]
    fn an_offline_pull_request_import_falls_back_on_the_mirror() {
        // The shape `--offline` leaves: the mirror already holds the base and the pull
        // request, the origin names a network transport, and neither fetch is allowed
        // to happen. The import still answers, from what is on disk.
        let tmp = tempfile::tempdir().unwrap();
        let (source, _sha) = pull_request_source(tmp.path());
        let layout = synced_project(tmp.path(), &source);
        let request = ImportArgs {
            at: None,
            pr: Some("7".to_owned()),
            ..args(None)
        };
        import(&layout, &request).unwrap();
        git2::Repository::open_bare(layout.mirror_dir())
            .unwrap()
            .remote_set_url("origin", "https://example.invalid/upstream.git")
            .unwrap();

        let done = import(&layout.with_offline(true), &request).unwrap();

        assert_eq!(
            done.outcome,
            Outcome::Unchanged,
            "the mirror held everything, so nothing needed asking"
        );
    }

    #[test]
    fn reimporting_a_pull_request_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let (source, _sha) = pull_request_source(tmp.path());
        let layout = synced_project(tmp.path(), &source);
        let request = ImportArgs {
            at: None,
            pr: Some("7".to_owned()),
            ..args(None)
        };

        let first = import(&layout, &request).unwrap();
        let second = import(&layout, &request).unwrap();

        assert_eq!(second.outcome, Outcome::Unchanged);
        assert_eq!(second.patch, first.patch);
        let series = std::fs::read_to_string(layout.series_file()).unwrap();
        assert_eq!(
            series.lines().count(),
            1,
            "the series must not grow:\n{series}"
        );
    }

    #[test]
    fn a_pull_request_that_gained_commits_updates_its_patch() {
        let tmp = tempfile::tempdir().unwrap();
        let (source, pr_sha) = pull_request_source(tmp.path());
        let layout = synced_project(tmp.path(), &source);
        let request = ImportArgs {
            at: None,
            pr: Some("7".to_owned()),
            ..args(None)
        };
        let first = import(&layout, &request).unwrap();

        // The pull request picks up a second commit.
        {
            let source_repo = git2::Repository::open(&source).unwrap();
            let tip = source_repo
                .find_commit(git2::Oid::from_str(&pr_sha).unwrap())
                .unwrap();
            commit_file(
                &source_repo,
                "refs/pull/7/head",
                "more of the pull request",
                b"pr2\n",
                &[&tip],
            );
        }

        let second = import(&layout, &request).unwrap();

        assert_eq!(second.outcome, Outcome::Updated);
        assert_eq!(
            second.patch, first.patch,
            "the same patch, not a second one"
        );
        assert_eq!(second.commits, 2);
        assert_eq!(second.previous_commits, Some(1));
        let text = std::fs::read_to_string(&second.path).unwrap();
        assert!(text.contains("+pr2"), "the new commit's change:\n{text}");
        assert!(!text.contains("+pr\n"), "the old state is gone:\n{text}");
        assert!(
            text.contains("X-Skot-Import: pr:7 "),
            "provenance must be refreshed:\n{text}"
        );
        let series = std::fs::read_to_string(layout.series_file()).unwrap();
        assert_eq!(
            series.lines().count(),
            1,
            "an update rewrites, it does not add:\n{series}"
        );
    }

    #[test]
    fn a_multi_commit_pull_request_is_named_import_pr_n() {
        let tmp = tempfile::tempdir().unwrap();
        let (source, pr_sha) = pull_request_source(tmp.path());
        {
            let source_repo = git2::Repository::open(&source).unwrap();
            let tip = source_repo
                .find_commit(git2::Oid::from_str(&pr_sha).unwrap())
                .unwrap();
            commit_file(
                &source_repo,
                "refs/pull/7/head",
                "second PR commit",
                b"pr2\n",
                &[&tip],
            );
        }
        let layout = synced_project(tmp.path(), &source);

        let done = import(
            &layout,
            &ImportArgs {
                at: None,
                pr: Some("7".to_owned()),
                ..args(None)
            },
        )
        .unwrap();

        assert_eq!(done.commits, 2);
        assert_eq!(
            done.patch, "import-pr-7.patch",
            "the request, not the resolved range, names a multi-commit import"
        );
    }

    #[test]
    fn the_forges_people_use_know_their_own_namespace() {
        let config = crate::config::Config::parse("").unwrap();
        for (url, want) in [
            ("https://github.com/o/r", "refs/pull/7/head"),
            ("https://gitlab.com/o/r", "refs/merge-requests/7/head"),
            ("https://bitbucket.org/o/r", "refs/pull-requests/7/from"),
            ("https://codeberg.org/o/r", "refs/pull/7/head"),
            ("https://gitea.com/o/r", "refs/pull/7/head"),
            ("https://some-forge.example/o/r", "refs/pull/7/head"),
            ("git@github.com:o/r.git", "refs/pull/7/head"),
            ("../local/path", "refs/pull/7/head"),
        ] {
            assert_eq!(pr_ref_for(Some(url), &config, 7), want, "for {url}");
        }
        assert_eq!(pr_ref_for(None, &config, 7), "refs/pull/7/head");
    }

    #[test]
    fn a_config_entry_names_a_forge_the_table_does_not_know() {
        let config = crate::config::Config::parse(
            "[pr-refs]\n\"forge.corp.example\" = \"refs/merge-requests/{n}/head\"\n",
        )
        .unwrap();
        assert_eq!(
            pr_ref_for(Some("https://forge.corp.example/o/r"), &config, 9),
            "refs/merge-requests/9/head"
        );
    }

    #[test]
    fn a_wildcard_entry_covers_the_subdomains_of_its_domain() {
        let config =
            crate::config::Config::parse("[pr-refs]\n\"*.corp.example\" = \"refs/mr/{n}/head\"\n")
                .unwrap();
        assert_eq!(
            pr_ref_for(Some("https://git.corp.example/o/r"), &config, 9),
            "refs/mr/9/head"
        );
        assert_eq!(
            pr_ref_for(Some("https://a.git.corp.example/o/r"), &config, 9),
            "refs/mr/9/head"
        );
        // Predictable: the wildcard covers subdomains, never the domain itself.
        assert_eq!(
            pr_ref_for(Some("https://corp.example/o/r"), &config, 9),
            "refs/pull/9/head"
        );
        assert_eq!(
            pr_ref_for(Some("https://notcorp.example/o/r"), &config, 9),
            "refs/pull/9/head"
        );
    }

    #[test]
    fn an_exact_config_entry_beats_a_wildcard() {
        let config = crate::config::Config::parse(
            "[pr-refs]\n\"git.corp.example\" = \"refs/exact/{n}\"\n\"*.corp.example\" = \"refs/wild/{n}\"\n",
        )
        .unwrap();
        assert_eq!(
            pr_ref_for(Some("https://git.corp.example/o/r"), &config, 9),
            "refs/exact/9"
        );
        assert_eq!(
            pr_ref_for(Some("https://other.corp.example/o/r"), &config, 9),
            "refs/wild/9"
        );
    }

    #[test]
    fn a_config_entry_can_override_a_forge_in_the_table() {
        let config =
            crate::config::Config::parse("[pr-refs]\n\"github.com\" = \"refs/mine/{n}\"\n")
                .unwrap();
        assert_eq!(
            pr_ref_for(Some("https://github.com/o/r"), &config, 3),
            "refs/mine/3"
        );
    }

    #[test]
    fn an_ip_remote_is_matched_by_its_ip() {
        let config = crate::config::Config::parse(
            "[pr-refs]\n\"127.0.0.1\" = \"refs/merge-requests/{n}/head\"\n",
        )
        .unwrap();
        assert_eq!(
            pr_ref_for(Some("http://127.0.0.1:8080/o/r"), &config, 5),
            "refs/merge-requests/5/head"
        );
        assert_eq!(
            pr_ref_for(Some("git@127.0.0.1:o/r.git"), &config, 5),
            "refs/merge-requests/5/head"
        );
    }

    #[test]
    fn a_url_without_a_host_suggests_no_namespace() {
        for url in [
            "file:///tmp/o/r.git",
            "/abs/path",
            "../rel/path",
            "./r",
            "~/r",
            "plain",
        ] {
            assert_eq!(url_domain(url).as_deref(), None, "for {url}");
        }
        assert_eq!(
            url_domain("https://github.com/o/r").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            url_domain("https://github.com:8443/o/r").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            url_domain("ssh://git@github.com/o/r").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            url_domain("git@gitlab.com:o/r.git").as_deref(),
            Some("gitlab.com")
        );
    }

    #[test]
    fn a_remote_url_with_a_port_or_brackets_keeps_only_the_host() {
        // Ports and brackets never become part of the key: the host is what a remote
        // is matched by, so an IPv6 address arrives as the bare address, which is
        // `"::1"` as a quoted skot.toml key.
        assert_eq!(url_domain("ssh://[::1]:2222/o/r").as_deref(), Some("::1"));
        assert_eq!(
            url_domain("ssh://user@[::1]:2222/o/r").as_deref(),
            Some("::1")
        );
        assert_eq!(
            url_domain("http://127.0.0.1:8080/o/r").as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(
            url_domain("git@127.0.0.1:o/r.git").as_deref(),
            Some("127.0.0.1")
        );
    }

    #[test]
    fn an_scp_remote_without_a_user_is_still_a_remote() {
        // git reads a colon before the first slash as scp syntax whether or not there
        // is a `user@` in front of it, so the host is still a host.
        assert_eq!(
            url_domain("127.0.0.1:o/r.git").as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(
            url_domain("github.com:o/r.git").as_deref(),
            Some("github.com")
        );
    }

    #[test]
    fn a_host_is_matched_the_way_a_user_would_spell_it() {
        // Hosts are case-insensitive everywhere they are looked up; the same goes for
        // the side being looked up (`Config::parse` lowercases the keys).
        assert_eq!(
            url_domain("git@GitHub.COM:o/r.git").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            url_domain("ssh://GitHub.COM/o/r").as_deref(),
            Some("github.com")
        );
    }

    #[test]
    fn every_shape_a_remote_can_take_never_choke() {
        // Whatever the answer, extraction cannot fail a command: no host means the
        // default namespace, which is all a missing answer ever costs.
        for url in [
            "",
            "git@[::1]:o/r.git",
            "C:\\repo",
            "C:/repo",
            "file://gitlab.com/o/r",
            "http://",
            "https://user:p@ss@example.com/x",
            "git://github.com/o/r",
            "unknown-scheme://host.example/x",
            "user@host:/abs/path",
            "/abs/with:colon",
        ] {
            let _ = url_domain(url);
        }
        // A file URL speaks to no forge, host or not.
        assert_eq!(url_domain("file://gitlab.com/o/r").as_deref(), None);
        assert_eq!(
            url_domain("git://github.com/o/r").as_deref(),
            Some("github.com")
        );
    }

    #[test]
    fn a_pull_request_number_is_read_against_the_repository_being_asked() {
        let config = crate::config::Config::parse("").unwrap();
        let request = request(
            &ImportArgs {
                pr: Some("7".to_owned()),
                ..args(None)
            },
            &config,
            Some("https://gitlab.com/o/r"),
        )
        .unwrap();
        assert_eq!(request.to, "refs/merge-requests/7/head");
        assert_eq!(request.pull.as_deref(), Some("7"));
    }

    #[test]
    fn a_fully_qualified_pull_request_ref_is_used_as_written() {
        let config = crate::config::Config::parse("").unwrap();
        let request = request(
            &ImportArgs {
                pr: Some("refs/changes/12/345/1".to_owned()),
                ..args(None)
            },
            &config,
            Some("https://github.com/o/r"),
        )
        .unwrap();
        assert_eq!(request.to, "refs/changes/12/345/1");
        assert_eq!(request.pull.as_deref(), Some("refs/changes/12/345/1"));
    }

    #[test]
    fn a_pull_request_argument_that_is_neither_a_number_nor_a_ref_is_refused() {
        let config = crate::config::Config::parse("").unwrap();
        let err = request(
            &ImportArgs {
                pr: Some("abc".to_owned()),
                ..args(None)
            },
            &config,
            None,
        )
        .err()
        .expect("neither a number nor a ref is refused")
        .to_string();
        assert!(err.contains("--pr 42"), "both shapes, got: {err}");
        assert!(err.contains("refs/changes"), "both shapes, got: {err}");
    }

    #[test]
    fn a_fully_qualified_pull_request_ref_is_fetched_as_written() {
        let tmp = tempfile::tempdir().unwrap();
        let (source, pr_sha) = pull_request_source(tmp.path());
        let layout = synced_project(tmp.path(), &source);

        let done = import(
            &layout,
            &ImportArgs {
                pr: Some("refs/pull/7/head".to_owned()),
                ..args(None)
            },
        )
        .unwrap();

        assert_eq!(done.to, "refs/pull/7/head");
        assert_eq!(done.to_sha, pr_sha);
        let text = std::fs::read_to_string(&done.path).unwrap();
        assert!(
            text.contains("X-Skot-Import: pr:refs/pull/7/head "),
            "the full ref records as written:\n{text}"
        );
    }

    #[test]
    fn a_pull_request_comes_from_the_source_repository_when_one_is_given() {
        // The cross-forge shape: the project's origin does not carry the request, the
        // `--repo` repository does, and the request is fetched from there.
        let tmp = tempfile::tempdir().unwrap();
        let (source, pr_sha) = pull_request_source(tmp.path());
        let origin = tmp.path().join("origin-clone");
        {
            // The origin clone takes the history and not the request: the source repo's
            // HEAD is unborn, so a plain clone transfers nothing, and the heads are
            // fetched across by name.
            let clone = git2::Repository::clone(source.to_str().unwrap(), &origin).unwrap();
            let mut remote = clone.find_remote("origin").unwrap();
            remote
                .fetch(&["+refs/heads/*:refs/heads/*"], None, None)
                .unwrap();
        }
        let layout = synced_project(tmp.path(), &origin);

        let done = import(
            &layout,
            &ImportArgs {
                pr: Some("7".to_owned()),
                repo: Some(source.to_str().unwrap().to_owned()),
                ..args(None)
            },
        )
        .unwrap();

        assert_eq!(done.to, "refs/pull/7/head");
        assert_eq!(done.to_sha, pr_sha);
        assert_eq!(done.outcome, Outcome::Created);
    }

    #[test]
    fn the_forge_of_the_repository_decides_the_pull_request_namespace() {
        // The shape an offline run leaves: the mirror already holds the merge request
        // under its forge's own name, origin names gitlab.com, nothing may be asked.
        // The import can only succeed if the namespace came from the domain.
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("src");
        let mr = {
            let repo = git2::Repository::init(&source).unwrap();
            let main_oid = commit_file(&repo, "refs/heads/main", "main", b"base\n", &[]);
            let main_commit = repo.find_commit(main_oid).unwrap();
            commit_file(
                &repo,
                "refs/merge-requests/7/head",
                "the merge request",
                b"mr\n",
                &[&main_commit],
            )
        };
        let layout = synced_project(tmp.path(), &source);
        {
            let mirror_repo = git2::Repository::open_bare(layout.mirror_dir()).unwrap();
            let mut seed = mirror_repo
                .remote("seed", source.to_str().unwrap())
                .unwrap();
            seed.fetch(
                &["+refs/merge-requests/7/head:refs/merge-requests/7/head"],
                None,
                None,
            )
            .unwrap();
            drop(seed);
            mirror_repo
                .remote_set_url("origin", "https://gitlab.com/team/project.git")
                .unwrap();
        }

        let done = import(
            &layout.with_offline(true),
            &ImportArgs {
                pr: Some("7".to_owned()),
                ..args(None)
            },
        )
        .unwrap();

        assert_eq!(done.to, "refs/merge-requests/7/head");
        assert_eq!(done.to_sha, mr.to_string());
    }

    #[test]
    fn the_config_table_decides_the_namespace_for_a_forge_it_does_not_know() {
        // The same offline shape, for a forge the built-in table cannot name: one line
        // in skot.toml is enough.
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("src");
        let mr = {
            let repo = git2::Repository::init(&source).unwrap();
            let main_oid = commit_file(&repo, "refs/heads/main", "main", b"base\n", &[]);
            let main_commit = repo.find_commit(main_oid).unwrap();
            commit_file(
                &repo,
                "refs/corp/requests/7/head",
                "the change",
                b"corp\n",
                &[&main_commit],
            )
        };
        let layout = synced_project(tmp.path(), &source);
        std::fs::write(
            layout.config_file(),
            format!(
                "[upstream]\nurl = \"{}\"\ndefault-ref = \"main\"\n\n\
                 [pr-refs]\n\"forge.corp.example\" = \"refs/corp/requests/{{n}}/head\"\n",
                source.display()
            ),
        )
        .unwrap();
        {
            let mirror_repo = git2::Repository::open_bare(layout.mirror_dir()).unwrap();
            let mut seed = mirror_repo
                .remote("seed", source.to_str().unwrap())
                .unwrap();
            seed.fetch(
                &["+refs/corp/requests/7/head:refs/corp/requests/7/head"],
                None,
                None,
            )
            .unwrap();
            drop(seed);
            mirror_repo
                .remote_set_url("origin", "https://forge.corp.example/team/project.git")
                .unwrap();
        }

        let done = import(
            &layout.with_offline(true),
            &ImportArgs {
                pr: Some("7".to_owned()),
                ..args(None)
            },
        )
        .unwrap();

        assert_eq!(done.to, "refs/corp/requests/7/head");
        assert_eq!(done.to_sha, mr.to_string());
    }

    #[test]
    fn an_unknown_forge_is_told_how_to_name_its_namespace() {
        let tmp = tempfile::tempdir().unwrap();
        let (source, _sha) = pull_request_source(tmp.path());
        let layout = synced_project(tmp.path(), &source);
        git2::Repository::open_bare(layout.mirror_dir())
            .unwrap()
            .remote_set_url("origin", "https://forge.unknown.example/o/r.git")
            .unwrap();

        let err = import(
            &layout.with_offline(true),
            &ImportArgs {
                pr: Some("7".to_owned()),
                ..args(None)
            },
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("forge.unknown.example"), "got: {msg}");
        assert!(msg.contains("[pr-refs]"), "got: {msg}");
        assert!(msg.contains("refs/pull/{n}/head"), "got: {msg}");
    }
}

#[cfg(test)]
mod provenance_tests {
    use super::{parse_provenance, provenance_line};

    #[test]
    fn an_identity_with_spaces_survives_the_round_trip() {
        let line = provenance_line("repo:../fork main..feat", "aaa111", "bbb222", "ccc333");
        let parsed = parse_provenance(&line).expect("the line parses");

        assert_eq!(parsed.identity, "repo:../fork main..feat");
        assert_eq!(parsed.from, "aaa111");
        assert_eq!(parsed.to, "bbb222");
        assert_eq!(parsed.change, "ccc333");
    }

    #[test]
    fn a_line_that_is_not_well_formed_is_ignored() {
        assert!(parse_provenance("X-Skot-Import: too few fields").is_none());
        assert!(parse_provenance("X-Skot-Import: ").is_none());
        assert!(parse_provenance("Some other header").is_none());
    }
}
