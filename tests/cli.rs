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

/// A bare repository with one commit on `main`: an upstream that costs no network.
fn bare_with_one_commit(path: &Path) {
    let repo = git2::Repository::init_bare(path).unwrap();
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
}

/// A project with a mirror holding one commit, a sandbox, and no patches.
fn bare_project(root: &Path) -> Layout {
    // The scope the spawned commands resolve from config, which defaults to one tree
    // per series: a fixture whose own Layout disagreed would read the wrong sandbox.
    let layout = Layout::new(root).with_per_series_sandbox(true);
    std::fs::create_dir_all(layout.patches_dir()).unwrap();
    std::fs::create_dir_all(layout.mirror_dir()).unwrap();

    bare_with_one_commit(&layout.mirror_dir());

    std::fs::write(
        layout.config_file(),
        "[upstream]\nurl = \"unused\"\ndefault-ref = \"main\"\n",
    )
    .unwrap();

    let mirror = Mirror::open(&layout).unwrap();
    skot::git::sandbox::create(&layout, &mirror).unwrap();
    layout
}

/// The path of a fresh upstream repository: `init --url` can name it without a network.
fn upstream(root: &Path) -> String {
    let path = root.join("upstream.git");
    bare_with_one_commit(&path);
    path.display().to_string()
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
    write_series(&layout, "a.patch", patch_one());

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
    write_series(&layout, "a.patch", patch_one());

    let out = skot(tmp.path(), &["patch", "apply", "main", "--json"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    serde_json::from_str::<serde_json::Value>(&text)
        .unwrap_or_else(|e| panic!("stdout is not one JSON document: {e}\n---\n{text}\n---"));
    assert_eq!(sandbox_text(&layout), "first\n");
}

/// The enabled set lives in the series file, so `disable`/`enable` steer what `apply`
/// lands, and the whole flow is scriptable: named patches, `--json`, no picker.
#[test]
fn enable_and_disable_steer_what_apply_lands() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = bare_project(tmp.path());
    write_series(&layout, "a.patch", patch_one());
    std::fs::write(
        layout.patches_dir().join("b.patch"),
        "diff --git a/f.txt b/f.txt\nindex 3333333..4444444 100644\n--- a/f.txt\n+++ b/f.txt\n@@ -1,1 +1,1 @@\n-first\n+second\n",
    )
    .unwrap();
    std::fs::write(layout.series_file(), "a.patch\nb.patch\n").unwrap();

    // Disable the second patch: its line is marked `#!`, and apply leaves it out.
    let out = skot(tmp.path(), &["patch", "disable", "b.patch", "--json"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(
        std::fs::read_to_string(layout.series_file()).unwrap(),
        "a.patch\n#!b.patch\n"
    );

    let out = skot(tmp.path(), &["patch", "apply", "main", "--json"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(sandbox_text(&layout), "first\n");

    // Enable puts the line back; apply realizes the enabled set again.
    let out = skot(tmp.path(), &["patch", "enable", "b.patch", "--json"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(
        std::fs::read_to_string(layout.series_file()).unwrap(),
        "a.patch\nb.patch\n"
    );

    let out = skot(tmp.path(), &["patch", "apply", "main", "--json"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(sandbox_text(&layout), "second\n");
}

/// `patch move` reorders, and under `--json` it still owes the caller exactly one
/// document, with the positions it moved between.
#[test]
fn move_json_is_a_single_document_and_the_series_follows() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = bare_project(tmp.path());
    std::fs::write(layout.patches_dir().join("a.patch"), "a\n").unwrap();
    std::fs::write(layout.patches_dir().join("b.patch"), "b\n").unwrap();
    std::fs::write(layout.series_file(), "a.patch\nb.patch\n").unwrap();

    let out = skot(
        tmp.path(),
        &["patch", "move", "b.patch", "--to", "1", "--json"],
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));

    let text = stdout(&out);
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("stdout is not one JSON document: {e}\n---\n{text}\n---"));
    assert_eq!(parsed["ok"], true, "{text}");
    assert_eq!(parsed["patch"], "b.patch", "{text}");
    assert_eq!(parsed["from"], 2, "{text}");
    assert_eq!(parsed["to"], 1, "{text}");
    assert_eq!(
        std::fs::read_to_string(layout.series_file()).unwrap(),
        "b.patch\na.patch\n",
        "the series file followed the move"
    );
    assert_eq!(
        std::fs::read_to_string(layout.patches_dir().join("b.patch")).unwrap(),
        "b\n",
        "and the files kept the names they were created with"
    );
    assert_eq!(
        std::fs::read_to_string(layout.patches_dir().join("a.patch")).unwrap(),
        "a\n"
    );
}

/// Deleting a file is the one thing `skot patch` cannot take back, so a caller has to
/// say `--yes`.
#[test]
fn drop_deletes_only_when_asked_to() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = bare_project(tmp.path());
    write_series(&layout, "a.patch", patch_one());

    // No terminal here, so without --yes there is nobody to ask. The refusal goes to
    // stderr; under --json the logger is off by contract, and only the exit code shows.
    let out = skot(tmp.path(), &["patch", "drop", "a.patch"]);
    assert_ne!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(stderr(&out).contains("--yes"), "stderr: {}", stderr(&out));
    assert!(
        layout.patches_dir().join("a.patch").is_file(),
        "nothing was deleted"
    );

    let out = skot(tmp.path(), &["patch", "drop", "a.patch", "--yes", "--json"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("stdout is not one JSON document: {e}\n---\n{text}\n---"));
    assert_eq!(parsed["ok"], true, "{text}");
    assert_eq!(parsed["file_removed"], true, "{text}");
    assert!(!layout.patches_dir().join("a.patch").exists());
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
        "a.patch",
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
    write_series(&layout, "a.patch", patch_one());

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
    write_series(&layout, "a.patch", patch_one());

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
    write_series(&layout, "a.patch", patch_one());

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
    write_series(&layout, "a.patch", patch_one());

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

/// SKOT_SERIES names the series when nothing else does. The unit tests share one
/// process, so the variable can only be checked here, where the real binary gets a
/// clean environment: no pointer, two series, and the variable as the only answer.
#[test]
fn skot_series_environment_variable_names_the_series() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = bare_project(tmp.path());
    std::fs::create_dir_all(layout.series_root().join("exp")).unwrap();
    write_series(&layout, "a.patch", patch_one());
    let exp = layout.with_series("exp").unwrap();
    std::fs::write(exp.patches_dir().join("b.patch"), patch_one()).unwrap();
    std::fs::write(exp.series_file(), "b.patch\n").unwrap();

    // Without the variable this is the ambiguity refusal: several series, nothing
    // naming the active one. Not `--json`: with the flag the logger is off, and an
    // error before any command runs has no document to print.
    let refused = skot(tmp.path(), &["check"]);
    assert_ne!(
        refused.status.code(),
        Some(0),
        "stderr: {}",
        stderr(&refused)
    );
    assert!(
        stderr(&refused).contains("several series"),
        "stderr: {}",
        stderr(&refused)
    );

    let out = Command::new(env!("CARGO_BIN_EXE_skot"))
        .arg("--dir")
        .arg(tmp.path())
        .args(["check", "--json"])
        .env("SKOT_SERIES", "exp")
        .output()
        .expect("running skot");
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let doc: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(doc["series_name"], "exp");
}

/// A run that cannot ask refuses the one `init` question that has no default.
///
/// An upstream URL is a fact about the project, not a value a default could stand for,
/// so both ways of not being able to ask (`--yes`, and a missing terminal) take the same
/// route: exit 2, clap's usage shape, the flag that answers it, and nothing written.
#[test]
fn an_init_that_cannot_ask_refuses_to_guess_an_upstream_url() {
    let tmp = tempfile::tempdir().unwrap();
    for (name, args) in [("yes", vec!["init", "-y"]), ("silent", vec!["init"])] {
        let project = tmp.path().join(name);
        std::fs::create_dir_all(&project).unwrap();

        let out = skot(&project, &args);
        assert_eq!(out.status.code(), Some(2), "{name}: {}", stderr(&out));
        let err = stderr(&out);
        assert!(err.contains("--url"), "{name}: names the flag: {err}");
        assert!(err.contains("Usage: skot init"), "{name}: {err}");
        assert!(
            !project.join("skot.toml").exists(),
            "{name}: an init that refused to answer must write nothing"
        );
    }
}

/// Every question that cannot be asked is answered the same way: with the default it
/// would have offered. `--yes` and a terminal-less run must write the same project.
#[test]
fn init_takes_the_default_of_every_question_it_cannot_ask() {
    let tmp = tempfile::tempdir().unwrap();
    let url = upstream(tmp.path());

    let mut configs = Vec::new();
    for name in ["yes", "silent"] {
        let project = tmp.path().join(name);
        std::fs::create_dir_all(project.join("target")).unwrap();
        let mut args = vec!["init"];
        if name == "yes" {
            args.push("-y");
        }
        args.extend(["--url", url.as_str()]);

        let out = skot(&project, &args);
        assert_eq!(out.status.code(), Some(0), "{name}: {}", stderr(&out));

        let config = std::fs::read_to_string(project.join("skot.toml")).unwrap();
        assert!(
            config.contains(&url),
            "{name}: the URL it was given: {config}"
        );
        assert!(
            config.lines().any(|l| l.starts_with("policy = ")),
            "{name}: the policy default is taken, not left commented: {config}"
        );
        assert!(
            config.lines().any(|l| l == "keep = [\"target\"]"),
            "{name}: every keep candidate present is protected: {config}"
        );
        assert!(
            config.lines().any(|l| l.starts_with("# default-ref")),
            "{name}: a question with no default stays unset: {config}"
        );

        let layout = Layout::new(&project);
        let series = std::fs::read_to_string(layout.series_file())
            .unwrap_or_else(|e| panic!("{name}: the default series exists: {e}"));
        assert!(
            series
                .lines()
                .all(|line| line.is_empty() || line.starts_with('#')),
            "{name}: the default series starts empty of patches: {series}"
        );
        let mirror = git2::Repository::open(layout.mirror_dir())
            .unwrap_or_else(|e| panic!("{name}: the mirror was fetched: {e}"));
        assert!(
            mirror.refname_to_id("refs/heads/main").is_ok(),
            "{name}: the upstream landed in the mirror"
        );
        configs.push(config);
    }
    assert_eq!(
        configs[0], configs[1],
        "a terminal-less run and --yes must write the same project"
    );
}

/// A question with no default is the same refusal everywhere: exit 2, clap's usage shape,
/// and the flag that answers it. `--yes` does not invent an answer it cannot know.
#[test]
fn a_question_nothing_can_answer_is_a_usage_error() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = bare_project(tmp.path());
    // Two patches, one of each state: every picker below has something real to ask about,
    // so an empty-selection precondition cannot answer the question instead of the asker.
    std::fs::write(layout.patches_dir().join("a.patch"), patch_one()).unwrap();
    std::fs::write(layout.patches_dir().join("b.patch"), patch_one()).unwrap();
    std::fs::write(layout.series_file(), "a.patch\n#!b.patch\n").unwrap();
    // The pointer first: a second series without one would refuse every command for
    // ambiguity instead of reaching its question.
    let used = skot(tmp.path(), &["series", "use", "default"]);
    assert_eq!(used.status.code(), Some(0), "fixture: {}", stderr(&used));
    // `series drop` needs a series that is not active; `series undo` needs an undo
    // buffer, which is checked before the question is reached at all.
    std::fs::create_dir_all(layout.series_root().join("exp")).unwrap();
    std::fs::create_dir_all(layout.undo_entry()).unwrap();

    let cases: [(&[&str], &str); 7] = [
        (&["patch", "enable"], "skot patch enable"),
        (&["patch", "enable", "-y"], "skot patch enable"),
        (&["patch", "disable"], "skot patch disable"),
        (&["patch", "move"], "skot patch move"),
        (&["patch", "drop", "a.patch"], "skot patch drop"),
        (&["series", "drop", "exp"], "skot series drop"),
        (&["series", "undo"], "skot series undo"),
    ];
    for (args, advice) in cases {
        let out = skot(tmp.path(), args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", stderr(&out));
        let err = stderr(&out);
        assert!(err.contains(advice), "{args:?}: names the flag: {err}");
        assert!(err.contains("Usage: skot"), "{args:?}: clap-shaped: {err}");
    }
    assert!(
        layout.patches_dir().join("a.patch").is_file(),
        "a refusal deletes nothing"
    );
}

/// `--yes` must reach the confirmation `series undo` carries as its own flag.
///
/// The claim is about the question, not the swap that follows it: nothing here says the
/// undo succeeds, only that a `--yes` run is never told it cannot ask. Both spellings
/// count: the subcommand's own long flag, and the global one given before the command.
#[test]
fn yes_reaches_series_undo() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = bare_project(tmp.path());
    std::fs::create_dir_all(layout.undo_entry()).unwrap();

    for args in [
        vec!["series", "undo", "--yes"],
        vec!["-y", "series", "undo"],
    ] {
        let out = skot(tmp.path(), &args);
        assert_eq!(out.status.code(), Some(0), "{args:?}: {}", stderr(&out));
        let err = stderr(&out);
        assert!(
            !err.contains("cannot ask"),
            "{args:?}: a --yes run must not be refused for having nobody to ask: {err}"
        );
    }
}

/// `--hook` answers the question the hook wizard would have asked.
#[test]
fn the_hook_flag_lands_without_a_wizard() {
    let tmp = tempfile::tempdir().unwrap();
    let url = upstream(tmp.path());
    let project = tmp.path().join("hooked");
    std::fs::create_dir_all(&project).unwrap();

    let out = skot(
        &project,
        &["init", "-y", "--url", &url, "--hook", "build=make"],
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let config = std::fs::read_to_string(project.join("skot.toml")).unwrap();
    assert!(
        config.contains("build = \"make\""),
        "the hook from the flag: {config}"
    );
}

/// On a pipe, `show` is the patch file: same bytes, nothing added, so it can be
/// redirected into a file or a pager and land exactly where it came from.
#[test]
fn patch_show_pipes_the_exact_patch_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = bare_project(tmp.path());
    write_series(&layout, "a.patch", patch_one());

    let out = skot(tmp.path(), &["patch", "show", "a.patch"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), patch_one());
}

#[test]
fn patch_show_stat_counts_the_files_without_printing_the_diff() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = bare_project(tmp.path());
    write_series(&layout, "a.patch", patch_one());

    let out = skot(tmp.path(), &["patch", "show", "a.patch", "--stat"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("1 file changed"), "{text}");
    assert!(text.contains("1 insertion(+)"), "{text}");
    assert!(text.contains("1 deletion(-)"), "{text}");
    assert!(!text.contains("@@"), "--stat leaves the body out: {text}");
}

#[test]
fn patch_show_json_carries_the_text_and_the_counts() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = bare_project(tmp.path());
    write_series(&layout, "a.patch", patch_one());

    let out = skot(tmp.path(), &["patch", "show", "a.patch", "--json"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let doc: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(doc["series_name"], "default");
    let patch = &doc["patches"][0];
    assert_eq!(patch["text"], patch_one());
    assert_eq!(patch["added"], 1);
    assert_eq!(patch["removed"], 1);
    assert_eq!(patch["enabled"], true);
    assert_eq!(patch["position"], 1);
}

#[test]
fn patch_show_without_a_name_needs_an_open_work_area() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = bare_project(tmp.path());
    write_series(&layout, "a.patch", patch_one());

    let out = skot(tmp.path(), &["patch", "show"]);
    assert_ne!(out.status.code(), Some(0), "stdout: {}", stdout(&out));
    assert!(stderr(&out).contains("name the patch"), "{}", stderr(&out));
}

/// Under `--json` a failure is a document, not a swallowed log line: the logger is
/// off (stdout carries the machine's output), so without this an agent would learn
/// only "exit 1". This is the highest-impact bug every evaluation hit.
#[test]
fn a_json_failure_is_one_document_with_the_advice() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = bare_project(tmp.path());
    write_series(&layout, "a.patch", patch_one());

    let out = skot(tmp.path(), &["patch", "show", "nope", "--json"]);
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    let doc: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("stdout is one JSON document: {err}: {text}"));
    assert_eq!(doc["ok"], serde_json::json!(false));
    let message = doc["error"].as_str().expect("the message is a string");
    assert!(message.contains("nope"), "{message}");
}

/// A question only a flag can answer is a usage error (exit 2) in human mode; under
/// `--json` it is the same envelope, so the refusal is still readable by a script.
#[test]
fn a_json_usage_error_is_a_document_and_exits_two() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = bare_project(tmp.path());
    write_series(&layout, "a.patch", patch_one());

    // No name and no terminal: `move` cannot ask which patch to move.
    let out = skot(tmp.path(), &["patch", "move", "--json"]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    let doc: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("stdout is one JSON document: {err}: {text}"));
    assert_eq!(doc["ok"], serde_json::json!(false));
    let message = doc["error"].as_str().expect("the message is a string");
    assert!(message.contains("answer it with"), "{message}");
}
