//! Shared plumbing for the authoring commands: `new`, `refresh`, `push`, `pop`, `rebase`.
//!
//! These five all move between three states — the series on disk, the sandbox working tree,
//! and the recorded state — and each of them has to work out the same three things first:
//! which ref the sandbox is on, which series entries are in it, and what tree the sandbox
//! should be diffed against. Getting that in one place is what keeps `refresh` from silently
//! diffing against a base that `apply` would never have produced.

use anyhow::{Context, Result, bail};

use crate::check;
use crate::config::Config;
use crate::git::{Sha, diff, sandbox};
use crate::layout::Layout;
use crate::series::Entry;
use crate::state::State;

/// Resolve the ref a command should work against: the argument, then the configured default.
///
/// `command` is the name the advice should show (`"rebase"`), kept as a plain scalar so this
/// stays callable from tests without a clap `Ctx` behind it.
pub fn wanted_ref(
    layout: &Layout,
    ref_name: Option<&str>,
    config: &Config,
    command: &'static str,
) -> Result<String> {
    ref_name
        .map(str::to_owned)
        .or_else(|| config.upstream.default_ref.clone())
        .context(format!(
            "no ref to work against.\n\
             Pass one (as in `skot {command} <ref>`) or set `upstream.default-ref` in {}.",
            layout.config_file().display()
        ))
}

/// The tree that the patch at `index` sits on top of: the series prefix *before* it.
///
/// This is the base a patch has to be expressed against, and it is deliberately not the
/// tree after the whole stack:
///
/// ```text
/// tree(ref + p0..=pn)      <- what `apply` puts in the sandbox, after the last patch
/// sandbox                  = that, plus your edits
/// ```
///
/// Diffing the sandbox against *that* yields *only your edits*, so replacing the patch with
/// the result silently throws away everything the patch originally did — the patch looks
/// plausible and the series no longer applies. The patch's own base is the prefix below it:
///
/// ```text
/// tree(ref + p0..p(n-1))                  <- what p(n-1) leaves behind
/// sandbox - that = p(n)'s own change, plus your edits
/// ```
///
/// For a patch `new` has just created and `push` has applied, the two trees are equal, because
/// an empty patch produces no change — so the same expression correctly yields just your edits.
///
/// It is recomputed by replaying the prefix rather than read from recorded state, which is
/// the point: a recorded tree is a claim about a sandbox that has since been edited, and a
/// stale claim would silently produce a patch that drops earlier work.
pub fn base_tree_for_patch<'r>(
    layout: &Layout,
    state: &State,
    config: &Config,
    repo: &'r git2::Repository,
    entries: &[Entry],
    index: usize,
) -> Result<git2::Tree<'r>> {
    let ref_name = stack_ref(layout, state, config)?;
    let (_mirror, sha) = super::sync::resolve_in_mirror(layout, &ref_name)?;

    let prefix = &entries[..index];
    if prefix.is_empty() {
        let oid = git2::Oid::from_str(&sha)
            .context(format!("the resolved ref {ref_name} is not a commit id"))?;
        let commit = repo.find_commit(oid)?;
        return Ok(commit.tree()?);
    }

    let applied = replay(repo, &sha, prefix)?;
    if !applied.report.ok {
        bail!(
            "cannot work out what `{}` sits on, because the patches below it do not apply \
             cleanly.\n\
             {}",
            entries[index].name,
            applied.report.summary()
        );
    }
    Ok(repo.find_tree(applied.tree)?)
}

/// Replay `entries` against `base` in the sandbox repository, without checking anything out.
///
/// `push`, `pop` and `rebase` all need "what tree does this set of patches produce" and none
/// of them want a checkout, so they share this rather than each calling `replay_series` and
/// remembering to handle the failing case.
pub fn replay(repo: &git2::Repository, base: &Sha, entries: &[Entry]) -> Result<check::Applied> {
    check::replay_series(repo, base, entries).context(format!(
        "replaying {} patch(es) against {base}",
        entries.len()
    ))
}

/// Check the sandbox is present and readable, with a message that says how to get there.
pub fn require_sandbox(layout: &Layout) -> Result<git2::Repository> {
    if !sandbox::is_repository(layout) {
        bail!(
            "no sandbox at {}.\n\
             Run `skot apply` first.",
            layout.sandbox_dir().display()
        );
    }
    sandbox::open(layout)
}

/// The signature to stamp a brand-new patch with.
///
/// Taken from the mirror's config where it has one, so the author of a new patch is whoever
/// git already knows is the author, rather than whatever the environment happens to contain.
pub fn signature(repo: &git2::Repository) -> git2::Signature<'static> {
    repo.signature()
        .or_else(|_| git2::Signature::now("skot", "skot@localhost"))
        .expect("a signature is always constructible")
}

/// Read the header of an existing patch file, or make one for a new patch.
///
/// Preserving the existing header is the point: a refreshed patch that re-stamps the author
/// makes every rebase show up as a diff in the series.
pub fn header_for(
    existing: Option<&str>,
    fallback_subject: &str,
    sig: &git2::Signature<'_>,
) -> diff::Header {
    match existing.and_then(diff::Header::parse) {
        Some(header) => header,
        None => diff::Header::signed(fallback_subject, sig),
    }
}

/// The subject to use for a patch named `name`, when the file has no header of its own.
pub fn subject_from_name(name: &str) -> String {
    let stem = name.rsplit('/').next().unwrap_or(name);
    let without_ext = stem.strip_suffix(".patch").unwrap_or(stem);
    // `0001-trim-the-widgets` reads as "trim the widgets" once the number and extension are
    // gone. This is a default, not a rule: an existing `Subject:` header always wins.
    without_ext
        .split_once('-')
        .map(|(_, rest)| rest.replace('-', " "))
        .unwrap_or_else(|| without_ext.replace('_', " "))
}

/// The ref a stack command should work against.
///
/// For `push` and `pop` this is the ref the sandbox is *already* on, not one from the command
/// line: those commands take a patch name, and replaying a stack against a different ref than
/// the one it was built on would produce a tree the user never asked for. So the recorded ref
/// wins, with the configured default as the fallback for a sandbox that was never applied.
pub fn stack_ref(layout: &Layout, state: &State, config: &Config) -> Result<String> {
    state
        .ref_name
        .clone()
        .or_else(|| config.upstream.default_ref.clone())
        .context(format!(
            "cannot tell which ref the sandbox is on.\n\
             `.skot/state.json` records none and `upstream.default-ref` is unset in {}.\n\
             Run `skot apply <ref>` first.",
            layout.config_file().display()
        ))
}

/// Which way a stack command moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move {
    /// `push`: one patch deeper.
    Deeper,
    /// `pop`: one patch shallower.
    Shallower,
}

/// The stack a command should leave in the sandbox: a prefix of the series, in order.
#[derive(Debug)]
pub struct Stack {
    /// The entries to replay, which is also what the sandbox will hold.
    pub entries: Vec<Entry>,
    /// Their names, for messages and for the state record.
    pub names: Vec<String>,
}

/// Work out the stack that `push` or `pop` should produce.
///
/// The rule behind both commands is the same one: **a stack is always a prefix of the series.**
/// So `push` cannot apply one file in isolation — patch 3 is only correct on top of 1 and 2 —
/// and `pop` cannot revert one file in isolation either, since removing a patch from the middle
/// of a stack means re-deriving everything below it. Both therefore end by replaying a prefix
/// and checking that out, and the difference between them is only which prefix.
///
/// Every refusal here is a case where the answer would otherwise be a guess. A guessed stack is
/// a corrupt one: it looks like a valid prefix, passes `check`, and builds the wrong thing.
/// The recorded stack has to be a prefix of the series before any of this means anything.
fn validate_prefix(
    all: &[Entry],
    applied: &[String],
    series: &crate::series::Series,
) -> Result<()> {
    if applied.len() > all.len() {
        bail!(
            "the sandbox holds {} patch(es), but the series at {} lists only {}.\n\
             `skot unapply` then `skot apply` puts the two back in step.",
            applied.len(),
            series.path().display(),
            all.len()
        );
    }
    for (recorded, entry) in applied.iter().zip(all.iter()) {
        if recorded != &entry.name {
            bail!(
                "the sandbox holds `{}` where the series has `{}`.\n\
                 The recorded stack is not a prefix of the series at {}, so no `push`, \
                 `pop` or `build` can tell which tree you meant.\n\
                 `skot status` reports the same; `skot unapply` then `skot apply` \
                 rebuilds the stack.",
                recorded,
                entry.name,
                series.path().display()
            );
        }
    }
    Ok(())
}

/// The stack exactly as recorded: no push, no pop, the prefix the sandbox already holds.
///
/// `build` replays this rather than the whole series, so a patch popped out for testing
/// stays out through a build. Same prefix validation as [`resolve_stack`], because a
/// recorded stack that is not a prefix of the series means nothing to any command.
pub fn recorded_stack(
    layout: &Layout,
    series: &crate::series::Series,
    state: &State,
) -> Result<Stack> {
    let all = series.entries_lenient(layout);
    validate_prefix(&all, &state.applied, series)?;
    let entries = all[..state.applied.len()].to_vec();
    let names = entries.iter().map(|e| e.name.clone()).collect();
    Ok(Stack { entries, names })
}

pub fn resolve_stack(
    layout: &Layout,
    series: &crate::series::Series,
    state: &State,
    want: Option<&str>,
    move_: Move,
) -> Result<Stack> {
    let all = series.entries_lenient(layout);
    validate_prefix(&all, &state.applied, series)?;

    let applied = &state.applied;
    let growing = move_ == Move::Deeper;

    let end = match want {
        Some(name) => {
            let Some(position) = all.iter().position(|e| e.name == name) else {
                let known: Vec<&str> = all.iter().map(|e| e.name.as_str()).collect();
                bail!(
                    "`{name}` is not in the series. Patches in it: {}",
                    if known.is_empty() {
                        "(none)".to_owned()
                    } else {
                        known.join(", ")
                    }
                );
            };
            let is_on_the_stack = applied.len() > position;

            match (growing, is_on_the_stack) {
                (true, true) => bail!(
                    "`{name}` is already applied — the stack stops there.\n\
                     `skot pop` takes the patch on top back off.",
                    name = all[position].name
                ),
                (false, false) => bail!(
                    "`{}` is not applied, so there is nothing to pop.",
                    all[position].name
                ),
                (true, false) => position + 1,
                // Popping patch `position` leaves everything above it gone, so the stack ends
                // where that patch began.
                (false, true) => position,
            }
        }
        None => {
            if growing {
                if applied.len() == all.len() {
                    bail!(
                        "the whole series is applied, so there is no next patch.\n\
                         `skot new <name>` starts another."
                    );
                }
                applied.len() + 1
            } else {
                if applied.is_empty() {
                    bail!("nothing is applied to the sandbox, so there is nothing to pop.");
                }
                applied.len() - 1
            }
        }
    };

    let entries = all[..end].to_vec();
    let names = entries.iter().map(|e| e.name.clone()).collect();
    Ok(Stack { entries, names })
}

/// Replay `stack` onto `base`, check it out, and record it as the sandbox's contents.
///
/// One function because the order matters and `push` and `pop` must not disagree about it:
/// replay first, so a patch that does not apply leaves the sandbox alone; check out second,
/// so the tree and the index never disagree; record third, so `status`, `pop` and `refresh`
/// see the same stack the sandbox actually holds.
pub fn land(layout: &Layout, ref_name: &str, base: &Sha, stack: &Stack) -> Result<git2::Oid> {
    let repo = require_sandbox(layout)?;
    let applied = replay(&repo, base, &stack.entries)?;

    if !applied.report.ok {
        bail!(
            "{} does not apply to {ref_name}.\n{}\n\
             The sandbox was left on the tree it already had.",
            applied
                .report
                .first_failure()
                .map(|p| p.name.as_str())
                .unwrap_or("the series"),
            applied.report.summary()
        );
    }

    crate::git::sandbox::checkout_tree(&repo, applied.tree)?;

    let mut state = State::load(layout)?;
    state.set_ref(layout, ref_name, base.as_str())?;
    state.set_applied(layout, stack.names.clone())?;
    Ok(applied.tree)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subject_is_derived_from_a_patch_filename() {
        assert_eq!(
            subject_from_name("0001-trim-the-widgets.patch"),
            "trim the widgets"
        );
        assert_eq!(subject_from_name("sub/0002-fix-thing.patch"), "fix thing");
        assert_eq!(
            subject_from_name("0003-no-number-here.patch"),
            "no number here"
        );
        assert_eq!(subject_from_name("loose.patch"), "loose");
    }

    #[test]
    fn an_existing_header_wins_over_the_filename() {
        let sig = git2::Signature::now("a", "a@b").unwrap();
        let existing = "From: Real Person <real@example.com>\nSubject: [PATCH] the real title\n\n---\n\ndiff --git a/a b/a\n";
        let header = header_for(Some(existing), "derived from name", &sig);
        assert_eq!(header.subject(), "the real title");
        assert_eq!(
            header.author.as_deref(),
            Some("Real Person <real@example.com>")
        );
    }

    #[test]
    fn a_patch_with_no_header_gets_the_filename_subject_and_the_signature() {
        let sig = git2::Signature::now("A U Thor", "author@example.com").unwrap();
        let header = header_for(None, "trim widgets", &sig);
        assert_eq!(header.subject(), "trim widgets");
        assert!(
            header
                .author
                .is_some_and(|a| a.contains("author@example.com"))
        );
    }

    #[test]
    fn a_header_from_a_bare_diff_is_stamped_rather_than_ignored() {
        // An empty patch file has no header, and `refresh` has to turn it into a real patch.
        let sig = git2::Signature::now("A U Thor", "author@example.com").unwrap();
        let header = header_for(Some("diff --git a/a b/a\n"), "fallback", &sig);
        assert_eq!(header.subject(), "fallback");
        assert!(header.author.is_some());
    }
}
