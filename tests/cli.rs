//! End-to-end tests that run the real binary.
//!
//! These exist for the things a unit test cannot see, and they are not about coverage:
//!
//! - **stdout.** `--json` promises exactly one parseable document. Every in-process test of
//!   that promise is worthless, because the JSON goes to the real stdout through `println!`
//!   and the test cannot see it. Here it can.
//! - **Exit codes and stderr**, which is what a shell script or a CI job actually reads.
//!
//! Everything else is cheaper to test in process, and is tested there.

use std::path::Path;
use std::process::{Command, Output};

use skot::git::mirror::Mirror;
use skot::layout::Layout;

/// Run the binary in `dir` and hand back the raw result.
fn skot(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_skot"))
        .arg("--dir")
        .arg(dir)
        .args(args)
        .output()
        .expect("running skot")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A project with a mirror holding one commit, a sandbox, and no patches.
fn bare_project(root: &Path) -> Layout {
    let layout = Layout::new(root);
    std::fs::create_dir_all(layout.patches_dir()).unwrap();
    std::fs::create_dir_all(layout.mirror_dir()).unwrap();

    let repo = git2::Repository::init_bare(layout.mirror_dir()).unwrap();
    let tree = {
        let mut b = repo.treebuilder(None).unwrap();
        b.insert("f.txt", repo.blob(b"base\n").unwrap(), 0o100644)
            .unwrap();
        b.write().unwrap()
    };
    let sig = git2::Signature::now("Test", "test@example.com").unwrap();
    let tree_obj = repo.find_tree(tree).unwrap();
    let parents: &[&git2::Commit] = &[];
    let commit = repo
        .commit(Some("HEAD"), &sig, &sig, "base", &tree_obj, parents)
        .unwrap();
    // A branch points at a *commit*, not a tree. Getting this wrong leaves the mirror with a
    // ref that resolves to nothing, and every command that asks for `main` fails to find it.
    repo.reference("refs/heads/main", commit, true, "init")
        .unwrap();

    std::fs::write(
        layout.config_file(),
        "[upstream]\nurl = \"unused\"\ndefault-ref = \"main\"\n",
    )
    .unwrap();

    let mirror = Mirror::open(&layout).unwrap();
    skot::git::sandbox::create(&layout, &mirror).unwrap();
    layout
}

fn patch_one() -> &'static str {
    "diff --git a/f.txt b/f.txt\nindex 1111111..2222222 100644\n--- a/f.txt\n+++ b/f.txt\n@@ -1,1 +1,1 @@\n-base\n+first\n"
}

fn write_series(layout: &Layout, name: &str, body: &str) {
    std::fs::write(layout.patches_dir().join(name), body).unwrap();
    std::fs::write(layout.series_file(), format!("{name}\n")).unwrap();
}

fn sandbox_text(layout: &Layout) -> String {
    std::fs::read_to_string(layout.sandbox_dir().join("f.txt")).unwrap()
}

/// `--json` must produce exactly one document, however many commands went into producing it.
///
/// `build` runs `sync` and `apply` as phases. If either of those emits JSON when the flag is
/// set, stdout carries three documents where the caller expects one. Both are written by
/// `println!` inside the process, so this is the only place the claim can actually be checked.
#[test]
fn build_json_is_a_single_document() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = bare_project(tmp.path());
    write_series(&layout, "0001-a.patch", patch_one());

    let out = skot(tmp.path(), &["build", "--only", "apply", "--json"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));

    let text = stdout(&out);
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("stdout is not one JSON document: {e}\n---\n{text}\n---"));
    assert!(
        parsed.get("phases").is_some() || parsed.get("ok").is_some(),
        "expected build's own document, got: {text}"
    );
}

/// The same flag on `apply` alone, which is the phase most likely to start regressing, since
/// it is the one that has to write a report.
#[test]
fn apply_json_is_a_single_document() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = bare_project(tmp.path());
    write_series(&layout, "0001-a.patch", patch_one());

    let out = skot(tmp.path(), &["patch", "apply", "main", "--json"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    serde_json::from_str::<serde_json::Value>(&text)
        .unwrap_or_else(|e| panic!("stdout is not one JSON document: {e}\n---\n{text}\n---"));
    assert_eq!(sandbox_text(&layout), "first\n");
}

/// `patch move` reorders and renumbers, and under `--json` it still owes the caller exactly
/// one document, with the positions it moved between.
#[test]
fn move_json_is_a_single_document_and_the_series_follows() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = bare_project(tmp.path());
    std::fs::write(layout.patches_dir().join("0001-a.patch"), "a\n").unwrap();
    std::fs::write(layout.patches_dir().join("0002-b.patch"), "b\n").unwrap();
    std::fs::write(layout.series_file(), "0001-a.patch\n0002-b.patch\n").unwrap();

    let out = skot(
        tmp.path(),
        &["patch", "move", "0002-b.patch", "--to", "1", "--json"],
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));

    let text = stdout(&out);
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("stdout is not one JSON document: {e}\n---\n{text}\n---"));
    assert_eq!(parsed["ok"], true, "{text}");
    assert_eq!(parsed["patch"], "0002-b.patch", "{text}");
    assert_eq!(parsed["from"], 2, "{text}");
    assert_eq!(parsed["to"], 1, "{text}");
    assert_eq!(
        std::fs::read_to_string(layout.series_file()).unwrap(),
        "0001-b.patch\n0002-a.patch\n",
        "the series file followed the move"
    );
    assert_eq!(
        std::fs::read_to_string(layout.patches_dir().join("0001-b.patch")).unwrap(),
        "b\n"
    );
    assert_eq!(
        std::fs::read_to_string(layout.patches_dir().join("0002-a.patch")).unwrap(),
        "a\n"
    );
}

/// Deleting a file is the one thing `skot patch` cannot take back, so a caller has to
/// say `--yes`.
#[test]
fn drop_deletes_only_when_asked_to() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = bare_project(tmp.path());
    write_series(&layout, "0001-a.patch", patch_one());

    // No terminal here, so without --yes there is nobody to ask. The refusal goes to
    // stderr; under --json the logger is off by contract, and only the exit code shows.
    let out = skot(tmp.path(), &["patch", "drop", "0001-a.patch"]);
    assert_ne!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(stderr(&out).contains("--yes"), "stderr: {}", stderr(&out));
    assert!(
        layout.patches_dir().join("0001-a.patch").is_file(),
        "nothing was deleted"
    );

    let out = skot(
        tmp.path(),
        &["patch", "drop", "0001-a.patch", "--yes", "--json"],
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("stdout is not one JSON document: {e}\n---\n{text}\n---"));
    assert_eq!(parsed["ok"], true, "{text}");
    assert_eq!(parsed["file_removed"], true, "{text}");
    assert!(!layout.patches_dir().join("0001-a.patch").exists());
}

/// A failing patch must exit non-zero and explain itself on stderr, because that is what a CI
/// job reads. A silent non-zero exit is worse than an error message.
#[test]
fn a_series_that_does_not_apply_fails_loudly() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = bare_project(tmp.path());
    // A patch that expects content the base does not have.
    write_series(
        &layout,
        "0001-a.patch",
        "diff --git a/f.txt b/f.txt\nindex 1111111..2222222 100644\n--- a/f.txt\n+++ b/f.txt\n@@ -1,1 +1,1 @@\n-nonexistent\n+first\n",
    );

    let out = skot(tmp.path(), &["patch", "apply", "main"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected failure, stdout: {}",
        stdout(&out)
    );
    assert!(
        stderr(&out).contains("does not apply") || stdout(&out).contains("does not apply"),
        "the failure should say the series does not apply.\nstdout: {}\nstderr: {}",
        stdout(&out),
        stderr(&out)
    );
}

/// A hook's own output must not land in front of the JSON document.
///
/// Hooks are build commands, and build commands print — `cargo`, `npm`, and a bare `echo`
/// all write to stdout. Left inherited, that output prefixes the document and
/// `skot build --json` stops parsing. This fails in the caller's script rather than in
/// skot, which is the worst place for it to fail.
#[test]
fn build_json_survives_a_noisy_hook() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = bare_project(tmp.path());
    write_series(&layout, "0001-a.patch", patch_one());

    std::fs::write(
        layout.config_file(),
        "[upstream]\nurl = \"unused\"\ndefault-ref = \"main\"\n\
         \n[hooks]\npre-build = \"echo NOISE; cat f.txt\"\n",
    )
    .unwrap();

    let out = skot(tmp.path(), &["build", "--only", "build", "--json"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));

    let text = stdout(&out);
    serde_json::from_str::<serde_json::Value>(&text)
        .unwrap_or_else(|e| panic!("stdout is not one JSON document: {e}\n---\n{text}\n---"));

    // The hook's output is not thrown away either — it moves to stderr, where a person
    // watching the build still sees it.
    assert!(
        stderr(&out).contains("NOISE"),
        "the hook should still be visible:\n{}",
        stderr(&out)
    );
}

/// A failed run still owes the caller a document.
///
/// `check`, `apply` and `status` all report bad news in JSON when `--json` is set. If `build`
/// printed nothing on failure, the only thing a script could read would be the exit code, and
/// finding out *why* would mean scraping stderr — which is what `--json` exists to avoid.
#[test]
fn build_json_reports_a_failure_as_a_document() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = bare_project(tmp.path());
    write_series(&layout, "0001-a.patch", patch_one());

    std::fs::write(
        layout.config_file(),
        "[upstream]\nurl = \"unused\"\ndefault-ref = \"main\"\n\
         \n[hooks]\npre-build = \"exit 3\"\n",
    )
    .unwrap();

    let out = skot(tmp.path(), &["build", "--only", "build", "--json"]);
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));

    let text = stdout(&out);
    let doc: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("stdout is not one JSON document: {e}\n---\n{text}\n---"));
    assert_eq!(doc["ok"], serde_json::json!(false));
    assert!(
        doc["error"]
            .as_str()
            .unwrap_or_default()
            .contains("pre_build"),
        "the error should name the hook: {text}"
    );
}

/// `keep` must protect the path it names and nothing else.
///
/// This is the dangerous version of the promise. Restoring one path by checking out a tree
/// that contains only that path makes libgit2 treat it as the truth about the working
/// directory and delete every other tracked file — so the sandbox comes back holding exactly
/// the thing it was asked to protect and none of the work, and the command reports success.
/// `remove_untracked(false)` does not save it, because the deleted files are tracked.
#[test]
fn a_keep_path_survives_a_base_that_dropped_it_without_taking_the_sandbox_with_it() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let layout = bare_project(root);

    // `main` drops `kept.txt` and adds `other.txt`; `old` still has `kept.txt`.
    let repo = git2::Repository::open(layout.mirror_dir()).unwrap();
    let sig = git2::Signature::now("Test", "test@example.com").unwrap();
    let old_tree = {
        let mut b = repo.treebuilder(None).unwrap();
        b.insert("f.txt", repo.blob(b"base\n").unwrap(), 0o100644)
            .unwrap();
        b.insert("kept.txt", repo.blob(b"tracked\n").unwrap(), 0o100644)
            .unwrap();
        b.write().unwrap()
    };
    let old_commit = repo
        .commit(
            None,
            &sig,
            &sig,
            "has kept.txt",
            &repo.find_tree(old_tree).unwrap(),
            &[],
        )
        .unwrap();
    repo.reference("refs/heads/old", old_commit, true, "old")
        .unwrap();

    let new_tree = {
        let mut b = repo.treebuilder(None).unwrap();
        b.insert("f.txt", repo.blob(b"base\n").unwrap(), 0o100644)
            .unwrap();
        b.insert("other.txt", repo.blob(b"from main\n").unwrap(), 0o100644)
            .unwrap();
        b.write().unwrap()
    };
    let new_commit = repo
        .commit(
            None,
            &sig,
            &sig,
            "drops kept.txt",
            &repo.find_tree(new_tree).unwrap(),
            &[],
        )
        .unwrap();
    repo.reference("refs/heads/main", new_commit, true, "moved")
        .unwrap();

    std::fs::write(
        layout.config_file(),
        "[upstream]\nurl = \"unused\"\ndefault-ref = \"main\"\n\
         \n[sandbox]\nkeep = [\"kept.txt\"]\n",
    )
    .unwrap();
    write_series(&layout, "0001-a.patch", patch_one());

    let sandbox = layout.sandbox_dir();
    let mirror = Mirror::open(&layout).unwrap();
    skot::git::sandbox::create(&layout, &mirror).unwrap();
    // Put the sandbox on the base that still has the keep path, so there is something to lose.
    skot::git::sandbox::reset_to(&layout, &old_commit.to_string(), &[]).unwrap();
    assert!(sandbox.join("kept.txt").exists(), "fixture did not set up");

    let out = skot(root, &["patch", "apply", "main"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));

    assert_eq!(
        std::fs::read_to_string(sandbox.join("kept.txt")).unwrap(),
        "tracked\n",
        "the keep path should have been put back"
    );
    // The whole point: the files that were not asked about have to still be there.
    assert!(
        sandbox.join("other.txt").exists(),
        "other.txt came from the new base and must survive a keep restore"
    );
    assert!(
        sandbox.join("f.txt").exists(),
        "f.txt came from the series and must survive a keep restore"
    );
}

#[test]
fn an_untracked_cache_survives_repeated_applies() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let layout = bare_project(root);
    std::fs::write(
        layout.config_file(),
        "[upstream]\nurl = \"unused\"\ndefault-ref = \"main\"\n\
         \n[sandbox]\nkeep = [\"node_modules\"]\n",
    )
    .unwrap();
    write_series(&layout, "0001-a.patch", patch_one());

    let sandbox = layout.sandbox_dir();
    let mirror = Mirror::open(&layout).unwrap();
    skot::git::sandbox::create(&layout, &mirror).unwrap();
    std::fs::create_dir_all(sandbox.join("node_modules/dep")).unwrap();
    std::fs::write(sandbox.join("node_modules/dep/index.js"), "cached\n").unwrap();

    for round in 1..=3 {
        let out = skot(root, &["patch", "apply", "main"]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "round {round}: {}",
            stderr(&out)
        );
        assert_eq!(
            std::fs::read_to_string(sandbox.join("node_modules/dep/index.js")).unwrap(),
            "cached\n",
            "the cache must survive apply {round}"
        );
    }
}

/// Running a command with no project should say so and point at the fix, not panic.
#[test]
fn commands_outside_a_project_fail_with_advice() {
    let tmp = tempfile::tempdir().unwrap();
    let out = skot(tmp.path(), &["check"]);
    assert_ne!(out.status.code(), Some(0));
    let message = format!("{}{}", stdout(&out), stderr(&out));
    assert!(
        message.contains("skot.toml") || message.contains("init"),
        "expected advice about initialising, got: {message}"
    );
    assert!(
        !message.contains("panicked"),
        "a missing project is not a bug in skot:\n{message}"
    );
}
