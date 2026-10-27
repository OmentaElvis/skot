//! The blob store: binary file content a patch names by hash, shared by the project.
//!
//! A patch file stays small and textual by holding references instead of payloads. Each
//! `X-Skot-Blob` header line names a path, the sha256 of its bytes and the size; the
//! bytes themselves live once, at `blobs/<hash>`, no matter how many patches or series
//! point at them. Two properties fall out of content addressing and everything below
//! leans on them: the name is a pure function of the bytes, so writing the same content
//! twice is a no-op and reading can verify what it got; and a reference is a line of
//! text, so the set of live references is derived from the patches themselves. There is
//! no second book to keep in step with them and nothing to drift.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::layout::Layout;

/// The header line a patch uses to reference stored content.
pub const REF_HEADER: &str = "X-Skot-Blob";

/// One reference a patch carries: where the file sits in the tree, and which bytes it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    /// The path the file takes in the tree, as the patch spells it.
    pub path: String,
    /// The sha256 of the content: its name in the store.
    pub hash: String,
    /// The content's length, so a preview can say how big a thing it holds.
    pub size: u64,
}

/// Whether `path` may name a file in the tree.
///
/// The reference is text from a patch file, so it is read as hostile: it must be a plain
/// relative path. A leading slash would escape the tree, a `..` segment would climb out
/// of it, a backslash is a path separator on some platforms skot runs on, and an empty
/// path names nothing. Everything else, including spaces and unusual characters, is a
/// legal file name and is left alone.
pub fn safe_repo_path(path: &str) -> bool {
    if path.is_empty() || path.starts_with('/') || path.contains('\\') || path.contains('\0') {
        return false;
    }
    path.split('/')
        .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// The hash of `bytes`, as its lowercase hex name in the store.
pub fn hash_of(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Store `bytes` under their hash and hand back the name.
///
/// Written to a scratch name and renamed, so a reader never sees a half-written blob and
/// two writers of the same content cannot interleave. Content already stored is the
/// normal case (two patches holding the same bytes) and costs one hash and one exists
/// check.
pub fn write(dir: &Path, bytes: &[u8]) -> Result<String> {
    let hash = hash_of(bytes);
    let target = dir.join(&hash);
    if target.exists() {
        return Ok(hash);
    }
    std::fs::create_dir_all(dir).context(format!("creating {}", dir.display()))?;
    let scratch = dir.join(format!(".{hash}.tmp"));
    std::fs::write(&scratch, bytes).context(format!("writing {}", scratch.display()))?;
    std::fs::rename(&scratch, &target).context(format!("storing {}", target.display()))?;
    Ok(hash)
}

/// The stored bytes for `hash`, with the name checked against the content.
///
/// The name is validated as a bare hash before it becomes part of a path: a reference is
/// text from a patch, and `..`, a separator or a drive prefix must not turn into a read
/// outside the store. The content is then hashed and compared to the name, so a blob
/// that was corrupted on disk is reported as that rather than as whatever it now
/// decodes to.
pub fn read(dir: &Path, hash: &str) -> Result<Vec<u8>> {
    if !is_hash(hash) {
        bail!(
            "`{hash}` is not a blob name; refusing to read it from {}.",
            dir.display()
        );
    }
    let path = dir.join(hash);
    let bytes =
        std::fs::read(&path).context(format!("reading the stored blob {}", path.display()))?;
    if hash_of(&bytes) != hash {
        bail!(
            "the stored blob {} does not match its name; the bytes are corrupt.",
            path.display()
        );
    }
    Ok(bytes)
}

/// Whether `text` is a bare store name: exactly the hex length a sha256 spells.
fn is_hash(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Every reference a patch's header carries.
///
/// Only the mail header is scanned, and only up to the first blank line or the first
/// `diff --git` line, whichever comes first: a commit body may say anything at all, and
/// a line that merely looks like a reference must not send a reader into the store. A
/// line that does claim to be a reference but does not parse is an error rather than
/// something to skip, because the patch that holds it is broken either way and applying
/// it halfway is worse than stopping.
pub fn refs_in(text: &str) -> Result<Vec<Ref>> {
    let mut refs = Vec::new();
    for line in header_lines(text) {
        let Some(rest) = line.strip_prefix(&format!("{REF_HEADER}: ")) else {
            continue;
        };
        refs.push(parse_ref(rest)?);
    }
    Ok(refs)
}

/// The same scan, skipping anything malformed: what the sweeper uses.
///
/// A corrupt reference line means the patch is already broken, and refusing to sweep
/// because of it would pin every blob in the project in place forever.
pub fn refs_in_lossy(text: &str) -> Vec<Ref> {
    let mut refs = Vec::new();
    for line in header_lines(text) {
        let Some(rest) = line.strip_prefix(&format!("{REF_HEADER}: ")) else {
            continue;
        };
        if let Ok(parsed) = parse_ref(rest) {
            refs.push(parsed);
        }
    }
    refs
}

/// The header region of a patch: the lines before the first blank line, stopping at
/// `diff --git` even for a headerless patch that starts with one.
fn header_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .take_while(|line| !line.trim_end().is_empty() && !line.starts_with("diff --git "))
}

/// `path <64 hex> <digits>`: the name last two fields so a path may contain spaces.
fn parse_ref(rest: &str) -> Result<Ref> {
    let Some((head, size)) = rest.rsplit_once(' ') else {
        bail!("`{REF_HEADER}: {rest}` is not a path, a hash and a size.");
    };
    let Some((path, hash)) = head.rsplit_once(' ') else {
        bail!("`{REF_HEADER}: {rest}` is not a path, a hash and a size.");
    };
    let size: u64 = size
        .parse()
        .context(format!("`{REF_HEADER}: {rest}` has no size at the end"))?;
    if !is_hash(hash) {
        bail!("`{REF_HEADER}: {rest}` does not end in a sha256.");
    }
    if !safe_repo_path(path) {
        bail!("`{REF_HEADER}` names `{path}`, which is not a safe path in the tree.");
    }
    Ok(Ref {
        path: path.to_owned(),
        hash: hash.to_owned(),
        size,
    })
}

/// Remove stored blobs no patch anywhere references any more.
///
/// The keep set is read back from the patches themselves: every `*.patch` under the live
/// series, the undo buffer, the work areas and scratch, because any of them can hold the
/// only reference to a blob at the moment a mutation frees one elsewhere. A file whose
/// name is not a store name is not the store's business and is left where it is.
///
/// Called after a mutation that can drop a reference, never before: the order is always
/// write the new state, then sweep, so an interrupted run leaves an unreferenced blob
/// rather than a reference with nothing behind it.
pub fn sweep(layout: &Layout) -> Result<usize> {
    let mut live = std::collections::HashSet::new();
    for root in [
        layout.series_root(),
        layout.undo_dir(),
        layout.work_dir(),
        layout.tmp_dir(),
    ] {
        for path in patch_files(&root) {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            for found in refs_in_lossy(&text) {
                live.insert(found.hash);
            }
        }
    }

    let store = layout.blobs_dir();
    let Ok(entries) = std::fs::read_dir(&store) else {
        return Ok(0);
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !is_hash(&name) || live.contains(&name) {
            continue;
        }
        if entry.path().is_file() && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        log::debug!(
            target: "blobs",
            "swept {removed} unreferenced blob(s) from {}",
            store.display()
        );
    }
    Ok(removed)
}

/// Every `*.patch` under `root`, recursively.
fn patch_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(patch_files(&path));
        } else if path.extension().is_some_and(|ext| ext == "patch") {
            out.push(path);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn layout() -> (tempfile::TempDir, Layout) {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path());
        (tmp, layout)
    }

    /// A patch whose header carries one reference, followed by a body that says
    /// anything at all.
    fn patch_with(path: &str, hash: &str, size: u64) -> String {
        format!(
            "From: A U Thor <a@b>\n\
             Subject: [PATCH] t\n\
             X-Skot-Blob: {path} {hash} {size}\n\
             \n\
             body that may mention X-Skot-Blob: evil deadbeef 1\n\
             \n\
             ---\n\
             \n\
             diff --git a/{path} b/{path}\n\
             Binary files a/x and b/{path} differ\n"
        )
    }

    #[test]
    fn a_stored_blob_round_trips_and_keeps_its_name() {
        let dir = store();
        let hash = write(dir.path(), b"contents").unwrap();
        assert_eq!(hash.len(), 64);
        assert!(dir.path().join(&hash).is_file());
        assert_eq!(read(dir.path(), &hash).unwrap(), b"contents");
        // The second write of the same bytes changes nothing.
        assert_eq!(write(dir.path(), b"contents").unwrap(), hash);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_name_that_is_not_a_hash_is_refused_before_any_path_is_built() {
        let dir = store();
        // Each of these, joined onto a directory, would walk out of the store.
        for bad in [
            "../escaped",
            "a/../../b",
            "/etc/passwd",
            "..",
            "short",
            "g".repeat(64).as_str(),
            "x\\y",
        ] {
            let err = read(dir.path(), bad).unwrap_err();
            assert!(
                format!("{err:#}").contains("not a blob name"),
                "refused {bad:?} without saying why: {err:#}"
            );
        }
        assert!(
            !dir.path().parent().unwrap().join("escaped").exists(),
            "nothing may be read or written outside the store"
        );
    }

    #[test]
    fn corrupt_bytes_are_caught_by_the_name() {
        let dir = store();
        let hash = write(dir.path(), b"the real bytes").unwrap();
        std::fs::write(dir.path().join(&hash), b"tampered bytes").unwrap();
        let err = read(dir.path(), &hash).unwrap_err();
        assert!(format!("{err:#}").contains("corrupt"), "{err:#}");
    }

    #[test]
    fn a_reference_parses_with_a_path_that_contains_spaces() {
        let hash = hash_of(b"x");
        let refs = refs_in(&patch_with("dir/my logo.png", &hash, 12)).unwrap();
        assert_eq!(
            refs,
            vec![Ref {
                path: "dir/my logo.png".into(),
                hash,
                size: 12
            }]
        );
    }

    #[test]
    fn the_body_is_not_scanned_for_references() {
        // The body of the fixture claims a reference to a hash that does not exist as a
        // file; only the header line may be honoured.
        let hash = hash_of(b"x");
        let refs = refs_in(&patch_with("logo.png", &hash, 12)).unwrap();
        assert_eq!(refs.len(), 1, "only the header's own line: {refs:?}");
        assert_eq!(refs[0].path, "logo.png");
    }

    #[test]
    fn a_malformed_reference_is_an_error_not_a_silent_skip() {
        let hash = hash_of(b"x");
        let bad_hash = "not-a-hash";
        let text = format!(
            "Subject: [PATCH] t\nX-Skot-Blob: logo.png {bad_hash} 12\n\n---\n\n\
             diff --git a/logo.png b/logo.png\n"
        );
        let err = refs_in(&text).unwrap_err();
        assert!(format!("{err:#}").contains("sha256"), "{err:#}");

        let traversal = format!(
            "Subject: [PATCH] t\nX-Skot-Blob: ../../escape {hash} 12\n\n---\n\n\
             diff --git a/x b/x\n"
        );
        let err = refs_in(&traversal).unwrap_err();
        assert!(format!("{err:#}").contains("safe path"), "{err:#}");
        // The lossy scan the sweeper uses skips both without pinning the store.
        assert_eq!(refs_in_lossy(&text).len(), 0);
        assert_eq!(refs_in_lossy(&traversal).len(), 0);
    }

    #[test]
    fn unsafe_tree_paths_are_refused() {
        for bad in [
            "",
            "/abs",
            "a/../b",
            "..",
            "./x",
            "a//b",
            "a\\b",
            "nul\0byte",
        ] {
            assert!(!safe_repo_path(bad), "{bad:?} must be refused");
        }
        for good in ["logo.png", "dir/sub/logo.png", "a..b", "sp ace.png"] {
            assert!(safe_repo_path(good), "{good:?} must be allowed");
        }
    }

    #[test]
    fn the_sweeper_keeps_what_any_series_work_area_or_undo_still_names() {
        let (_tmp, l) = layout();
        let hash = hash_of(b"shared");
        let orphan = hash_of(b"unreferenced");
        std::fs::create_dir_all(l.blobs_dir()).unwrap();
        std::fs::write(l.blobs_dir().join(&hash), b"shared").unwrap();
        std::fs::write(l.blobs_dir().join(&orphan), b"unreferenced").unwrap();
        // A stranger that is not a store name is not the store's business.
        std::fs::write(l.blobs_dir().join("README"), "not a blob").unwrap();

        // One live series names it, a second series does not, an undo entry does, and a
        // work area copy does: any of those alone must keep the blob alive.
        for dir in [
            l.series_root().join("default"),
            l.series_root().join("other"),
            l.undo_entry(),
            l.work_dir().join("default").join("some-patch"),
        ] {
            std::fs::create_dir_all(&dir).unwrap();
        }
        std::fs::write(
            l.series_root().join("default").join("a.patch"),
            patch_with("logo.png", &hash, 6),
        )
        .unwrap();
        std::fs::write(
            l.undo_entry().join("b.patch"),
            patch_with("logo.png", &hash, 6),
        )
        .unwrap();
        // `other` and the work area hold no reference at all.

        let removed = sweep(&l).unwrap();

        assert_eq!(removed, 1, "exactly the unreferenced blob goes");
        assert!(l.blobs_dir().join(&hash).exists(), "the live blob stays");
        assert!(!l.blobs_dir().join(&orphan).exists());
        assert!(l.blobs_dir().join("README").exists(), "strangers stay");
    }

    #[test]
    fn a_sweep_of_a_project_with_no_store_is_nothing() {
        let (_tmp, l) = layout();
        assert_eq!(sweep(&l).unwrap(), 0);
    }
}
