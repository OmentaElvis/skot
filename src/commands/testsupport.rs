//! Fixtures shared by the command tests.
//!
//! Every command needs the same two things: a [`Layout`] over a temporary project, and a
//! mirror with a commit in it. Building them per-command duplicated about sixty lines across
//! five modules and made them drift, which is worse than a test-only module.
//!
//! Nothing here knows about a command line. Tests drive work functions with a `Layout` and
//! scalars, so no test hauls `clap` in to reach the code it is actually checking.

#![cfg(test)]

use crate::layout::Layout;

/// A temporary project directory with a `skot.toml` that has no upstream URL.
pub fn project() -> (tempfile::TempDir, Layout) {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path()).unwrap();
    let layout = Layout::new(tmp.path());
    std::fs::write(layout.config_file(), "[upstream]\nurl = \"unused\"\n").unwrap();
    std::fs::create_dir_all(layout.patches_dir()).unwrap();
    (tmp, layout)
}

/// A project whose mirror has one commit containing `f.txt` with `base\n`.
///
/// Returns the commit id. `main` points at it.
pub fn mirror_with_one_commit(root: &std::path::Path) -> (Layout, String) {
    let layout = Layout::new(root);
    std::fs::write(layout.config_file(), "[upstream]\nurl = \"unused\"\n").unwrap();
    std::fs::create_dir_all(layout.patches_dir()).unwrap();
    std::fs::create_dir_all(layout.mirror_dir()).unwrap();

    let repo = git2::Repository::init_bare(layout.mirror_dir()).unwrap();
    let tree = {
        let mut b = repo.treebuilder(None).unwrap();
        b.insert("f.txt", repo.blob(b"base\n").unwrap(), 0o100644)
            .unwrap();
        b.write().unwrap()
    };
    let who = git2::Signature::now("t", "t@e").unwrap();
    let sha = repo
        .commit(
            Some("refs/heads/main"),
            &who,
            &who,
            "base",
            &repo.find_tree(tree).unwrap(),
            &[],
        )
        .unwrap();
    repo.set_head("refs/heads/main").unwrap();
    drop(repo);
    (layout, sha.to_string())
}

/// A project whose mirror has one commit, with a sandbox worktree already created and reset.
pub fn project_with_sandbox(root: &std::path::Path) -> (Layout, String) {
    let (layout, sha) = mirror_with_one_commit(root);
    let mirror = crate::git::mirror::Mirror::open(&layout).unwrap();
    crate::git::sandbox::create(&layout, &mirror).unwrap();
    crate::git::sandbox::reset_to(&layout, &sha, &[]).unwrap();
    (layout, sha)
}

/// A patch that turns `base\n` into `first\n`.
pub fn patch_one() -> &'static str {
    "\
diff --git a/f.txt b/f.txt
index 1111111..2222222 100644
--- a/f.txt
+++ b/f.txt
@@ -1,1 +1,1 @@
-base
+first
"
}

/// A patch that turns `first\n` into `second\n`, so it only applies after [`patch_one`].
pub fn patch_two() -> &'static str {
    "\
diff --git a/f.txt b/f.txt
index 3333333..4444444 100644
--- a/f.txt
+++ b/f.txt
@@ -1,1 +1,1 @@
-first
+second
"
}

/// Write patch files and a series listing them, in order.
pub fn write_series(layout: &Layout, patches: &[(&str, &str)]) {
    let mut text = String::new();
    for (name, body) in patches {
        std::fs::write(layout.patches_dir().join(name), body).unwrap();
        text.push_str(name);
        text.push('\n');
    }
    std::fs::write(layout.series_file(), text).unwrap();
}

/// A project with a mirror, a sandbox, and a two-patch series, ready for `apply`.
pub fn applied_two_patch_stack(root: &std::path::Path) -> Layout {
    let (layout, _sha) = project_with_sandbox(root);
    write_series(
        &layout,
        &[("0001-a.patch", patch_one()), ("0002-b.patch", patch_two())],
    );
    std::fs::write(
        layout.config_file(),
        "[upstream]\nurl = \"unused\"\ndefault-ref = \"main\"\n",
    )
    .unwrap();

    crate::commands::apply::apply_locked(&layout, Some("main")).unwrap();
    layout
}
