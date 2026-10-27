//! The git engine.
//!
//! `git2` (libgit2) is the only git implementation skot uses. Nothing shells out to
//! `git`, so skot works where git is not installed.
//!
//! The choice matters: libgit2 already implements patch application, worktree
//! creation, checkout and diff generation. skot calls those rather than
//! reimplementing them, which is the whole point of embedding a git engine.

pub mod binary;
pub mod diff;
pub mod mirror;
pub mod sandbox;

use std::path::Path;

use anyhow::{Context, Result};

/// A commit id, kept as a string because it crosses the JSON boundary.
pub type Sha = String;

/// Resolve `name` against `repo`, preferring a tag over a branch of the same name.
///
/// Deliberately *not* doing anything clever: a ref either resolves or it does not, and
/// saying so is more useful than guessing.
pub fn resolve_ref(repo: &git2::Repository, name: &str) -> Result<Sha> {
    // A commit expression first: neither `refs/tags/` nor `refs/heads/` means anything
    // to it, and a miss here is about the history behind the expression rather than
    // about spelling, so it says that with libgit2's own detail underneath.
    if mirror::is_commit_expression(name) {
        return match repo.revparse_single(name) {
            Ok(obj) => match obj.peel(git2::ObjectType::Commit) {
                Ok(commit) => Ok(commit.id().to_string()),
                Err(err) => Err(anyhow::Error::new(err)
                    .context(format!("`{name}` resolves, but not to a commit"))),
            },
            Err(err) => Err(anyhow::Error::new(err).context(format!(
                "`{name}` is a commit expression, and the mirror does not hold the \
                 history behind it.\n\
                 Fetch the ref it reads from (`{}`), or pass a ref.",
                crate::invocation::skot_args("sync", mirror::commit_expression_base(name))
            ))),
        };
    }
    for candidate in [format!("refs/tags/{name}"), format!("refs/heads/{name}")] {
        if let Some(id) = try_commit(repo, &candidate) {
            return Ok(id);
        }
    }
    // A raw commit id, for `check <sha>`.
    if let Some(id) = try_commit(repo, name) {
        return Ok(id);
    }
    if mirror::is_commit_id(name) {
        anyhow::bail!(
            "`{name}` is not in the mirror, and a commit id cannot be fetched by name.\n\
             Sync the branch or tag that contains it."
        )
    }
    anyhow::bail!(
        "`{name}` is not in the mirror.\n\
         Run `{} {name}` to fetch it, if it is new upstream.",
        crate::invocation::skot("sync")
    )
}

pub(crate) fn try_commit(repo: &git2::Repository, rev: &str) -> Option<Sha> {
    repo.revparse_single(rev)
        .ok()?
        .peel(git2::ObjectType::Commit)
        .ok()
        .map(|o| o.id().to_string())
}

/// Every ref in the repo, grouped for display.
pub fn refs(repo: &git2::Repository) -> Result<Refs> {
    let mut out = Refs::default();
    for reference in repo.references()? {
        // A broken ref should not abort the listing; one bad entry in `.git/refs` is
        // not a reason `skot status` cannot tell the user what is there.
        let Ok(reference) = reference else { continue };
        let sha = reference.target().map(|id| id.to_string());
        // A reference whose name cannot be read is one bad entry in `.git/refs`, not a reason
        // to abandon the listing.
        let Ok(name) = reference.name() else { continue };
        match name {
            n if n.starts_with("refs/heads/") => out
                .branches
                .push((n["refs/heads/".len()..].to_owned(), sha)),
            n if n.starts_with("refs/tags/") => {
                out.tags.push((n["refs/tags/".len()..].to_owned(), sha))
            }
            n => out.other.push((n.to_owned(), sha)),
        }
    }
    out.branches.sort();
    out.tags.sort();
    out.other.sort();
    Ok(out)
}

#[derive(Debug, Default)]
pub struct Refs {
    pub branches: Vec<(String, Option<Sha>)>,
    pub tags: Vec<(String, Option<Sha>)>,
    pub other: Vec<(String, Option<Sha>)>,
}

/// The tree of the commit `rev` points at.
pub fn commit_tree<'a>(repo: &'a git2::Repository, rev: &str) -> Result<git2::Tree<'a>> {
    let obj = repo
        .revparse_single(rev)
        .context(format!("resolving {rev}"))
        .and_then(|o| {
            o.peel_to_commit()
                .map_err(anyhow::Error::from)
                .context(format!("{rev} is not a commit"))
        })?;
    obj.tree().context(format!("reading the tree of {rev}"))
}

/// Is `path` inside a repository?
pub fn is_repo(path: &Path) -> bool {
    git2::Repository::discover(path).is_ok()
}
