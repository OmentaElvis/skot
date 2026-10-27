//! Shared plumbing for the authoring commands: `patch new`, `patch save`,
//! `patch push`, `patch pop`, and `rebase`.
//!
//! These five all move between three states: the series on disk, the sandbox working tree,
//! and the recorded state. Each of them has to work out the same three things first:
//! which ref the sandbox is on, which series entries are in it, and what tree the sandbox
//! should be diffed against. Getting that in one place is what keeps `push` from silently
//! diffing against a base that `apply` would never have produced.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::check;
use crate::config::Config;
use crate::git::{Sha, diff, sandbox};
use crate::invocation::{skot, skot_args};
use crate::layout::Layout;
use crate::series::Entry;
use crate::state::State;

/// Resolve the ref a command should work against: the argument, then the series' base,
/// then the configured default, then the mirror's own HEAD.
///
/// `command_line` is what the advice tells the user to run, already resolved through
/// [`crate::invocation`] so a command that moves updates this message too, and `target`
/// is which command the fallback note is attributed to. Kept as plain scalars so this
/// stays callable from tests without a clap `Ctx` behind it.
pub fn wanted_ref(
    layout: &Layout,
    ref_name: Option<&str>,
    config: &Config,
    command_line: &str,
    target: &'static str,
) -> Result<String> {
    ref_name
        .map(str::to_owned)
        .or_else(|| layout.base_ref())
        .or_else(|| config.upstream.default_ref.clone())
        .or_else(|| super::mirror_head_ref(layout, target))
        .context(format!(
            "no ref to work against.\n\
             Pass one (as in `{command_line} <ref>`), record a base for this series, \
             or set `upstream.default-ref` in {}.",
            layout.config_file().display()
        ))
}

/// Replay `entries` against `base` in the sandbox repository, without checking anything out.
///
/// `push`, `pop` and `rebase` all need "what tree does this set of patches produce" and none
/// of them want a checkout, so they share this rather than each calling `replay_series` and
/// remembering to handle the failing case.
pub fn replay(
    repo: &git2::Repository,
    base: &Sha,
    entries: &[Entry],
    blobs: &Path,
) -> Result<check::Applied> {
    check::replay_series(repo, base, entries, blobs).context(format!(
        "replaying {} patch(es) against {base}",
        entries.len()
    ))
}

/// Check the sandbox is present and readable, with a message that says how to get there.
pub fn require_sandbox(layout: &Layout) -> Result<git2::Repository> {
    if !sandbox::is_repository(layout) {
        bail!(
            "no sandbox at {}.\n\
             Run `{}` first.",
            layout.sandbox_dir().display(),
            skot("apply")
        );
    }
    sandbox::open(layout)
}

/// Refuse to touch a sandbox that belongs to another series.
///
/// One shared sandbox holds one series at a time. After a `keep` switch the tree still
/// holds the series you left, and pushing or popping would land this series' patches on
/// top of that series' state. `apply` is the command that takes the sandbox over; until
/// it runs, stack commands wait.
pub fn require_holder(layout: &Layout, state: &State) -> Result<()> {
    if layout.per_series_sandbox() {
        return Ok(());
    }
    let Some(holder) = state.sandbox_holder(layout) else {
        return Ok(());
    };
    if holder == layout.series() {
        return Ok(());
    }
    bail!(
        "the sandbox holds `{}`, not `{}`.\n\
         `{}` takes it over for the series you are on.",
        holder,
        layout.series(),
        skot("apply")
    );
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
/// Preserving the existing header is the point: a patch that is re-saved with a re-stamped
/// author makes every rebase show up as a diff in the series.
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
///
/// The stem read as words: a name is the patch's own, so this just drops the extension
/// and separates what the separators separate. An existing `Subject:` header always
/// wins over this.
pub fn subject_from_name(name: &str) -> String {
    let stem = name.rsplit('/').next().unwrap_or(name);
    let without_ext = stem.strip_suffix(".patch").unwrap_or(stem);
    without_ext.replace(['-', '_'], " ")
}

/// The ref a stack command should work against.
///
/// For `push` and `pop` this is the ref the sandbox is *already* on, not one from the command
/// line: those commands take a patch name, and replaying a stack against a different ref than
/// the one it was built on would produce a tree the user never asked for. So the recorded ref
/// wins, with the series' base and then the configured default as the fallbacks for a
/// sandbox that was never applied.
pub fn stack_ref(layout: &Layout, state: &State, config: &Config) -> Result<String> {
    state
        .ref_name(layout)
        .map(str::to_owned)
        .or_else(|| layout.base_ref())
        .or_else(|| config.upstream.default_ref.clone())
        .context(format!(
            "cannot tell which ref the sandbox is on.\n\
             `.skot/state.json` records none, this series has no base, and \
             `upstream.default-ref` is unset in {}.\n\
             Run `{} <ref>` first.",
            layout.config_file().display(),
            skot("apply")
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

/// The set a command should leave applied in the sandbox: an ordered subset of the series.
#[derive(Debug)]
pub struct Stack {
    /// The entries to replay, which is also what the sandbox will hold.
    pub entries: Vec<Entry>,
    /// Their names, for messages and for the state record.
    pub names: Vec<String>,
}

/// The recorded set must at least be a set of names the series still knows: a name that
/// has been deleted from the series (or a corrupt double-entry) makes every command
/// guess, so it is refused here with the command that repairs it.
fn validate_set(all: &[Entry], applied: &[String], series: &crate::series::Series) -> Result<()> {
    for (i, recorded) in applied.iter().enumerate() {
        if applied[..i].contains(recorded) {
            bail!(
                "the sandbox records `{recorded}` twice.\n\
                 The recorded set at .skot/state.json is corrupt.\n\
                 `{}` then `{}` rebuilds it.",
                skot("unapply"),
                skot("apply"),
            );
        }
        if !all.iter().any(|e| &e.name == recorded) {
            bail!(
                "the sandbox holds `{recorded}`, which the series at {} does not list.\n\
                 The recorded set and the series have drifted apart, so no `push`, \
                 `pop` or `build` can tell which patches you meant.\n\
                 `{}` reports the same; `{}` then `{}` \
                 rebuilds the set",
                series.path().display(),
                skot("status"),
                skot("unapply"),
                skot("apply")
            );
        }
    }
    Ok(())
}

/// The set exactly as recorded: no push, no pop, what the sandbox already holds.
///
/// `build` replays this rather than the whole series, so a patch popped out for testing
/// stays out through a build. Same membership validation as [`resolve_stack`], because a
/// recorded name the series no longer lists means nothing to any command.
pub fn recorded_stack(
    layout: &Layout,
    series: &crate::series::Series,
    state: &State,
) -> Result<Stack> {
    let all = series.entries_lenient(layout);
    validate_set(&all, state.applied(layout), series)?;
    let entries: Vec<Entry> = all
        .into_iter()
        .filter(|e| state.applied(layout).contains(&e.name))
        .collect();
    let names = entries.iter().map(|e| e.name.clone()).collect();
    Ok(Stack { entries, names })
}

/// Work out the set that `push` or `pop` should leave applied.
///
/// The rule behind both commands: **the applied set is an ordered subset of the series.**
/// Any patch may be on or off (disabling b while keeping c on a is legitimate),
/// and the set always replays in series order. What the commands refuse to do is *guess*
/// which patch the user meant to touch; both stay directional, quilt-style:
///
/// - `push <name>`: enable `name` and every patch below it (heal the set up to it),
///   keeping whatever is applied above.
/// - `pop <name>`: disable `name` and every patch above it (cut the set from it up).
/// - `push` (no name): enable the first patch above the current top, leaving
///   deliberate holes below untouched.
/// - `pop` (no name): disable the topmost applied patch.
///
/// The outcome is replayed in series order and checked out, so a hole in the middle only
/// ever fails when the patches genuinely do not apply without it. That is the lesson
/// the failure teaches, not a reason to refuse the set.
/// How a `[PATCH]` argument resolved: to a patch's name, or to a 1-based position
/// among the enabled patches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchRef {
    /// An exact name, or a unique prefix of one, such as `fix`.
    Name(String),
    /// A 1-based position among the enabled patches. A disabled patch has no position.
    Position(usize),
}

impl PatchRef {
    /// The patch's name: itself, or the enabled entry the position points at.
    pub fn name(self, enabled: &[&str]) -> String {
        match self {
            PatchRef::Name(name) => name,
            PatchRef::Position(position) => enabled[position - 1].to_owned(),
        }
    }
}

/// Resolve a patch reference the way every `[PATCH]` argument reads one.
///
/// An exact name wins, then a unique name prefix such as `fix`, then a 1-based position
/// among the enabled patches. `all` is every name in the series, enabled or not, so a
/// disabled patch can still be named; `enabled` is the position space, since a disabled
/// patch has no position.
pub fn resolve_patch(all: &[&str], enabled: &[&str], want: &str) -> Result<PatchRef> {
    if all.contains(&want) {
        return Ok(PatchRef::Name(want.to_owned()));
    }
    let matches: Vec<&str> = all
        .iter()
        .copied()
        .filter(|n| n.starts_with(want))
        .collect();
    match matches.as_slice() {
        [one] => return Ok(PatchRef::Name((*one).to_owned())),
        [] => {}
        many => bail!(
            "`{want}` matches more than one patch: {}.\nName one in full.",
            many.join(", ")
        ),
    }
    if let Ok(position) = want.parse::<usize>() {
        if position == 0 {
            bail!("positions are 1-based: 1 is the front of the series");
        }
        if position > enabled.len() {
            bail!(
                "the series holds {} patches, so position {position} does not exist. \
                 Patches in it: {}",
                enabled.len(),
                if enabled.is_empty() {
                    "(none)".to_owned()
                } else {
                    enabled.join(", ")
                }
            );
        }
        return Ok(PatchRef::Position(position));
    }
    bail!(
        "`{want}` is not in the series. Patches in it: {}",
        if all.is_empty() {
            "(none)".to_owned()
        } else {
            all.join(", ")
        }
    )
}

pub fn resolve_stack(
    layout: &Layout,
    series: &crate::series::Series,
    state: &State,
    want: Option<&str>,
    move_: Move,
) -> Result<Stack> {
    let all = series.entries_lenient(layout);
    validate_set(&all, state.applied(layout), series)?;

    let applied = state.applied(layout);
    let mut on: Vec<bool> = all.iter().map(|e| applied.contains(&e.name)).collect();
    // Everything above the top is off by construction, so `push` with no name moves to
    // exactly one patch: the next series entry after the topmost applied one.
    let top = on.iter().rposition(|&b| b);

    match want {
        Some(want) => {
            // The reference can be a name, a prefix or a position; a disabled patch
            // resolves too, and lands in the refusal below rather than in a series
            // the sandbox would disagree with.
            let names: Vec<&str> = all.iter().map(|e| e.name.as_str()).collect();
            let disabled: Vec<String> = series.disabled();
            let mut universe: Vec<&str> = names.clone();
            universe.extend(disabled.iter().map(String::as_str));
            let name = resolve_patch(&universe, &names, want)?.name(&names);

            let Some(position) = all.iter().position(|e| e.name == name) else {
                if disabled.iter().any(|d| d == &name) {
                    bail!(
                        "`{name}` is disabled, so it is not part of what applies.\n`{}` \
                         puts it back in.",
                        skot_args("enable", &name)
                    );
                }
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

            match move_ {
                Move::Deeper => {
                    if on[position] {
                        bail!(
                            "`{}` is already applied.\n\
                             `{}` takes it back off.",
                            all[position].name,
                            skot_args("pop", &all[position].name)
                        );
                    }
                    // Enable it and everything below: a patch cannot stand alone, so pushing
                    // a named patch fills the holes under it (but never the ones above).
                    for slot in on.iter_mut().take(position + 1) {
                        *slot = true;
                    }
                }
                Move::Shallower => {
                    if !on[position] {
                        bail!(
                            "`{}` is not applied, so there is nothing to pop.",
                            all[position].name
                        );
                    }
                    // Cut from it up: the patches above were enabled after it and stand on it.
                    for slot in on.iter_mut().skip(position) {
                        *slot = false;
                    }
                }
            }
        }
        None => match move_ {
            Move::Deeper => {
                let next = top.map_or(0, |t| t + 1);
                if next >= all.len() {
                    bail!(
                        "the applied set already reaches the end of the series, so there \
                         is no next patch.\n\
                         `{}` chooses which patches are on; `{}` \
                         starts another.",
                        skot("apply"),
                        skot_args("new", "<name>")
                    );
                }
                on[next] = true;
            }
            Move::Shallower => {
                let Some(t) = top else {
                    bail!("nothing is applied to the sandbox, so there is nothing to pop.");
                };
                // No name means the topmost applied patch; holes below it stay.
                on[t] = false;
            }
        },
    }

    let entries: Vec<Entry> = all
        .into_iter()
        .enumerate()
        .filter(|(i, _)| on[*i])
        .map(|(_, e)| e)
        .collect();
    let names = entries.iter().map(|e| e.name.clone()).collect();
    Ok(Stack { entries, names })
}

/// Replay `stack` onto `base`, check it out, and record it as the sandbox's contents.
///
/// One function because the order matters and `push` and `pop` must not disagree about it:
/// replay first, so a patch that does not apply leaves the sandbox alone; check out second,
/// so the tree and the index never disagree; record third, so `status` and `pop`
/// see the same stack the sandbox actually holds.
pub fn land(layout: &Layout, ref_name: &str, base: &Sha, stack: &Stack) -> Result<git2::Oid> {
    let repo = require_sandbox(layout)?;
    let applied = replay(&repo, base, &stack.entries, &layout.blobs_dir())?;

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

    const THREE: [&str; 3] = ["a.patch", "b.patch", "c.patch"];

    #[test]
    fn an_exact_name_resolves_to_itself() {
        let got = resolve_patch(&THREE, &THREE, "b.patch").unwrap();
        assert_eq!(got, PatchRef::Name("b.patch".to_owned()));
    }

    #[test]
    fn a_name_prefix_resolves_to_its_patch() {
        let got = resolve_patch(&THREE, &THREE, "b").unwrap();
        assert_eq!(got, PatchRef::Name("b.patch".to_owned()));
    }

    #[test]
    fn a_bare_number_is_a_position_among_the_enabled() {
        let got = resolve_patch(&THREE, &THREE, "2").unwrap();
        assert_eq!(got, PatchRef::Position(2));
        assert_eq!(got.name(&THREE), "b.patch");
    }

    #[test]
    fn a_prefix_matching_more_than_one_patch_is_refused_with_the_candidates() {
        let many = ["fix-a.patch", "fix-b.patch", "other.patch"];
        let err = resolve_patch(&many, &many, "fix").unwrap_err().to_string();
        assert!(err.contains("more than one"), "{err}");
        assert!(err.contains("fix-a.patch"), "{err}");
        assert!(err.contains("fix-b.patch"), "{err}");
    }

    #[test]
    fn a_zero_position_is_refused_as_not_one_based() {
        let plain = ["a.patch", "b.patch"];
        let err = resolve_patch(&plain, &plain, "0").unwrap_err().to_string();
        assert!(err.contains("1-based"), "{err}");
    }

    #[test]
    fn a_position_past_the_enabled_set_lists_what_is_there() {
        let err = resolve_patch(&THREE, &THREE, "4").unwrap_err().to_string();
        assert!(err.contains("does not exist"), "{err}");
        assert!(err.contains("c.patch"), "{err}");
    }

    #[test]
    fn an_unknown_reference_lists_every_name_including_the_disabled() {
        let all = ["a.patch", "b.patch", "c.patch"];
        let enabled = ["a.patch", "c.patch"];
        let err = resolve_patch(&all, &enabled, "nope.patch")
            .unwrap_err()
            .to_string();
        assert!(err.contains("is not in the series"), "{err}");
        assert!(err.contains("b.patch"), "{err}");
    }

    #[test]
    fn a_disabled_patch_can_still_be_named() {
        let all = ["a.patch", "b.patch", "c.patch"];
        let enabled = ["a.patch", "c.patch"];
        let got = resolve_patch(&all, &enabled, "b").unwrap();
        assert_eq!(got, PatchRef::Name("b.patch".to_owned()));
    }

    #[test]
    fn recorded_stack_is_the_recorded_set_not_a_prefix() {
        // With holes legal, the set is what was recorded, in series order: not the
        // first n entries of the series.
        let (_tmp, layout) = crate::commands::testsupport::project();
        crate::commands::testsupport::write_series(
            &layout,
            &[("a.patch", ""), ("b.patch", ""), ("c.patch", "")],
        );
        let series = layout.project().unwrap().1;
        let mut state = State::default();
        state
            .set_applied(&layout, vec!["a.patch".to_owned(), "c.patch".to_owned()])
            .unwrap();

        let stack = recorded_stack(&layout, &series, &state).unwrap();
        assert_eq!(stack.names, ["a.patch", "c.patch"]);
    }

    #[test]
    fn a_subject_is_derived_from_a_patch_filename() {
        assert_eq!(
            subject_from_name("trim-the-widgets.patch"),
            "trim the widgets"
        );
        assert_eq!(subject_from_name("sub/fix-thing.patch"), "fix thing");
        assert_eq!(subject_from_name("no-number-here.patch"), "no number here");
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
        // An empty patch file has no header, and `save` has to turn it into a real patch.
        let sig = git2::Signature::now("A U Thor", "author@example.com").unwrap();
        let header = header_for(Some("diff --git a/a b/a\n"), "fallback", &sig);
        assert_eq!(header.subject(), "fallback");
        assert!(header.author.is_some());
    }

    #[test]
    fn a_stack_command_waits_until_the_sandbox_is_its_series() {
        let (_tmp, layout) = crate::commands::testsupport::project();
        std::fs::create_dir_all(layout.series_root().join("exp")).unwrap();
        let mut state = State::load(&layout).unwrap();
        state
            .set_applied(&layout, vec!["a.patch".to_owned()])
            .unwrap();

        // The holder is the series the sandbox was applied for.
        require_holder(&layout, &state).unwrap();

        // After a keep switch the tree still holds the old series, so the new one waits.
        let exp = layout.with_series("exp").unwrap();
        let err = require_holder(&exp, &state).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("the sandbox holds `default`, not `exp`"),
            "got {text}"
        );
        assert!(text.contains(&skot("apply")), "got {text}");

        // Per-series mode: every series owns its tree, so there is nothing to wait for.
        let per = layout.with_per_series_sandbox(true);
        require_holder(&per, &state).unwrap();
        let per_exp = exp.with_per_series_sandbox(true);
        require_holder(&per_exp, &state).unwrap();
    }
}
