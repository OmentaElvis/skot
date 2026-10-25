<h1 align="center">skot</h1>

<p align="center">
  <strong>Maintain your changes as a clean patch series, not a painful fork.</strong>
</p>

<p align="center">
  <a href="https://crates.io/crates/skot"><img src="https://img.shields.io/crates/v/skot.svg" alt="Crates.io"></a>
  <a href="https://github.com/OmentaElvis/skot/actions"><img src="https://github.com/OmentaElvis/skot/workflows/CI/badge.svg" alt="CI"></a>
  <a href="https://opensource.org/licenses/MIT"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="License: MIT"></a>
  <a href="https://rust-lang.org"><img src="https://img.shields.io/badge/rust-1.85+-orange.svg" alt="Rust Version"></a>
</p>

---

<img src="assets/skot.svg" alt="skot logo" align="right" width="160">

Every developer has been there: you need one small tweak or critical bugfix in an open-source tool, so you fork the repository. Three months later, upstream is hundreds of commits ahead, your branch is hopelessly stale, and a simple rebase turns into an afternoon of merge conflict shenanigans.

**skot** changes that dynamic. Instead of maintaining an unwieldy, drifting fork, skot lets you keep your modifications as an explicit, ordered series of plain patch files on top of clean upstream code. Each patch stays small, reviewable, and isolated. When upstream moves, your patches replay cleanly, and you can add, drop, reorder, or toggle any change in seconds.

<br clear="right" />

---

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->

- [Why Forks Break Your Heart](#why-forks-break-your-heart)
- [⚡ Base Features at a Glance](#-base-features-at-a-glance)
- [Quick Start](#quick-start)
  - [Install](#install)
  - [60-Second Walkthrough](#60-second-walkthrough)
  - [What a skot tree looks like](#what-a-skot-tree-looks-like)
- [📦 Recipes: Real-World Survival Guides](#-recipes-real-world-survival-guides)
  - [Rescuing abandoned or rejected PRs ("Where PRs go to die")](#rescuing-abandoned-or-rejected-prs-where-prs-go-to-die)
  - [Need that bugfix in production yesterday?](#need-that-bugfix-in-production-yesterday)
  - [Gotta catch 'em all: Stacking multiple community PRs](#gotta-catch-em-all-stacking-multiple-community-prs)
  - [Bringing back a removed feature or legacy workflow](#bringing-back-a-removed-feature-or-legacy-workflow)
  - [Bonus Recipe: No real upstream Git repository? Working with ZIP files and custom hooks](#bonus-recipe-no-real-upstream-git-repository-working-with-zip-files-and-custom-hooks)
    - [The raw preparation flow](#the-raw-preparation-flow)
    - [Taming the prep with custom hooks](#taming-the-prep-with-custom-hooks)
- [The Authoring Loop](#the-authoring-loop)
- [Pull requests from any forge](#pull-requests-from-any-forge)
- [CI, Automation, and Autonomous Agents](#ci-automation-and-autonomous-agents)
  - [Automatic non-interactive detection](#automatic-non-interactive-detection)
  - [Autonomous next-step guidance ("next" hints)](#autonomous-next-step-guidance-next-hints)
  - [Machine-readable, stable JSON output (`--json`)](#machine-readable-stable-json-output---json)
  - [Reliable CI matrix testing](#reliable-ci-matrix-testing)
- [Rebasing Across Upstream Releases](#rebasing-across-upstream-releases)
  - [Why patches become invalid (History churn and anchor drift)](#why-patches-become-invalid-history-churn-and-anchor-drift)
    - [Example: A patch before rebase (anchored to v1.0.0)](#example-a-patch-before-rebase-anchored-to-v100)
  - [How `skot rebase` realigns the patch coordinates](#how-skot-rebase-realigns-the-patch-coordinates)
    - [The patch after running `skot rebase v2.0.0`:](#the-patch-after-running-skot-rebase-v200)
  - [Branching to a new series with `--into`](#branching-to-a-new-series-with---into)
  - [Atomic safety and undo](#atomic-safety-and-undo)
- [The Series Format](#the-series-format)
- [Multiple Series](#multiple-series)
- [Cumulative Verification (`check`)](#cumulative-verification-check)
- [The Mirror and the Sandbox](#the-mirror-and-the-sandbox)
- [Command Reference](#command-reference)
  - [Project commands](#project-commands)
  - [Patch commands](#patch-commands)
  - [Series commands](#series-commands)
- [MISC](#misc)
  - [How a ref gets chosen](#how-a-ref-gets-chosen)
  - [Terminal Output and Logging](#terminal-output-and-logging)
  - [Development](#development)
- [License](#license)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

---
## Why Forks Break Your Heart

Long-lived forks are technical debt disguised as Git branches. Here is the developer pain that skot eliminates:

- **[Where PRs go to die](#rescuing-abandoned-or-rejected-prs-where-prs-go-to-die):** You find the exact pull request that solves your problem, but it has been sitting unreviewed for eight months, or got closed because upstream maintainers declared it "out of scope." With skot, you do not need maintainer approval to use it in your build.
- **[Can't wait weeks for an upstream release?](#need-that-bugfix-in-production-yesterday)** You found a nasty bug, submitted a fix, and upstream merged it. But their next release cycle is six weeks away, and your production deployment is breaking right now. skot lets you cherry-pick that fix into your build today and cleanly drop it when the official release lands.
- **[Hunt and gather features like Pokémon](#gotta-catch-em-all-stacking-multiple-community-prs):** You stumble across two or three brilliant, independent pull requests from different contributors. Managing a custom fork that tries to merge two other forks is an octopus-merge nightmare. With skot, you gather community PRs directly into your series and let them stack cleanly.
- **[A favorite feature was pruned or deprecated?](#bringing-back-a-removed-feature-or-legacy-workflow)** Maintainers make difficult, necessary decisions to keep their codebases lean, deprecating older workflows or niche configuration flags. If your workflow still relies on that capability, you do not need to stay stuck on an ancient release. skot can import the removal commit in reverse (`-r`), cleanly restoring the feature in your custom build while keeping everything else up to date.
- **[Escape merge conflict staleness](#cumulative-verification-check):** In a monolithic fork, resolving a conflict often means untangling dozens of unrelated commits. With skot, patches apply sequentially, and [cumulative checks](#cumulative-verification-check) pinpoint the exact line, file, and context that drifted.

---

## ⚡ Base Features at a Glance

- **Extract patches from anywhere:** Pull patches from any Git source: SSH URLs, local filesystem folders, or remote HTTP repositories. Extract from single commit SHAs, commit history ranges (`A..B`, `A...B`), or directly from Pull Requests across GitHub, GitLab, Codeberg, Bitbucket, and private on-premise forges.
- **Zero history requirement with `--repo`:** Point skot at *any* repository on the internet or local disk. It does not matter whether that repository shares a common Git history with your project. If you want a handy `tsconfig.json`, a CI workflow, or a utility script from another project, skot packages it into a patch and adds it to your series. Compatibility is verified when you test it with [`skot check`](#cumulative-verification-check).
- **Zero `git` binary dependency:** Built directly on [libgit2](https://libgit2.org) via the `git2` Rust crate. Works in lightweight containers, minimal CI images, or environments where the Git executable is not even installed.
- **Reversible patch imports (`-r`):** Invert any commit or range on the fly to undo upstream regressions or restore pruned functionality.
- **[Isolated authoring worktrees](#the-authoring-loop):** Author and tweak patches inside dedicated, sandboxed work areas under `.skot/work/`. Your sandbox stays clean, and you can edit multiple patches in parallel without polluting your active build tree.
- **[Cumulative verification (`skot check`)](#cumulative-verification-check):** skot replays your entire series sequentially against any target ref to ensure patch *n* works on top of patches 1 through *n-1*, catching conflicts long before you run a build.
- **[Multi-series management](#multiple-series):** Maintain distinct series (such as `production`, `edge`, or `modded`) against different upstream bases with instant switching and safety checks.
- **Flexible custom hooks:** Works like the `scripts` section in npm's `package.json`. Define lifecycle hooks (`pre-build`, `build`, `post-build`) or custom named hooks (`[[hook]]`) to automate repetitive workflows with `skot run <name>`.

---

## Quick Start

### Install

```bash
cargo install --path .
```

Or grab prebuilt binaries from [GitHub Releases](https://github.com/OmentaElvis/skot/releases).

### 60-Second Walkthrough

```bash
# 1. Initialize skot against any upstream repository
skot init --url https://github.com/example/project --default-ref v1.0.0

# 2. Extract a pull request or author a new patch
skot patch import --pr 42
# ...or create your own:
skot patch new my-fix
$EDITOR .skot/work/default/my-fix/src/main.rs
skot patch save

# 3. Build: runs the entire pipeline end-to-end
# skot build automatically runs the full chain: syncs mirror -> checks series -> applies patches -> runs build hooks.
# You can go straight from init to build! (Run `skot check` whenever you want a dry check without building.)
skot build
```

### What a skot tree looks like

After running `skot init` and authoring or importing a patch, your project directory is structured cleanly into version-controlled source files and disposable working state:

```text
my-project/
├── skot.toml               # Main configuration (upstream URL, default ref, build hooks)
├── patches/                # Version-controlled patch store (commit this to Git)
│   └── default/            # One directory per series (the default series is 'default')
│       ├── series          # Ordered list of patch files ('#!' marks disabled patches)
│       ├── base            # The upstream ref and commit this series sits on
│       ├── 001-my-fix.patch
│       └── 002-feature.patch
└── .skot/                  # Disposable local workspace (add to .gitignore, safe to delete)
    ├── mirror.git/         # Bare git mirror of upstream (only thing that touches network)
    ├── sandbox/            # Linked worktree workspace where patches are applied and built
    │   └── default/
    ├── work/               # Isolated authoring worktrees for 'skot patch edit'
    │   └── default/
    └── undo/               # Automatic safety checkpoints created during rebases
```

- **`skot.toml`**: The main configuration file that records your upstream repository URL, default base ref, and lifecycle build hooks.
- **`patches/`**: The permanent source of truth. Each series gets its own directory with an explicit `series` ordering file and plain unified `.patch` files. Commit this entire directory to Git.
- **`.skot/`**: Disposable local cache and workspaces. It contains the upstream mirror, your linked build sandbox, and authoring worktrees. If you ever delete `.skot/`, running `skot build` or `skot sync` will simply recreate it on the fly.

---

## 📦 Recipes: Real-World Survival Guides

### Rescuing abandoned or rejected PRs ("Where PRs go to die")

An open-source maintainer hasn't had bandwidth to touch their repository, or closed an essential PR because it didn't align with their current roadmap. You can pull that PR directly into your build without needing upstream maintainer intervention:

```bash
# Pull PR #142 straight from GitHub into your patch series
skot patch import --pr 142

# Or pull a PR from GitLab or an external fork
skot patch import --pr 88 --repo https://gitlab.com/contributor/project
```

> [!TIP]
> **Zero shared history needed with `--repo`:**
> The `--pr` flag tells skot what change to extract. When `--repo` is omitted, skot operates against your project's primary upstream mirror. When you pass `--repo`, skot fetches from that target repository regardless of whether it shares any commit history with your current project. You can borrow configs, workflows, or helpers from anywhere across the open-source ecosystem.

### Need that bugfix in production yesterday?

You submitted a fix to upstream, but the official release cycle is weeks away. Import your branch or commit range today and ship immediately:

```bash
# Import a range of commits from your local branch or remote branch
skot patch import main..my-bugfix

# Verify and build
skot check
skot build
```

When upstream finally cuts their next release that includes your fix, run `skot patch drop my-bugfix` and you are back to pristine vanilla upstream.

### Gotta catch 'em all: Stacking multiple community PRs

Want community feature A (PR #12), performance fix B (PR #45), and a custom tweak in the same custom binary?

```bash
skot patch import --pr 12
skot patch import --pr 45
skot patch new my-custom-tweak
# edit in .skot/work/default/my-custom-tweak and save:
skot patch save

# Cumulative check ensures all three patches play nicely together
skot check
skot build
```

Because each patch is an independent file in your `patches/` series, you can toggle any patch on or off anytime with [`skot patch disable`](#patch-commands) and [`skot patch enable`](#patch-commands) without breaking your Git history.

### Bringing back a removed feature or legacy workflow

Maintainers periodically prune older APIs, obsolete configuration flags, or niche features to keep their project nimble and maintainable. If your team still depends on that capability, you do not need to choose between running an ancient release or abandoning your workflow. skot makes it simple to restore functionality by reversing the commit that removed it:

```bash
# -r reverses the diff, restoring the code that was deleted in commit a1b2c3d
skot patch import a1b2c3d -r
skot check
skot build
```

### Bonus Recipe: No real upstream Git repository? Working with ZIP files and custom hooks

Sometimes the software you need to patch has no public Git repository at all. Perhaps you run commercial deployments distributed as raw ZIP/tarball downloads, or you are modding a game or tool where releases are delivered as archive files.

When a new version arrives, applying your manual tweaks to fresh ZIP files usually means losing your changes or spending hours doing manual diff comparisons. skot turns any folder into a first-class patch target.

#### The raw preparation flow

Here is what the raw preparation looks like without automation:

```bash
# 1. Unpack the original release into a local folder
unzip upstream-v1.0.zip -d ./upstream-src
cd ./upstream-src
git init && git add . && git commit -m "upstream v1.0.0"
cd ..

# 2. Initialize skot pointing to the unpacked folder
skot init --url ./upstream-src --default-ref master

# 3. Add your custom fixes, performance tweaks, or mods
skot patch new optimize-caching
# edit files in .skot/work/default/optimize-caching...
skot patch save

# 4. Build directly!
# Because `skot build` executes the full chain (sync -> check -> apply -> build hooks),
# you can go straight from init to a completed build:
skot build
```

#### Taming the prep with custom hooks

Nobody wants to manually run unzip, `git init`, and `git commit` every single time an upstream archive drops. You can cleanly extract that prep into a custom `skot` hook in `skot.toml`, working just like the `scripts` section in `package.json`:

```toml
# skot.toml
[upstream]
url = "./upstream-src"
default-ref = "master"

# Custom hook: works just like the "scripts" section in package.json!
[[hook]]
name = "unpack-upstream"
cmd = "unzip -qo upstream-v*.zip -d ./upstream-src && cd ./upstream-src && git init && git add . && git commit -m 'upstream release'"
```

With that hook in place, handling a new upstream release drops down to two clean commands:

```bash
# 1. Drop the new zip in the folder and let your hook handle prep:
skot run unpack-upstream

# 2. Build the new release with your patch series replayed automatically!
# Because `skot build` runs the full chain (sync -> check -> apply -> build hooks),
# you go straight from prep to a finished build:
skot build
```

If you ever want to dry-run or verify compatibility before compiling, `skot check` is always available to cumulative-test your series without triggering a build. No manual diffing, no lost tweaks.

---

## The Authoring Loop

```bash
skot patch new "add a greeting"   # add-a-greeting.patch created, work area opened
$EDITOR .skot/work/default/add-a-greeting/f.txt   # edit in the work area
skot patch save                   # your edits become the patch contents
skot check                        # verify the whole series still applies
skot patch apply                  # stamp the saved patch into the sandbox
skot build                        # sync -> apply -> build
```

Every open patch gets its own worktree under `.skot/work/<series>/`, holding the patches below it plus its own content. You are always editing the change in the context it will apply to. The sandbox is left to applying and building; authoring edits never disturb the applied set. Two patches can be open at once because their trees are separate.

There is no separate close step: a work area exists while it holds changes the patch file does not. `save` retires it on success, and `clean` reaps the ones holding nothing extra as derived state.

`save` measures your worktree against the base the work area recorded, not against whatever the sandbox happens to hold. Any patch can be saved in any order. If patches below changed after the work area opened, `save` refuses rather than exporting against a tree the series cannot rebuild; `--force` exports against the recorded base anyway.

`save` also refuses two things: an export with no diff when the patch is not empty (the worktree no longer holds what the patch describes), and a patch that libgit2 cannot apply back to its base. Both would replace real work with something broken.

Unsaved changes are the only thing protected: `patch edit` will not open over them (`save` first, or `--discard`), an open work area rides along with `patch drop` (the confirmation says so), and `clean` refuses to remove the mirror while any hold unsaved changes. `clean --sessions` names them all.

`--json` on any command gives machine-readable output on stdout; human-readable progress goes to stderr and is suppressed. Hook output goes to stderr too under `--json`, so stdout stays exactly one parseable document.

---

## Pull requests from any forge

`skot patch import --pr N` reads the number against the forge the request is fetched from. GitHub, GitLab, Bitbucket, Codeberg, and gitea.com are recognized out of the box: `--pr 7` on GitLab fetches `refs/merge-requests/7/head`, and on GitHub `refs/pull/7/head`. A self-hosted or private forge names its namespace in `skot.toml`:

```toml
[pr-refs]
"gitlab.corp.example" = "refs/merge-requests/{n}/head"
"*.corp.example" = "refs/merge-requests/{n}/head"
```

Keys are hosts; a `*.` prefix matches subdomains, never the domain itself. An entry overrides what skot knows. For a request that will not fit a number, `--pr` takes the full ref as written: `--pr refs/changes/12/345/1`.

Which forge is consulted is the repository being asked: `--repo` when you pass one, the mirror's origin otherwise. This makes the cross-forge case an ordinary command:

```bash
skot patch import --pr 7 --repo https://gitlab.com/team/project
```

The request is fetched from the GitLab repository under GitLab's namespace, and the base still comes from the project's own origin. Remember that skot does not require any shared Git history when `--repo` is specified: you can extract a change from an entirely different repository and test whether it applies to your project with `skot check`.

---

## CI, Automation, and Autonomous Agents

skot is built from the ground up to excel in headless CI environments, automated scripts, and agentic workflows (such as AI coding assistants and autonomous agents).

### Automatic non-interactive detection

When run in environments without a controlling terminal (TTY), such as GitHub Actions, GitLab CI, headless Docker containers, or background agent subprocesses, skot automatically adapts:

- **No hanging prompts:** Interactive pickers and confirmation prompts are disabled so scripts and autonomous agents never get stuck waiting on input.
- **Clean log streams:** Interactive animated spinners are suppressed so they do not flood logs with ANSI escape codes or carriage-return (`\r`) overwrites.
- **Explicit flags:** Use `-y` / `--yes` (or `SKOT_YES=1`) on mutating operations to automatically confirm actions. Use `--no-hints` if you prefer to silence next-step advice in minimal pipelines.

### Autonomous next-step guidance ("next" hints)

To make automated and agentic workflows seamless, skot outputs structured `next` guidance on stderr after every command. This tells the developer or autonomous agent what logical steps are available, eliminating guesswork:

```text
$ skot init --url https://github.com/akheron/jansson
> First series name
> Default ref master
> Sandbox policy persistent: kept between runs; a ref change resets it (right locally)
> Configure hooks? No
  init wrote /tmp/jansson/skot.toml
  init wrote /tmp/jansson/patches/default/series
  init created /tmp/jansson/patches/default
> Mirror the upstream repository now? Yes
  sync mirroring https://github.com/akheron/jansson
   git Enumerating objects: 485, done.
   git Counting objects: 100% (485/485), done.
   git Compressing objects: 100% (253/253), done.
  sync fetched 1 branch refs and 0 tag refs

next
  edit the [hooks] section to describe how you build and where the result
  goes; post-build is where you copy it into place.

  skot build                 the whole run: fetch, apply, build
  skot patch new <name>      start a patch and open a work area on it.
```

Autonomous agents can inspect these suggestions to decide their next tool call without having to read the full CLI manual on every turn.

### Machine-readable, stable JSON output (`--json`)

For scripts, automation pipelines, and autonomous agents, every `skot` command supports `--json`. Passing `--json` guarantees a single, predictable, parseable JSON document emitted to `stdout`, while all human diagnostics, logs, and hook outputs are redirected to `stderr` or silenced. This ensures tools like `jq` or Python scripts never encounter corrupt or interleaved data:

```bash
skot init --url https://github.com/akheron/jansson --json --default-ref=master
```

Output on `stdout`:

```json
{
  "config": "/tmp/jansson/skot.toml",
  "patches": "/tmp/jansson/patches/default",
  "root": "/tmp/jansson",
  "series": "/tmp/jansson/patches/default/series",
  "sync": {
    "branches": [
      "master"
    ],
    "created": true,
    "fetched": true,
    "head": "851a2145e3256f2e67e5dfe24b0e456bf198b741",
    "mirror": "/tmp/jansson/.skot/mirror.git",
    "tags": [],
    "url": "https://github.com/akheron/jansson"
  },
  "todo": []
}
```

Because `stdout` is reserved strictly for this JSON document, piping commands into JSON processors works safely and reliably in any script:

```bash
# Safely extract properties in shell scripts or agent tools:
ROOT_DIR=$(skot status --json | jq -r .root)
HEAD_SHA=$(skot init --url https://github.com/akheron/jansson --json --default-ref=master | jq -r .sync.head)
```

### Reliable CI matrix testing

In continuous integration, verifying that all your patch series remain compatible with upstream base branches is a single offline command:

```bash
# Verify every series against its configured base commit:
skot check --series all
```

Standard Unix exit codes (0 for success, non-zero for conflict or build failure) ensure that CI pipelines fail fast when upstream moves or patches collide.

---
## Rebasing Across Upstream Releases

Upstream projects never stand still. When upstream cuts a new release, commits land, functions move, and line numbers change. `skot rebase <ref>` is designed to replay your patch series onto that new upstream version and cleanly rewrite the patch files so they remain valid.

### Why patches become invalid (History churn and anchor drift)

A patch file is not just a blob of added or deleted lines. It is a unified diff anchored by three things:

1. The target file path.
2. The hunk header line numbers (for example, `@@ -42,6 +42,7 @@`).
3. The surrounding context lines (the 3 lines of unchanged code above and below your modification).

When Git or libgit2 applies a patch, it looks for the exact line coordinates first. If those shifted, it searches nearby for the matching context lines. But after weeks of upstream development, surrounding code gets refactored, adjacent lines get edited, or variable names change. When the context lines no longer match, the patch fails to anchor and is rejected as invalid.

#### Example: A patch before rebase (anchored to v1.0.0)

```patch
From: Alice <alice@example.com>
Date: Mon, 12 Oct 2026 10:00:00 +0000
Subject: [PATCH] Add custom timeout config

--- a/src/config.rs
+++ b/src/config.rs
@@ -42,6 +42,7 @@ pub struct Config {
     pub host: String,
     pub port: u16,
+    pub timeout_seconds: u64,
     pub retries: u32,
 }
```

In upstream `v2.0.0`, developers refactored `Config`, adding a `keepalive` setting, moving fields around, and bumping the struct definition down to line 75:

```rust
// Upstream v2.0.0 src/config.rs (around line 75):
pub struct Config {
    pub host: String,
    pub port: u16,
    pub keepalive: bool,
    pub retries: u32,
}
```

Because the surrounding context lines drifted and line 42 no longer matches `pub struct Config`, applying the old patch to `v2.0.0` will fail.

### How `skot rebase` realigns the patch coordinates

Running `skot rebase v2.0.0` replays your changes sequentially against the new base. Instead of doing a messy three-way merge that can corrupt formatting, skot recalculates the exact two-tree diff on the new base and rewrites the patch file with updated line numbers and refreshed context lines:

#### The patch after running `skot rebase v2.0.0`:

```patch
From: Alice <alice@example.com>
Date: Mon, 12 Oct 2026 10:00:00 +0000
Subject: [PATCH] Add custom timeout config

--- a/src/config.rs
+++ b/src/config.rs
@@ -75,6 +75,7 @@ pub struct Config {
     pub host: String,
     pub port: u16,
+    pub timeout_seconds: u64,
     pub keepalive: bool,
     pub retries: u32,
 }
```

Notice what changed:

- The hunk header automatically updated from line 42 to line 75 (`@@ -75,6 +75,7 @@`).
- The context lines now accurately capture upstream's new `pub keepalive: bool` field.
- The original author, commit date, and commit message are completely preserved.

### Branching to a new series with `--into`

What if you still need your patch series to support an older release like `v1.2.3`, but also want a modern series compatible with `v2.0.0`?

Instead of overwriting your current series, use the `--into` flag:

```bash
skot rebase v2.0.0 --into v2-support
```

This branches your work:

- Your active series remains anchored to `v1.2.3` without touching a single file.
- The freshly rewritten patches are installed into `patches/v2-support/` anchored to `v2.0.0`.
- You can switch between them anytime using `skot series switch v2-support` or test both in CI with `skot check --series all`.

### Atomic safety and undo

Every rebase is assembled and verified in `.skot/tmp/` before touching your live series. If a patch fails to apply to the new ref, the operation aborts immediately and leaves your existing series completely untouched.

When rebasing in place, skot automatically saves the replaced series to `.skot/undo/<series>`. If you ever need to roll back, simply run:

```bash
skot series undo
```

This instantly trades the active series back to its pre-rebase state.

---

## The Series Format

The format is one entry per line. A line naming a patch is that patch enabled, a line starting `#!` is that patch disabled, `#` starts a comment, and a blank line is ignored. Unrecognised lines are preserved byte for byte, so adding one entry produces a one-line diff.

A line says what it is: parsing reads the line alone and never the directory, so what a line means cannot change underfoot. A trailing `#` is part of the filename, and a comment is never mistaken for a patch however much it mentions one.

The order this file lists is the order `apply` follows, and it is the only place that order lives. A patch's name is the patch's own: chosen when the patch is created, never rewritten because something moved, and carrying no meaning beyond naming a file. Reordering is a change to this file alone, and skot never renames a patch.

What the sandbox holds is an ordered *subset* of the series: any patch can be on or off, and the set always replays in series order. `skot patch list` and `skot status` show it as `[x]` against each applied entry and `[ ]` against each that is off, so a disabled patch in the middle is visible rather than implied by a count. The choice of which patches apply lives in the series file itself: `skot patch disable` marks a line `#!` and `skot patch enable` takes the mark off, so it is trackable in git. Every `[PATCH]` argument reads the same way: an exact name, a unique name prefix such as `fix`, or a 1-based position among the enabled patches (`skot patch drop fix`, `skot patch push 3`).

`skot patch new --at` and `skot patch import --at` insert at an existing patch name or a 1-based position; only the new line is added and every other patch keeps its name and its place. A file that already occupies a name outside the series is refused rather than overwritten. `skot patch drop <patch>` goes the other way: it takes the line and the file, leaving the rest where they are. `skot patch move` reorders without taking anything out: the patch's line steps to its new place and the entries it passes fold one place at a time.

---

## Multiple Series

A project can hold several series at once, one directory each under `patches/`. Each has its own `series` file, its own patches, and its own `base`: the ref it sits on and the commit that ref pointed at when it was recorded, written by `rebase` and carried over by `series create`. One series is active, named in this order: `--series <name>` (or `SKOT_SERIES=<name>`) for a single run, the pointer in `.skot/state.json` that `series switch` moves, then `series = "<name>"` in skot.toml, which `skot init` writes so a fresh checkout builds the same series you do. With nothing naming it the directories decide: the one series there is wins, none means `default`, and several refuse with the list rather than guess which line of work you were on. The same words `all` and `current` are reserved everywhere a series name is accepted, so they always mean "every series" and "the active one" and can never be a directory.

Series are named and moved as whole units. `series create --from <series>` and `series copy --from <series>` seed a new series from another one's patches, list and base rather than the active one; `series rename <name>` moves the active series, taking the pointer, open work areas and their worktree registrations with it; `series drop <name>` removes a series and its patches for good, refusing the active one. `patch copy <patch> --into <series>` takes a single patch file to another series, and `--from <series>` brings one across. A name that differs from an existing series only in case is refused: on a case-insensitive file system the two would be one directory, and the same command would mean different things depending on where the project lives.

The sandbox follows the series. By default each series has its own worktree under `.skot/sandbox/<name>`, so switching becomes a pointer move and the cost is disk, one dependency tree per series. Setting `sandbox.per-series = false` shares one slot at `.skot/sandbox/@shared` instead: a single tree for the project, and `sandbox.on-switch` decides what a switch does to it, `rebuild` (the default) applying the series you switched to as part of the switch, `keep` leaving the tree alone until the next `build` or `apply`. The slot's name begins with `@`, which a series name can never contain, so the two scopes sit beside each other under `.skot/sandbox/` and can never claim the same path.

Work areas are per series too, under `.skot/work/<series>/`, so two series can have the same patch name open at once without colliding. `series copy <name>` copies the active series under a new name without switching to it, which is the convenient way to preserve a series as it stands before an experiment.

Every write to a series goes the same way: the replacement is built in full under `.skot/tmp/`, read back and checked against what was meant to go in it, and only then renamed into `patches/`. The series directory is a unit, so it lands whole or not at all, and a disk that fills up mid-write fails in scratch with the live series untouched. On a successful in-place `rebase` the directory that was swapped out becomes the undo entry: one version per series, at `.skot/undo/<name>`, and `series undo` trades the two (so a second undo reverses the first). Nothing lists the undo entry; it is there for the rebase you did not mean to finish. A failed rebase writes nothing, so it keeps nothing.

---

## Cumulative Verification (`check`)

`skot check` replays the whole series in order against a ref, so patch *n* is verified against the tree that patches 1 through *n-1* produced. `skot patch apply` does the same replay and then checks the result out, so the two can never disagree about whether your series applies.

> [!IMPORTANT]
> **Why cumulative testing matters:** If each patch were verified against the base commit in isolation, a series where two patches edit the same line in sequence would pass the check and then fail the build. Cumulative replay guarantees real-world correctness by verifying each patch in the exact context created by the patches preceding it.

When a patch does not apply, skot reports which file, which line, and what text it was looking for (usually the context lines of the hunk), pointing directly to the drifted area.

By default `check` stops at the first failure and names the patch. `--all` reports every failure, which is ideal after a rebase.

The default is the whole series on purpose: verifying the entire series is the strongest statement for rebases or CI runs. `--applied` narrows the replay to the recorded set, in series order, holes included, matching exactly what `build` lands. It answers whether a modified or popped set still holds against another ref (`skot check --applied v2.1.0`).

`apply` never leaves a half-applied series behind. If replay fails, the sandbox resets to the upstream ref first, so subsequent runs cannot report phantom conflicts.

---

## The Mirror and the Sandbox

`skot sync` creates a bare mirror at `.skot/mirror.git` and fetches branches and tags from `upstream.url`. It is the only operation in skot that touches the network, which is why `check` and `apply` work completely offline. `--offline` is enforced before anything connects: the local mirror continues answering, fetches from local folders succeed, and any URL requiring network access is cleanly refused. `--refresh` re-contacts the upstream even when the mirror is warm (`skot build --refresh`).

`skot patch apply` creates `.skot/sandbox/<series>` as a **linked worktree of that mirror** (or `.skot/sandbox/@shared` when `sandbox.per-series` is off). A linked worktree shares the mirror's object store, so the sandbox costs only a working tree plus a few kilobytes of metadata rather than a second copy of history. Resetting it is a fast checkout inside the mirror. Work areas under `.skot/work/<series>/` are also linked worktrees of the same mirror, one per patch: identical speed, shared object store, and `clean` automatically prunes their registrations before removing the mirror they depend on.

> [!WARNING]
> **The sandbox is a workspace, not a security boundary:**
> The `.skot/sandbox/` folder is purely a workspace where skot places disposable worktrees and builds artifacts in isolation from your git repository root, keeping your source tree clean. It is **not** a literal security sandbox (like a Linux namespace, container, or seccomp jail). Any commands or hooks configured in `skot.toml` execute with your normal local user permissions.

Passing a ref to `sync` does not narrow the fetch. It force-updates that ref, because a branch force-pushed upstream is often the exact branch you need to patch against.

Because a linked worktree depends on the mirror, skot never garbage-collects or prunes the mirror while a sandbox exists.

---

## Command Reference

skot organizes operations into three command namespaces:

- **[Project commands](#project-commands):** run directly (`skot build`, `skot check`, `skot sync`, `skot status`, `skot clean`, `skot hooks`, `skot run`)
- **[Patch commands](#patch-commands):** under `skot patch` (`new`, `edit`, `save`, `import`, `enable`, `disable`, `apply`, `unapply`, `push`, `pop`, `list`, `show`, `move`, `drop`, `copy`)
- **[Series commands](#series-commands):** under `skot series` (`list`, `create`, `switch`, `copy`, `undo`, `rename`, `drop`)

Global flags available on any command: `--series`, `--refresh`, `--offline`, `--json`, `--no-hints`, `--yes`, `--verbose`, `--dir`.

### Project commands

| Command | Description |
|---------|-------------|
| `init` | Scaffold `skot.toml` and `patches/<series>/`. Prompts for missing values including the first series name (default `default`). |
| `build [ref]` | The one-command run: sync, apply, then run your hooks (`pre-build`, `build`, `post-build`). |
| `status` | Show paths, disk usage, current ref, and whether a run was interrupted. |
| `check [ref] [--applied]` | Verify the series applies to a ref. `--applied` checks only the enabled set (what `build` lands). `--series all` checks every series against its own base. No build, no network. |
| `rebase <ref> [--into <name>]` | Replay the series onto a new base, rewriting each patch. On an empty series it just moves the base. The replacement is built and verified in `.skot/tmp` before replacing the live series; the old version becomes the undo entry. `--into` creates a new series instead of replacing the active one. |
| `sync [ref]` | Mirror the upstream repository and keep its refs current. Passing a ref force-updates it. |
| `clean [level] [--sessions]` | Free disk space: `sandbox`, `out`, `mirror`, `all`. Dry run unless `--yes`. Work areas with unsaved changes are never removed by default; `--sessions` drops them. `mirror`/`all` refuse while any work area holds unsaved changes. |
| `hooks` | List configured hooks in lifecycle order with their TOML keys. |
| `run <name>` | Run a `[[hook]]` entry by name (defined in `skot.toml`). |

### Patch commands

| Command | Description |
|---------|-------------|
| `patch new <name> [--base <ref>] [--at <patch/N>]` | Create an empty patch at `--at` (end by default), register it, and open a work area on it. Only the new line is added; every other patch keeps its name and place. |
| `patch edit <patch> [--base <ref>] [--discard]` | Open a work area on an existing patch: a worktree under `.skot/work/<series>/` holding the patches below plus the patch's own content. Refuses if the work area has unsaved changes; use `--discard` to throw them away. |
| `patch save [patch] [--force]` | Save the work area's changes into the patch file, keeping its author and date, and retire the work area. Refuses when patches below changed since opening; `--force` exports against the recorded base anyway. |
| `patch import <range> [--pr <n>] [--repo <url>] [-r]` | Turn a commit range, a `--repo` fork/clone, or a `--pr` pull request into a new patch at the end (or `--at`). `-r` reverses the diff (undoing what the range did). |
| `patch enable [patch...] [--all]` | Put patches back in the applying set by removing the `#!` marker. Several patches per run; `--all` takes every disabled one; otherwise an interactive picker. |
| `patch disable [patch...] [--all]` | Skip patches from now on: mark their series lines with `#!`. Several per run; `--all` takes every enabled one the sandbox is not holding; otherwise an interactive picker. Refuses while any are applied; `pop` first. |
| `patch apply [ref]` | Reset the sandbox to a ref and replay the enabled set into it. No prompts; choose the set with `enable`/`disable`. |
| `patch unapply` | Reset the sandbox to upstream, discarding the series. Patch files are untouched. |
| `patch push [name] / pop [name]` | Move through the applied set. `push` applies the next enabled patch (or a named one with everything below); `pop` removes the top applied patch (or a named one with everything above). |
| `patch list` (`ls`) | List the series in order: enabled entries with `[x]`/`[ ]`, disabled lines dimmed in place, work areas with unsaved changes marked `(unsaved)`. `--paths` shows file locations. |
| `patch show [patch...] [--stat]` | Print a patch with a report head (position, state, work area, per-file counts) above the patch. Mail headers, diffstat, and diff are coloured by role. Piped output is the raw patch bytes for round-tripping. `--json` includes text, counts, and state. `--stat` stops at the head. |
| `patch move [patch] --to <patch/N>` (`mv`) | Reorder: the patch becomes position N or lands before a named patch. No file is renamed. Interactive if arguments omitted. Refused while the patch is applied. |
| `patch drop <patch>` (`rm`) | Remove a patch from the series: its line and its file. A work area on it retires with it. Asks for confirmation; `--yes` skips. Refused while applied. |
| `patch copy <patch> (--into <series> / --from <series>) [--at <N>]` | Copy a patch between series. `--into` takes from active to another; `--from` brings into active. Name is kept; lands at end or `--at`. |

### Series commands

| Command | Description |
|---------|-------------|
| `series` | List all series: active one marked, each with patch count. |
| `series create <name> [--from <series>]` | Create a series directory (empty series, base copied from the series you are leaving) and switch to it. `--from` seeds from another series instead. |
| `series switch <name>` | Switch to an existing series. With `sandbox.per-series = false`, the shared slot follows `sandbox.on-switch`: `rebuild` applies the new series immediately, `keep` waits for the next `build`/`apply`. |
| `series copy <name> [--from <series>]` | Duplicate the active series under a new name with its patches and base. Does not switch. The copy is verified in `.skot/tmp` before landing. |
| `series undo [--yes]` | Trade the active series with the version the last successful rebase replaced (at `.skot/undo/<name>`). Undoing again reverses the trade. One version kept per series. |
| `series rename <name>` | Rename the active series. Pointer, open work areas, and worktree registrations follow. Refuses if the series holds unsaved work. |
| `series drop <name> [--yes]` | Remove a series permanently: its patches, list, and base. Refuses the active series and any holding unsaved work. |

---

## MISC
### How a ref gets chosen

Commands that take an optional ref resolve it in this order (first hit wins):

1. The ref you named on the command line.
2. `upstream.default-ref` in `skot.toml`.
3. The ref the sandbox is currently on, if it is on one at all.

This makes `skot check` work bare early on (before a sandbox exists), and makes it agree with `skot patch apply` when you pass a ref.

---

### Terminal Output and Logging

skot uses two distinct output streams:

- **stdout:** carries the machine-readable result. Under `--json` that is a single JSON document and nothing else, making `skot check --json | jq` safe to pipe. Human output never pollutes stdout.
- **stderr:** carries all human progress, tables, diagnostics, and hook outputs.

Stderr output is driven by the [`log`](https://docs.rs/log) crate, so `RUST_LOG` gives full control over verbosity:

| `RUST_LOG` | What skot prints |
| --- | --- |
| *(unset)* | Results: what was applied, written, synced, and requested tables |
| `RUST_LOG=info` | Same as default |
| `RUST_LOG=debug` | Detailed diagnostics: resolved SHAs, mirror paths, skipped hooks |
| `RUST_LOG=warn` | Only warnings and errors (ideal for clean CI logs) |
| `RUST_LOG=skot::commands::rebase=debug` | Scoped to a specific command |

Errors appear in red, warnings in yellow, and debug lines are dimmed. Under `--json`, logs are turned off completely to keep stdout strictly parseable.

Fetch progress is rendered interactively with [`indicatif`](https://docs.rs/indicatif) and suspended around log writes via [`indicatif-log-bridge`](https://docs.rs/indicatif-log-bridge) so spinners never overwrite output.

---

### Development

```bash
cargo test
cargo fmt
cargo clippy --all-targets -- -D warnings
```

---

## License

MIT. See [LICENSE](LICENSE).
