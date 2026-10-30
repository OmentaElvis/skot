//! Turning an unversioned directory into a commit.
//!
//! `skot track` uses this so a plain unpacked archive can stand in for an upstream Git
//! repository. The tree is read from disk once, written into the project mirror, and
//! wrapped in a synthetic commit whose metadata is fixed: the same directory always
//! produces the same commit id, whether it is tracked on this machine or another.
//!
//! The walk is deliberate rather than delegated to `git2::Index::add_all`: libgit2
//! refuses that call against a bare repository, which is exactly what `.skot/mirror.git`
//! is. What enters the tree is decided by the directory's own `.gitignore` files, read
//! the way git reads them. skot imposes no file-name list of its own, so a `.tmp` file a
//! project legitimately keeps is tracked unless that project says otherwise.
//!
//! `.git` and `.skot` are the two exceptions: they are repository and tool state rather
//! than source, and a snapshot that captured them would embed a repository inside itself.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// What tracking one directory produced.
#[derive(Debug)]
pub struct Snapshot {
    /// The synthetic commit's id.
    pub commit: git2::Oid,
    /// How many files the tree holds.
    pub files: usize,
}

/// Read `dir` into `repo` and commit it as `track <ref_name>`.
///
/// Author and committer are `skot <skot@localhost>` at the Unix epoch, so the id depends
/// only on the tree and the ref name. A second run over unchanged files is a no-op at
/// the object level.
pub fn snapshot(repo: &git2::Repository, dir: &Path, ref_name: &str) -> Result<Snapshot> {
    let (tree, files) = tree_for(repo, dir).context(format!("reading {}", dir.display()))?;
    if files == 0 {
        bail!(
            "`{}` holds no files to track.\n\
             Everything in it is ignored, or it contains only `.git`/`.skot` state.",
            dir.display()
        );
    }
    let tree = repo
        .find_tree(tree)
        .context("reading the tree that was just written")?;
    let signature = git2::Signature::new("skot", "skot@localhost", &git2::Time::new(0, 0))
        .context("building the synthetic commit signature")?;
    let commit = repo
        .commit(
            None,
            &signature,
            &signature,
            &format!("track {ref_name}"),
            &tree,
            &[],
        )
        .context("writing the synthetic commit")?;
    Ok(Snapshot { commit, files })
}

/// One entry in the snapshot: a file from disk, or a directory of more entries.
enum Node {
    File(PathBuf),
    Dir(BTreeMap<OsString, Node>),
}

/// Read `dir` into a tree, honoring its `.gitignore` files, and count the files kept.
///
/// The walker supplies git's own semantics: nested `.gitignore` files, negation, and
/// directory patterns. It is not asked for the global gitignore or for ignore files in
/// parent directories, because an unpacked archive must read the same on every machine.
fn tree_for(repo: &git2::Repository, dir: &Path) -> Result<(git2::Oid, usize)> {
    let mut nodes = BTreeMap::new();
    let mut walk = ignore::WalkBuilder::new(dir);
    walk.hidden(false)
        .parents(false)
        .ignore(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(false)
        .require_git(false)
        .follow_links(false)
        .sort_by_file_name(|a: &OsStr, b: &OsStr| a.cmp(b))
        .filter_entry(|entry| !is_state_dir(entry.file_name()));

    for entry in walk.build() {
        let entry = entry.context(format!("reading {}", dir.display()))?;
        if entry.depth() == 0 {
            continue;
        }
        let Some(kind) = entry.file_type() else {
            continue;
        };
        // Directories are implied by the files below them. Keeping only the files means
        // an empty directory never becomes a tree, which is git's own rule.
        if kind.is_dir() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(dir)
            .with_context(|| format!("reading {}", entry.path().display()))?;
        insert_file(&mut nodes, relative, entry.path().to_path_buf());
    }

    let mut files = 0;
    let tree = build_tree(repo, &nodes, &mut files)?;
    Ok((tree, files))
}

/// `.git` and `.skot` are state, not source, so the walk never enters them.
fn is_state_dir(name: &OsStr) -> bool {
    name == OsStr::new(".git") || name == OsStr::new(".skot")
}

/// Put one relative path into the directory tree.
fn insert_file(nodes: &mut BTreeMap<OsString, Node>, relative: &Path, path: PathBuf) {
    let mut current = nodes;
    let mut parts = relative.iter().peekable();
    while let Some(part) = parts.next() {
        let name = part.to_os_string();
        if parts.peek().is_none() {
            current.insert(name, Node::File(path));
            return;
        }
        let next = current
            .entry(name)
            .or_insert_with(|| Node::Dir(BTreeMap::new()));
        let Node::Dir(children) = next else {
            // The file system cannot hold a file and a child under the same name.
            return;
        };
        current = children;
    }
}

/// Turn the directory tree into git tree objects.
fn build_tree(
    repo: &git2::Repository,
    nodes: &BTreeMap<OsString, Node>,
    files: &mut usize,
) -> Result<git2::Oid> {
    let mut builder = repo.treebuilder(None).context("starting a tree")?;
    for (name, node) in nodes {
        match node {
            Node::File(path) => add_file(repo, &mut builder, name, path, files)?,
            Node::Dir(children) => {
                let subtree = build_tree(repo, children, files)?;
                builder
                    .insert(name, subtree, 0o040000)
                    .context(format!("adding the directory {}", name.to_string_lossy()))?;
            }
        }
    }
    builder.write().context("writing the tree")
}

/// Add one file or symlink to `builder`.
fn add_file(
    repo: &git2::Repository,
    builder: &mut git2::TreeBuilder<'_>,
    name: &OsStr,
    path: &Path,
    files: &mut usize,
) -> Result<()> {
    let meta = std::fs::symlink_metadata(path).context(format!("inspecting {}", path.display()))?;
    let kind = meta.file_type();

    if kind.is_symlink() {
        let target =
            std::fs::read_link(path).context(format!("reading the link {}", path.display()))?;
        let blob = repo
            .blob(&path_bytes(&target))
            .context(format!("storing {}", path.display()))?;
        builder
            .insert(name, blob, 0o120000)
            .context(format!("adding {}", path.display()))?;
        *files += 1;
    } else if kind.is_file() {
        let bytes = std::fs::read(path).context(format!("reading {}", path.display()))?;
        let blob = repo
            .blob(&bytes)
            .context(format!("storing {}", path.display()))?;
        builder
            .insert(name, blob, file_mode(&meta))
            .context(format!("adding {}", path.display()))?;
        *files += 1;
    } else {
        // A socket, fifo or device cannot be represented in a git tree, and a source
        // archive has no business carrying one.
        log::debug!(target: "track", "skipping special file {}", path.display());
    }
    Ok(())
}

/// The mode git records for a regular file.
fn file_mode(meta: &std::fs::Metadata) -> i32 {
    if is_executable(meta) {
        0o100755
    } else {
        0o100644
    }
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    false
}

/// A symlink target as the bytes git stores.
#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt as _;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().replace('\\', "/").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> (tempfile::TempDir, git2::Repository) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init_bare(tmp.path().join("mirror.git")).unwrap();
        (tmp, repo)
    }

    #[test]
    fn a_directory_becomes_a_tree_of_its_files() {
        let (tmp, repo) = repo();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), "a\n").unwrap();
        std::fs::write(src.join("sub/b.txt"), "b\n").unwrap();

        let done = snapshot(&repo, &src, "upstream").unwrap();

        assert_eq!(done.files, 2);
        let commit = repo.find_commit(done.commit).unwrap();
        assert_eq!(commit.message().unwrap(), "track upstream");
        assert_eq!(commit.author().name().unwrap(), "skot");
        assert_eq!(commit.author().email().unwrap(), "skot@localhost");
        assert_eq!(commit.author().when().seconds(), 0);
        let tree = commit.tree().unwrap();
        assert!(tree.get_path(Path::new("a.txt")).is_ok());
        assert!(tree.get_path(Path::new("sub/b.txt")).is_ok());
    }

    #[test]
    fn the_same_files_produce_the_same_commit_every_time() {
        let (tmp, repo) = repo();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), "a\n").unwrap();

        let first = snapshot(&repo, &src, "v1.0.0").unwrap();
        let second = snapshot(&repo, &src, "v1.0.0").unwrap();

        assert_eq!(first.commit, second.commit);
    }

    #[test]
    fn gitignore_patterns_decide_what_reaches_the_tree() {
        let (tmp, repo) = repo();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join(".git")).unwrap();
        std::fs::create_dir_all(src.join(".skot")).unwrap();
        std::fs::write(
            src.join(".gitignore"),
            "*.tmp\n*.swp\n*~\n.DS_Store\nThumbs.db\n.#*\n\\#*#\n",
        )
        .unwrap();
        std::fs::write(src.join("keep.txt"), "keep\n").unwrap();
        std::fs::write(src.join(".DS_Store"), "junk").unwrap();
        std::fs::write(src.join("Thumbs.db"), "junk").unwrap();
        std::fs::write(src.join("scratch.tmp"), "junk").unwrap();
        std::fs::write(src.join("editor.swp"), "junk").unwrap();
        std::fs::write(src.join("backup~"), "junk").unwrap();
        std::fs::write(src.join(".#lock"), "junk").unwrap();
        std::fs::write(src.join("#autosave#"), "junk").unwrap();
        std::fs::write(src.join(".git/config"), "junk").unwrap();
        std::fs::write(src.join(".skot/state.json"), "junk").unwrap();

        let done = snapshot(&repo, &src, "upstream").unwrap();

        let tree = repo.find_commit(done.commit).unwrap().tree().unwrap();
        let names: Vec<_> = tree
            .iter()
            .map(|entry| entry.name().unwrap().to_owned())
            .collect();
        assert_eq!(done.files, 2, "tracked: {names:?}");
        assert_eq!(names, [".gitignore", "keep.txt"]);
    }

    #[test]
    fn a_negated_gitignore_pattern_puts_a_file_back() {
        let (tmp, repo) = repo();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join(".gitignore"), "*.log\n!keep.log\n").unwrap();
        std::fs::write(src.join("drop.log"), "junk\n").unwrap();
        std::fs::write(src.join("keep.log"), "keep\n").unwrap();

        let done = snapshot(&repo, &src, "upstream").unwrap();

        assert_eq!(done.files, 2, "the ignore file and the re-included log");
        let tree = repo.find_commit(done.commit).unwrap().tree().unwrap();
        assert!(tree.get_path(Path::new("drop.log")).is_err());
        assert!(tree.get_path(Path::new("keep.log")).is_ok());
    }

    #[test]
    fn a_nested_gitignore_is_read_too() {
        let (tmp, repo) = repo();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("sub/.gitignore"), "*.o\n").unwrap();
        std::fs::write(src.join("sub/a.o"), "junk\n").unwrap();
        std::fs::write(src.join("sub/a.c"), "keep\n").unwrap();

        let done = snapshot(&repo, &src, "upstream").unwrap();

        assert_eq!(done.files, 2, "a.c and the nested ignore file");
        let tree = repo.find_commit(done.commit).unwrap().tree().unwrap();
        assert!(tree.get_path(Path::new("sub/a.o")).is_err());
        assert!(tree.get_path(Path::new("sub/a.c")).is_ok());
    }

    #[test]
    fn a_directory_with_nothing_to_track_is_refused() {
        let (tmp, repo) = repo();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join(".git")).unwrap();
        std::fs::write(src.join(".git/config"), "junk").unwrap();

        let err = snapshot(&repo, &src, "upstream").unwrap_err().to_string();

        assert!(err.contains("holds no files to track"), "got: {err}");
    }

    #[test]
    fn an_empty_directory_contributes_no_empty_subtree() {
        let (tmp, repo) = repo();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("empty")).unwrap();
        std::fs::write(src.join("keep.txt"), "keep\n").unwrap();

        let done = snapshot(&repo, &src, "upstream").unwrap();

        assert_eq!(done.files, 1);
        let tree = repo.find_commit(done.commit).unwrap().tree().unwrap();
        assert!(tree.get_path(Path::new("empty")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn an_executable_bit_survives_and_a_symlink_is_a_link() {
        use std::os::unix::fs::PermissionsExt as _;

        let (tmp, repo) = repo();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("run.sh"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(src.join("run.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::os::unix::fs::symlink("run.sh", src.join("link")).unwrap();

        let done = snapshot(&repo, &src, "upstream").unwrap();

        let tree = repo.find_commit(done.commit).unwrap().tree().unwrap();
        let script = tree.get_path(Path::new("run.sh")).unwrap();
        assert_eq!(script.filemode(), 0o100755);
        let link = tree.get_path(Path::new("link")).unwrap();
        assert_eq!(link.filemode(), 0o120000);
        let blob = repo.find_blob(link.id()).unwrap();
        assert_eq!(blob.content(), b"run.sh");
    }
}
