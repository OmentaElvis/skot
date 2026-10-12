//! Generating patches: tree and working tree -> a `format-patch` file.
//!
//! This is the other half of [`crate::check`]. `check` consumes a patch file and applies
//! it to a tree; this module produces one by diffing a tree against what the user has in
//! the sandbox. `edit-save` and `rebase` both need it, and neither writes patch text by hand,
//! because a patch file is a structured format that libgit2 already knows how to write.
//!
//! Two details that are easy to get wrong and expensive to debug:
//!
//! - **Untracked files must be included.** A patch series is where new files *start*, so a
//!   diff that only sees tracked files silently drops every file the user just created.
//!   `include_untracked` and `recurse_untracked_dirs` together are what make a new
//!   directory appear in the output rather than as an empty entry.
//! - **Author and date are preserved from the patch being replaced.** A re-saved patch that
//!   re-stamps the committer turns every rebase into a diff in the series' history. The
//!   mail header is parsed out of the existing patch and re-emitted verbatim; only the diff
//!   body is regenerated.

use std::path::Path;

use anyhow::{Context, Result};

/// The mail header of a `format-patch` file: everything before the diff.
///
/// Kept as its own type so that "re-export this patch with a new body" cannot accidentally
/// re-stamp the author, which is the failure mode that makes a re-saved series churn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// The `From <sha> Mon Sep 17 00:00:00 2001` line, preserved verbatim.
    pub from_line: Option<String>,
    pub author: Option<String>,
    pub date: Option<String>,
    /// Without the `[PATCH]` prefix.
    pub subject: Option<String>,
    /// Header lines this type does not model, kept verbatim and re-emitted in order.
    ///
    /// The mail format allows headers beyond the four above, and dropping one would be
    /// silent data loss: `import` records where a patch came from in an `X-Skot-Import`
    /// line, which is how a later run recognises the same request. `edit-save` parses the
    /// existing header before re-exporting, so anything a parser forgot here would
    /// disappear from the patch the first time it was saved.
    pub extra: Vec<String>,
    /// Anything after the headers and before the `---` separator: the commit body.
    pub body: String,
}

impl Header {
    /// A header for a brand-new patch.
    pub fn new(subject: &str) -> Self {
        Self {
            from_line: None,
            author: None,
            date: None,
            subject: Some(subject.to_owned()),
            extra: Vec::new(),
            body: String::new(),
        }
    }

    /// Take the header from `signature`, stamping it as a new commit would.
    pub fn signed(subject: &str, signature: &git2::Signature<'_>) -> Self {
        let when = signature.when();
        Self {
            from_line: Some(format!(
                "From {} Mon Sep 17 00:00:00 2001\n",
                when.seconds()
            )),
            author: Some(signature.to_string()),
            date: Some(rfc2822(when.seconds())),
            subject: Some(subject.to_owned()),
            extra: Vec::new(),
            body: String::new(),
        }
    }

    /// The subject, or an empty string.
    pub fn subject(&self) -> &str {
        self.subject.as_deref().unwrap_or_default()
    }

    /// Parse the header out of an existing patch file.
    ///
    /// Returns `None` when there is nothing header-shaped, which is the normal case for a
    /// patch that is nothing but a diff. Callers then decide what to stamp, rather than this
    /// function inventing an author.
    pub fn parse(text: &str) -> Option<Self> {
        // The header ends at the diff. Looking for the first `diff --git` rather than for a
        // blank line is what keeps a commit body containing a `---` divider from truncating
        // the header early.
        let end = text.find("diff --git ").unwrap_or(text.len());
        let head = &text[..end];
        if !head.contains("Subject:") && !head.contains("From:") {
            return None;
        }

        let mut header = Self {
            from_line: None,
            author: None,
            date: None,
            subject: None,
            extra: Vec::new(),
            body: String::new(),
        };
        let mut after_headers = false;

        for line in head.lines() {
            if !after_headers {
                if line.trim().is_empty() {
                    // The blank line after the last header ends the header block.
                    after_headers = header.author.is_some() || header.subject.is_some();
                    continue;
                }
                if let Some(rest) = line.strip_prefix("From ") {
                    header.from_line = Some(line.to_owned());
                    let _ = rest;
                } else if let Some(v) = line.strip_prefix("From: ") {
                    header.author = Some(v.trim().to_owned());
                } else if let Some(v) = line.strip_prefix("Date: ") {
                    header.date = Some(v.trim().to_owned());
                } else if let Some(v) = line.strip_prefix("Subject: ") {
                    header.subject = Some(strip_patch_prefix(v));
                } else {
                    header.extra.push(line.to_owned());
                }
                continue;
            }
            // Everything after the blank line is the commit body, minus the `---`
            // separator and diffstat that `format-patch` puts there.
            if line.trim() == "---" || line.starts_with(" ") || line.starts_with(' ') {
                break;
            }
            header.body.push_str(line);
            header.body.push('\n');
        }
        // Blank lines between the body and the `---` separator are part of the format, not
        // the message. Trimming only the tail keeps internal paragraph breaks intact.
        while header.body.ends_with("\n\n") {
            header.body.pop();
        }
        Some(header)
    }

    /// Render the mail header, including the `---` and diffstat separator.
    pub fn render(&self, stats: Option<&str>) -> String {
        let mut out = String::new();
        if let Some(from) = &self.from_line {
            out.push_str(from);
            if !from.ends_with('\n') {
                out.push('\n');
            }
        }
        if let Some(author) = &self.author {
            out.push_str(&format!("From: {author}\n"));
        }
        if let Some(date) = &self.date {
            out.push_str(&format!("Date: {date}\n"));
        }
        if let Some(subject) = self.subject.as_deref() {
            out.push_str(&format!("Subject: [PATCH] {subject}\n"));
        }
        for line in &self.extra {
            out.push_str(line);
            if !line.ends_with('\n') {
                out.push('\n');
            }
        }
        out.push('\n');
        if !self.body.is_empty() {
            out.push_str(&self.body);
            if !self.body.ends_with('\n') {
                out.push('\n');
            }
            out.push('\n');
        }
        // The separator is what tells a reader, and `git am`, where the message ends. It is
        // not decoration: without it the diffstat is not separated from the body.
        out.push_str("---\n");
        if let Some(stats) = stats {
            out.push_str(stats);
            if !stats.ends_with('\n') {
                out.push('\n');
            }
        }
        out.push('\n');
        out
    }
}

/// Format seconds-since-epoch as an RFC 2822 date, which is what `git log` and
/// `format-patch` write on a `Date:` line.
///
/// libgit2 can format a timestamp but exposes no formatter for it through `git2`, and this
/// is the only date skot has to write. The civil-from-days conversion is the standard one,
/// and UTC is honest: git itself records an offset, but a patch stamped here has no local
/// timezone to be honest about.
fn rfc2822(seconds: i64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let (hour, minute, second) = (rest / 3600, (rest % 3600) / 60, rest % 60);

    // Days since 1970-01-01 to a civil date, via the era-based algorithm from Howard
    // Hinnant's `chrono`-compatible maths. `z` is days since 1970-01-01.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!(
        "{}, {d} {} {y} {hour:02}:{minute:02}:{second:02} +0000",
        DAYS[days.rem_euclid(7) as usize],
        MONTHS[(m - 1) as usize]
    )
}

fn strip_patch_prefix(subject: &str) -> String {
    // `format-patch` writes `Subject: [PATCH] title`, and a later `format-patch -v` writes
    // `[PATCH v2]`. Both prefixes are display metadata, not part of the title, and both
    // would otherwise accumulate into the title on every save.
    let mut rest = subject.trim();
    if let Some(after) = rest.strip_prefix('[')
        && let Some(close) = after.find(']')
    {
        rest = after[close + 1..].trim_start();
    }
    rest.to_owned()
}

/// Diff `tree` against the sandbox's working tree and render it as a patch file.
///
/// Untracked files are included, because that is how a file gets into a series at all.
///
/// This is a *single* diff from the base tree to the working directory, not a base→index diff
/// followed by an index→workdir one. Concatenating the two halves looks equivalent and is not:
/// each half is a diff against a different tree, so a file that both `apply`ed and the author
/// then edited ends up with two `diff --git` sections — one carrying the patch's original
/// change, one carrying the edit. libgit2 applies that, `git apply` applies that, and the series
/// quietly ends up applying the patch's own change twice.
///
/// `diff_tree_to_workdir_with_index` walks the index and the working directory in one pass, so
/// it already accounts for staged, unstaged and untracked changes at once and prints the
/// contents of new files. The split was only ever needed because the *two-argument* form drops
/// untracked contents, and there is no reason to pay for that.
pub fn tree_to_patch(
    repo: &git2::Repository,
    tree: &git2::Tree<'_>,
    header: &Header,
) -> Result<String> {
    let mut options = git2::DiffOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .show_untracked_content(true)
        // A file that changes from a regular file to a symlink is a real change, and
        // dropping it silently is how a patch loses a file.
        .include_typechange(true);

    let diff = repo
        .diff_tree_to_workdir_with_index(Some(tree), Some(&mut options))
        .context("diffing the sandbox working directory against the base tree")?;

    let mut body = String::new();
    render_body(&diff, &mut body)?;
    let stats = render_stats(&diff)?.map(|lines| lines.join("\n"));

    Ok(format!("{}{body}", header.render(stats.as_deref())))
}

/// Diff two trees and render the result as a patch file.
///
/// This is what `rebase` uses: both sides are already in the object database, so the result
/// is a patch that applies to the new base by construction rather than by a later `check`.
pub fn tree_to_tree_patch(
    repo: &git2::Repository,
    old: &git2::Tree<'_>,
    new: &git2::Tree<'_>,
    header: &Header,
) -> Result<String> {
    let diff = repo
        .diff_tree_to_tree(Some(old), Some(new), None)
        .context("diffing two trees")?;

    let mut body = String::new();
    render_body(&diff, &mut body)?;
    let stats = render_stats(&diff)?.map(|lines| lines.join("\n"));

    Ok(format!("{}{body}", header.render(stats.as_deref())))
}

/// Append a diff's patch-formatted body to `out`.
///
/// libgit2 hands `content` over *without* the `+`/`-`/space sigil — that lives in `origin()`.
/// Writing `content` straight out produces something that reads like a patch and is not one:
/// every added line arrives as a context line, so the export silently applies as a no-op or
/// fails outright. Meta lines (file headers, hunk headers, "no newline" markers) carry no sigil
/// and are written as they come.
///
/// Appending rather than replacing is what lets a caller concatenate two diffs into one patch
/// file: libgit2 reads a patch as a header followed by however many file sections it finds, so
/// base→index and index→worktree join end to end into a single valid patch.
fn render_body(diff: &git2::Diff<'_>, out: &mut String) -> Result<()> {
    diff.print(git2::DiffFormat::Patch, |_, _, line| {
        let origin = line.origin();
        if matches!(origin, ' ' | '+' | '-') {
            out.push(origin);
        }
        out.push_str(&String::from_utf8_lossy(line.content()));
        true
    })
    .context("rendering the diff as a patch")?;
    Ok(())
}

/// A diff's diffstat, one entry per line, or `None` when the diff is empty.
///
/// The buffer libgit2 hands back is NUL-terminated, and writing that NUL into a patch file
/// produces a file that some tools read and others choke on, so it is trimmed here rather
/// than in every caller.
fn render_stats(diff: &git2::Diff<'_>) -> Result<Option<Vec<String>>> {
    if diff.deltas().len() == 0 {
        return Ok(None);
    }
    let buf = diff
        .stats()
        .context("summarising the diff")?
        .to_buf(git2::DiffStatsFormat::FULL, 80)
        .context("formatting the diffstat")?;
    let text = String::from_utf8_lossy(&buf);
    let text = text.trim_end_matches('\0');

    let lines: Vec<String> = text
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    Ok((!lines.is_empty()).then_some(lines))
}

/// Whether `path` is one a `keep` glob covers.
///
/// Deliberately simple: a glob is a prefix match with `*` matching one path segment and `**`
/// matching any number of them. Anything cleverer would be a glob library, and a `keep` list
/// is four entries written by a human who wants a dependency directory to survive.
///
/// A glob that names a directory covers everything under it, which is what makes
/// `keep = ["node_modules"]` mean the obvious thing without the user having to write
/// `node_modules/**`.
/// A one-line description of what a patch changes, for progress output.
///
/// The first file it touches and how many more. Read from the `diff --git a/x b/x` header
/// because that is the one place a patch names its files in a form that does not depend on
/// the language, the diff settings, or whether the diff is even parseable.
pub fn summarise(text: &str) -> String {
    let files: Vec<&str> = text
        .lines()
        .filter_map(|l| l.strip_prefix("diff --git "))
        // "a/path b/path": the post-image side is the name after the change.
        .filter_map(|l| l.split_whitespace().nth(1))
        .map(|p| p.rsplit('/').next().unwrap_or(p))
        .collect();
    match files.as_slice() {
        [] => "no changes".to_owned(),
        [only] => (*only).to_owned(),
        [first, rest @ ..] => format!("{first} and {} more file(s)", rest.len()),
    }
}

/// What a patch *does*, as opposed to how it is written.
///
/// Skips the parts that are re-derived every time: blob ids in `index`, and the line numbers
/// in hunk headers. Both come out of libgit2 now rather than off the author's keyboard, so
/// they change when a patch is saved even though the patch still makes exactly the same
/// edit. Comparing these is what makes `edit-save` idempotent and tells a work area whether
/// it has been edited; comparing whole files does neither.
pub fn change_of(patch: &str) -> Vec<&str> {
    let body = match patch.find("diff --git ") {
        Some(at) => &patch[at..],
        None => "",
    };
    body.lines()
        .filter(|line| {
            !line.starts_with("diff --git")
                && !line.starts_with("index ")
                && !line.starts_with("@@")
                && !line.starts_with("+++")
                && !line.starts_with("---")
        })
        .collect()
}

/// Match a `keep` glob against a path.
pub fn keep_matches(glob: &str, path: &Path) -> bool {
    let glob = glob.trim_end_matches('/');
    let path = path.to_string_lossy();
    if match_glob(glob.as_bytes(), path.as_bytes()) {
        return true;
    }
    // Anything under a directory the glob names is covered too, but only at a segment
    // boundary: `node_modules` must not match `node_modules_staging`.
    let mut rest = path.as_ref();
    while let Some(i) = rest.find('/') {
        if match_glob(glob.as_bytes(), &rest.as_bytes()[..i]) {
            return true;
        }
        rest = &rest[i + 1..];
    }
    false
}

fn match_glob(glob: &[u8], path: &[u8]) -> bool {
    // `**` matches any run of characters, including `/`. It has to, or `a/**` would not
    // cover `a/b/c`, which is the case everyone means by it.
    if let Some(i) = find_double_star(glob) {
        let before = &glob[..i];
        let after = &glob[i + 2..];
        if !path.starts_with(before) {
            return false;
        }
        let rest = &path[before.len()..];
        // `**/` also has to match *zero* directories, so `**/*.log` covers `a.log` and not
        // just `build/a.log`. Offering the remainder with the slash consumed is what does it.
        let after_slash = after.strip_prefix(b"/").unwrap_or(after);
        for split in 0..=rest.len() {
            let tail = &rest[split..];
            if match_glob(after, tail) || match_glob(after_slash, tail) {
                return true;
            }
        }
        return false;
    }

    match_glob_single(glob, path)
}

/// `*` matches within one segment, `?` matches one character.
fn match_glob_single(glob: &[u8], path: &[u8]) -> bool {
    if glob.is_empty() {
        return path.is_empty();
    }
    match glob[0] {
        b'*' => {
            for i in 0..=path.len() {
                // Do not let a single `*` cross a `/`: that is the whole difference between
                // `keep = ["build"]` protecting `build/x` and accidentally protecting
                // `build/../secrets`.
                if i < path.len() && path[i] == b'/' {
                    break;
                }
                if match_glob_single(&glob[1..], &path[i..]) {
                    return true;
                }
            }
            false
        }
        b'?' => !path.is_empty() && path[0] != b'/' && match_glob_single(&glob[1..], &path[1..]),
        c => !path.is_empty() && path[0] == c && match_glob_single(&glob[1..], &path[1..]),
    }
}

fn find_double_star(glob: &[u8]) -> Option<usize> {
    glob.windows(2).position(|w| w == b"**")
}

/// Paths in `tree` that the target checkout would delete, minus those a `keep` glob covers.
///
/// This is the set `reset_to` must put back after a force checkout: a force checkout removes
/// tracked files the target tree does not have, and a `keep` path is by definition one where
/// that is wrong.
pub fn keep_survivors(
    old_tree: &git2::Tree<'_>,
    new_tree: &git2::Tree<'_>,
    keep: &[String],
) -> Vec<std::path::PathBuf> {
    if keep.is_empty() {
        return Vec::new();
    }
    // `Tree::walk` is callback-based rather than an iterator, so the paths are collected
    // first and filtered after. A `keep` list is short and a tree is finite, so this is not
    // the expensive part.
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    let walked = old_tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
        if entry.kind() == Some(git2::ObjectType::Blob) {
            paths.push(Path::new(root).join(entry.name().unwrap_or_default()));
        }
        // `true` descends into this directory.
        git2::TreeWalkResult::Ok
    });
    if walked.is_err() {
        return Vec::new();
    }

    let mut out = Vec::new();
    for path in paths {
        // Only files the new tree dropped. A file both trees have is checked out normally.
        if new_tree.get_path(&path).is_ok() {
            continue;
        }
        if keep.iter().any(|glob| keep_matches(glob, &path)) {
            out.push(path);
        }
    }
    out.sort();
    out
}

/// Restore `paths` from `tree` into the working directory of `repo`.
///
/// The scope is the *paths*, not the tree, and that distinction is the whole implementation.
///
/// The obvious version of this — build a tree containing only the paths to restore, then
/// force-check it out — deletes every other tracked file in the working directory. libgit2
/// treats a checkout target as the truth about what should be on disk and removes the rest;
/// `remove_untracked(false)` protects untracked files only, so the tracked ones it just
/// restored are collateral damage. That failure is silent: the command reports success, the
/// keep path comes back, and the sandbox is quietly missing everything else.
///
/// `CheckoutBuilder::path` restricts the checkout to the named paths, so the files outside
/// `paths` are never considered at all. A path already present in the working directory is
/// also left alone: `keep` promises the reset will not delete a path, not that it will discard
/// the user's unsaved work in it. Skipping is the safe direction to be wrong in.
pub fn restore_paths(
    repo: &git2::Repository,
    tree: &git2::Tree<'_>,
    paths: &[std::path::PathBuf],
) -> Result<Vec<std::path::PathBuf>> {
    let workdir = repo
        .workdir()
        .context("the sandbox repository has no working directory")?;

    let mut options = git2::build::CheckoutBuilder::new();
    // `remove_untracked` off: a keep path is usually untracked precisely because it is a
    // build cache, and removing it would throw away the reason for a persistent sandbox.
    options.remove_untracked(false);

    let mut restored = Vec::new();
    for path in paths {
        // Nothing to restore from: either the old tree never had it, or this path is not a
        // plain blob. Either way there is no single entry to check out, which is why this is
        // a path-scoped restore rather than a one-entry tree.
        if tree.get_path(path).is_err() {
            continue;
        }
        if workdir.join(path).exists() {
            continue;
        }
        options.path(path);
        restored.push(path.clone());
    }

    if !restored.is_empty() {
        repo.checkout_tree(tree.as_object(), Some(&mut options))
            .context(format!("restoring {} keep path(s)", restored.len()))?;
    }
    Ok(restored)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nested_keep_path_is_restored_to_its_own_directory() {
        // `keep = ["docs/notes.md"]` has to come back as `docs/notes.md`. Taking the
        // file_name and checking out a one-entry tree at the root puts it at `notes.md`
        // instead — the path survives, the location does not, and the directory it was
        // supposed to protect looks empty.
        let tmp = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(tmp.path()).unwrap();
        std::fs::create_dir_all(repo.workdir().unwrap().join("docs")).unwrap();
        std::fs::write(repo.workdir().unwrap().join("docs/notes.md"), "# notes\n").unwrap();
        let mut index = repo.index().unwrap();
        index
            .add_path(std::path::Path::new("docs/notes.md"))
            .unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();

        // Take it away again, so the restore has something to do.
        std::fs::remove_file(repo.workdir().unwrap().join("docs/notes.md")).unwrap();

        let restored = restore_paths(&repo, &tree, &[std::path::PathBuf::from("docs/notes.md")])
            .expect("restore");

        assert_eq!(restored.len(), 1);
        let wanted = repo.workdir().unwrap().join("docs/notes.md");
        assert!(
            wanted.exists(),
            "expected {} to exist; the tree root has {:?}",
            wanted.display(),
            tree.iter()
                .map(|e| e.name().unwrap_or("?").to_owned())
                .collect::<Vec<_>>()
        );
        assert_eq!(std::fs::read_to_string(&wanted).unwrap(), "# notes\n");
    }

    #[test]
    fn restoring_one_path_leaves_every_other_file_alone() {
        // The dangerous one. Restoring a single path by checking out a tree containing only
        // that path makes libgit2 treat the tree as the truth about the working directory and
        // delete the rest — so `keep` would quietly empty the sandbox, keeping the one path it
        // was asked to protect. `remove_untracked(false)` does not help: these files are
        // tracked.
        let tmp = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(tmp.path()).unwrap();
        let workdir = repo.workdir().unwrap().to_path_buf();
        for (name, body) in [("kept.txt", "kept\n"), ("sibling.txt", "sibling\n")] {
            std::fs::write(workdir.join(name), body).unwrap();
        }
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("kept.txt")).unwrap();
        index.add_path(std::path::Path::new("sibling.txt")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();

        // Only `kept.txt` is gone; `sibling.txt` is right where it was.
        std::fs::remove_file(workdir.join("kept.txt")).unwrap();
        std::fs::write(workdir.join("sibling.txt"), "sibling edited\n").unwrap();

        let restored =
            restore_paths(&repo, &tree, &[std::path::PathBuf::from("kept.txt")]).unwrap();
        assert_eq!(restored, vec![std::path::PathBuf::from("kept.txt")]);
        assert_eq!(
            std::fs::read_to_string(workdir.join("kept.txt")).unwrap(),
            "kept\n"
        );
        assert_eq!(
            std::fs::read_to_string(workdir.join("sibling.txt")).unwrap(),
            "sibling edited\n",
            "restoring a keep path must not disturb its neighbours"
        );
    }

    #[test]
    fn restoring_leaves_a_path_that_is_already_there() {
        // `keep` promises a reset will not delete a path. It does not promise to throw away
        // unsaved work in it, so a path that is still present is left exactly as it is.
        let tmp = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(tmp.path()).unwrap();
        let workdir = repo.workdir().unwrap().to_path_buf();
        std::fs::write(workdir.join("kept.txt"), "tracked\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("kept.txt")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();

        std::fs::write(workdir.join("kept.txt"), "my unsaved work\n").unwrap();
        let restored =
            restore_paths(&repo, &tree, &[std::path::PathBuf::from("kept.txt")]).unwrap();
        assert!(restored.is_empty(), "nothing needed restoring");
        assert_eq!(
            std::fs::read_to_string(workdir.join("kept.txt")).unwrap(),
            "my unsaved work\n"
        );
    }

    #[test]
    fn a_keep_path_that_the_new_tree_dropped_is_left_alone() {
        // Nothing to restore is not an error: a keep path that the target tree never had has
        // nothing in the old tree either, and resetting should not start failing over it.
        let tmp = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(tmp.path()).unwrap();
        std::fs::write(repo.workdir().unwrap().join("kept.txt"), "kept\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("kept.txt")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let restored =
            restore_paths(&repo, &tree, &[std::path::PathBuf::from("node_modules")]).unwrap();
        assert!(restored.is_empty());
    }

    #[test]
    fn a_header_round_trips_through_render_and_parse() {
        let text = "\
From abc1234 Mon Sep 17 00:00:00 2001
From: A U Thor <author@example.com>
Date: Thu, 1 Oct 2026 18:17:53 +0300
Subject: [PATCH] trim the widgets
X-Skot-Import: range:main..feat aaa111 bbb222 ccc333

The body of the commit.

---
 a.txt | 1 +
 1 file changed

diff --git a/a.txt b/a.txt
";
        let header = Header::parse(text).expect("this is a header");
        assert_eq!(header.subject(), "trim the widgets");
        assert_eq!(
            header.author.as_deref(),
            Some("A U Thor <author@example.com>")
        );
        assert_eq!(header.body, "The body of the commit.\n");
        assert_eq!(
            header.extra,
            ["X-Skot-Import: range:main..feat aaa111 bbb222 ccc333"],
            "a header line the parser does not model must not be dropped"
        );

        // And re-emitting it keeps the author, which is the point: a re-saved patch must
        // not re-stamp the committer. The provenance line survives with it, or the next
        // import of the same request could not recognise the patch.
        let again = Header::parse(&header.render(Some(" a.txt | 1 +\n"))).unwrap();
        assert_eq!(again.author, header.author);
        assert_eq!(again.date, header.date);
        assert_eq!(again.subject, header.subject);
        assert_eq!(again.extra, header.extra);
        assert!(
            again
                .render(None)
                .contains("X-Skot-Import: range:main..feat aaa111 bbb222 ccc333"),
            "the provenance line must be re-emitted"
        );
    }

    #[test]
    fn a_patch_with_no_header_parses_to_nothing() {
        assert_eq!(
            Header::parse("diff --git a/a b/a\n@@ -1 +1 @@\n-a\n+b\n"),
            None
        );
    }

    #[test]
    fn the_patch_prefix_is_not_part_of_the_subject() {
        // `format-patch -v` writes `[PATCH v2]`. Re-stamping that into the title would make
        // every save of a v2 patch a v3.
        let header = Header::parse(
            "From: a@b\nSubject: [PATCH v2] title here\n\n---\n\ndiff --git a/a b/a\n",
        )
        .unwrap();
        assert_eq!(header.subject(), "title here");
    }

    #[test]
    fn a_body_containing_a_separator_does_not_truncate_the_subject() {
        let header = Header::parse(
            "From: a@b\nSubject: [PATCH] t\n\nbody\n\nmore body\n\n---\n a | 1 +\n\ndiff --git a/a b/a\n",
        )
        .unwrap();
        assert_eq!(header.subject(), "t");
    }

    #[test]
    fn the_from_line_survives_a_round_trip() {
        let text = "From abc1234 Mon Sep 17 00:00:00 2001\nFrom: a@b\nSubject: [PATCH] t\n\n---\n\ndiff --git a/a b/a\n";
        let header = Header::parse(text).unwrap();
        assert!(header.from_line.is_some(), "the sha line was dropped");
        assert!(header.render(None).contains("From abc1234"));
    }

    #[test]
    fn a_signed_header_stamps_an_author() {
        let sig = git2::Signature::now("A U Thor", "a@b").unwrap();
        let header = Header::signed("t", &sig);
        assert!(header.author.is_some());
        assert_eq!(header.subject(), "t");
    }

    #[test]
    fn keep_globs_cover_the_obvious_shapes() {
        let cases: &[(&str, &str, bool)] = &[
            ("node_modules", "node_modules", true),
            ("node_modules", "node_modules/react/index.js", true),
            ("node_modules/", "node_modules", true),
            ("node_modules", "node_modulesx", false),
            ("target/*", "target/debug", true),
            ("target/*", "target/debug/build", false),
            ("**/*.log", "build/logs/a.log", true),
            ("**/*.log", "a.log", true),
            ("a/**", "a/b/c", true),
            ("a/**", "b/c", false),
            ("?", "a", true),
            ("?", "ab", false),
        ];
        for (glob, path, expected) in cases {
            assert_eq!(
                keep_matches(glob, Path::new(path)),
                *expected,
                "glob {glob:?} against {path:?}"
            );
        }
    }

    #[test]
    fn a_summary_names_the_first_file() {
        let text = "diff --git a/a.txt b/a.txt\ndiff --git a/b.txt b/b.txt\n";
        assert_eq!(summarise(text), "a.txt and 1 more file(s)");
        assert_eq!(summarise("diff --git a/only b/only\n"), "only");
        assert_eq!(summarise("nothing"), "no changes");
    }

    #[test]
    fn change_of_sees_the_edit_but_not_the_parts_libgit2_rederives() {
        // The re-derived version has different blob ids and a shifted hunk offset, which
        // is what every save produces; it must read as the same change, or saving
        // twice rewrites the patch twice. A different edit must not.
        let written = "diff --git a/f.txt b/f.txt\nindex 1111111..2222222 100644\n--- a/f.txt\n+++ b/f.txt\n@@ -1 +1 @@\n-a\n+b\n";
        let rederived = "diff --git a/f.txt b/f.txt\nindex aaaaaaa..bbbbbbb 100644\n--- a/f.txt\n+++ b/f.txt\n@@ -42 +42 @@\n-a\n+b\n";
        let different = "diff --git a/f.txt b/f.txt\nindex 1111111..2222222 100644\n--- a/f.txt\n+++ b/f.txt\n@@ -1 +1 @@\n-a\n+c\n";

        assert_eq!(change_of(written), change_of(rederived));
        assert_ne!(change_of(written), change_of(different));
        assert_ne!(change_of(written), change_of("no diff at all"));
    }

    /// A repository with one commit holding `f.txt`, plus a working tree to edit.
    struct Sandbox {
        _tmp: tempfile::TempDir,
        repo: git2::Repository,
        base: git2::Oid,
    }

    impl Sandbox {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path().join("work");
            std::fs::create_dir_all(&dir).unwrap();
            let repo = git2::Repository::init(&dir).unwrap();

            std::fs::write(dir.join("f.txt"), "base\n").unwrap();
            let tree_id = {
                let mut b = repo.treebuilder(None).unwrap();
                b.insert("f.txt", repo.blob(b"base\n").unwrap(), 0o100644)
                    .unwrap();
                b.write().unwrap()
            };
            {
                let base = repo.find_tree(tree_id).unwrap();
                let mut index = repo.index().unwrap();
                index.read_tree(&base).unwrap();
                index.write().unwrap();
            }
            // `force`, because `f.txt` was already written to the workdir to have something to
            // diff against, and libgit2 otherwise calls that a local modification and refuses.
            let mut opts = git2::build::CheckoutBuilder::new();
            opts.force();
            repo.checkout_index(None, Some(&mut opts)).unwrap();

            Self {
                _tmp: tmp,
                repo,
                base: tree_id,
            }
        }

        fn dir(&self) -> std::path::PathBuf {
            self.repo.workdir().unwrap().to_path_buf()
        }

        fn base_tree(&self) -> git2::Tree<'_> {
            self.repo.find_tree(self.base).unwrap()
        }

        fn export(&self) -> String {
            let header = Header::signed("t", &git2::Signature::now("a", "a@b").unwrap());
            tree_to_patch(&self.repo, &self.base_tree(), &header).unwrap()
        }

        /// Apply an exported patch onto the base tree and return the resulting index.
        ///
        /// The index rather than a tree, because `apply_to_tree` builds it in memory — there is no
        /// repository behind it to `write_tree` into.
        fn replay(&self, patch: &str) -> git2::Index {
            let diff = git2::Diff::from_buffer(patch.as_bytes()).expect("a readable patch");
            self.repo
                .apply_to_tree(&self.base_tree(), &diff, None)
                .expect("the exported patch applies")
        }

        /// What a path holds in a replayed index, which is how these tests check that an export
        /// really is a patch rather than merely text that looks like one.
        fn text_in(&self, index: &git2::Index, path: &str) -> String {
            let entry = index
                .get_path(std::path::Path::new(path), 0)
                .expect("the patch created this path");
            String::from_utf8(self.repo.find_blob(entry.id).unwrap().content().to_vec()).unwrap()
        }
    }

    #[test]
    fn a_file_changed_by_both_apply_and_the_author_appears_once() {
        // The bug this guards: taking the export as base→index followed by index→workdir produces
        // two `diff --git` sections for one file — one for the change `apply` made, one for the
        // author's edit. Both apply cleanly, so nothing complains, and the patch's own change ends
        // up in the series twice.
        //
        // Staging a change and then editing it again is the same shape: the index holds what the
        // series produced, and the working tree holds what the author did to it.
        let s = Sandbox::new();
        std::fs::write(s.dir().join("f.txt"), "staged\n").unwrap();
        let mut index = s.repo.index().unwrap();
        index.add_path(std::path::Path::new("f.txt")).unwrap();
        index.write().unwrap();
        std::fs::write(s.dir().join("f.txt"), "staged\nthen edited\n").unwrap();

        let patch = s.export();
        assert_eq!(
            patch.matches("diff --git").count(),
            1,
            "one file, one section:\n{patch}"
        );
        assert_eq!(
            patch.matches("1 file changed").count(),
            1,
            "one diffstat summary, not two:\n{patch}"
        );
        assert_eq!(
            s.text_in(&s.replay(&patch), "f.txt"),
            "staged\nthen edited\n"
        );
    }

    #[test]
    fn an_untracked_file_is_exported_with_its_contents() {
        // The bug this guards: libgit2's `diff_tree_to_workdir` reports untracked files as
        // deltas but does not print their contents with `DiffFormat::Patch`, so an export
        // built from it produced a patch that claimed to add a file and did not contain it.
        let s = Sandbox::new();
        std::fs::write(s.dir().join("new.txt"), "brand new\n").unwrap();

        let patch = s.export();
        assert!(patch.contains("new.txt"), "the file is missing:\n{patch}");
        assert!(
            patch.contains("+brand new"),
            "the contents are missing:\n{patch}"
        );
        assert_eq!(s.text_in(&s.replay(&patch), "new.txt"), "brand new\n");
    }

    #[test]
    fn an_untracked_file_in_a_new_directory_is_exported() {
        // Untracked *directories* need `recurse_untracked_dirs`; without it the export names a
        // directory and no file, which applies to nothing.
        let s = Sandbox::new();
        std::fs::create_dir_all(s.dir().join("sub")).unwrap();
        std::fs::write(s.dir().join("sub/deep.txt"), "deep\n").unwrap();

        let patch = s.export();
        assert!(patch.contains("sub/deep.txt"), "not in patch:\n{patch}");
        assert!(patch.contains("+deep"), "no contents:\n{patch}");
        assert_eq!(s.text_in(&s.replay(&patch), "sub/deep.txt"), "deep\n");
    }

    #[test]
    fn a_staged_change_and_an_untracked_change_both_land_in_the_export() {
        // Two halves of one export: the staged edit comes from the index half and the new file
        // from the workdir half, and neither half may lose the other.
        let s = Sandbox::new();
        std::fs::write(s.dir().join("f.txt"), "staged\n").unwrap();
        {
            let mut index = s.repo.index().unwrap();
            index.add_path(std::path::Path::new("f.txt")).unwrap();
            index.write().unwrap();
        }
        std::fs::write(s.dir().join("added.txt"), "added\n").unwrap();

        let patch = s.export();
        assert!(patch.contains("+staged"), "staged edit missing:\n{patch}");
        assert!(patch.contains("+added"), "untracked file missing:\n{patch}");

        let applied = s.replay(&patch);
        assert_eq!(s.text_in(&applied, "f.txt"), "staged\n");
        assert_eq!(s.text_in(&applied, "added.txt"), "added\n");
    }

    #[test]
    fn an_unstaged_edit_over_a_staged_one_is_exported_as_the_final_contents() {
        // base -> staged -> unstaged. Applying both halves in sequence has to end at the
        // working tree, not at the index.
        let s = Sandbox::new();
        std::fs::write(s.dir().join("f.txt"), "middle\n").unwrap();
        {
            let mut index = s.repo.index().unwrap();
            index.add_path(std::path::Path::new("f.txt")).unwrap();
            index.write().unwrap();
        }
        std::fs::write(s.dir().join("f.txt"), "final\n").unwrap();

        assert_eq!(s.text_in(&s.replay(&s.export()), "f.txt"), "final\n");
    }

    #[test]
    fn a_deletion_is_exported() {
        let s = Sandbox::new();
        std::fs::remove_file(s.dir().join("f.txt")).unwrap();
        let patch = s.export();
        assert!(patch.contains("-base"), "no deletion:\n{patch}");
        s.replay(&patch);
    }

    #[test]
    fn an_export_of_no_changes_produces_no_diff_sections() {
        let s = Sandbox::new();
        let patch = s.export();
        assert!(!patch.contains("diff --git "), "expected nothing:\n{patch}");
    }

    #[test]
    fn a_diffstat_never_carries_libgit2s_nul_terminator() {
        // The stats buffer is NUL-terminated. Written out verbatim it makes a patch file that
        // skot reads and other tools reject.
        let s = Sandbox::new();
        std::fs::write(s.dir().join("f.txt"), "changed\n").unwrap();
        let patch = s.export();
        assert!(
            !patch.contains('\0'),
            "a NUL leaked into the patch:\n{patch:?}"
        );
        assert!(
            patch.contains("1 file changed"),
            "the diffstat is missing:\n{patch}"
        );
    }

    #[test]
    fn an_export_of_a_file_only_the_sandbox_has_still_applies() {
        // The end-to-end promise of `edit-save`: whatever the export says, it must apply back.
        let s = Sandbox::new();
        std::fs::write(s.dir().join("a.txt"), "a\n").unwrap();
        std::fs::create_dir_all(s.dir().join("d")).unwrap();
        std::fs::write(s.dir().join("d/b.txt"), "b\n").unwrap();
        std::fs::write(s.dir().join("f.txt"), "edited\n").unwrap();
        std::fs::remove_file(s.dir().join("f.txt")).unwrap();
        std::fs::write(s.dir().join("f.txt"), "edited again\n").unwrap();

        let applied = s.replay(&s.export());
        assert_eq!(s.text_in(&applied, "f.txt"), "edited again\n");
        assert_eq!(s.text_in(&applied, "a.txt"), "a\n");
        assert_eq!(s.text_in(&applied, "d/b.txt"), "b\n");
    }
}
