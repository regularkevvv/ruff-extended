---
name: aligning-with-upstream-versions
description: Use when a user says "sync with upstream", "align with the new upstream release", "merge the latest upstream version", or asks to update this fork (and its superproject) to a new upstream ty release.
---

# Aligning With Upstream Versions

Use this skill to bring the fork up to date with a new upstream ty release. Two repositories move
together: this repository (a fork of the upstream Ruff repository, consumed as a `ruff` git
submodule) and its superproject (a fork of the upstream ty repository, which owns versioning,
packaging, and releases).

Version mapping: upstream ty `0.0.N` maps to fork release `0.N.0`. Later fork releases on the same
upstream base increment the patch. The plugin protocol and SDK crates are versioned independently
and only bump when their public surface changes.

## Find the Upstream Pin

1. In the superproject, fetch upstream tags and resolve the new tag to a commit.
1. Read the Ruff commit that tag records for its submodule: `git ls-tree <tag-commit> ruff`.
1. That exact commit — not the tip of the upstream default branch — is the merge target for this
    repository. The merge base should be the previous release's pin; if it is not, stop and
    investigate before merging.

## Merge This Repository First

Create a `sync/ty-0.0.N` branch from the default branch and merge the pinned upstream commit.
Resolve conflicts by these principles, in order of preference:

- **Union, not either side.** Workspace dependency lists, feature tables, and re-export lists
    conflict because the fork and upstream both append. Keep both sides' entries.
- **Both-sides-inserted code keeps both blocks.** When the fork and upstream added independent
    items at the same anchor, git may splice the two insertions and share closing braces. Rebuild
    both blocks completely and verify brace balance by compiling, not by eye.
- **`Cargo.lock`: take upstream's side, then regenerate** (`cargo metadata`) so the fork-only
    crates are re-added. Verify the fork crates are present in the resolved lock afterwards.
- **`fuzz/` is a separate cargo workspace.** Its lockfile does not conflict but goes stale when
    upstream bumps crate versions. Regenerate it (with the same `RUSTFLAGS` the fuzz workspace
    expects, e.g. `--cfg fuzzing`) and confirm the delta is pure version bumps.

Upstream API changes routinely break fork-only code even when no file conflicts:

- **Renames**: confirm the rename is behavior-preserving by diffing the upstream definition
    between the old and new pins, then rename every fork call site.
- **New enum variants** hitting non-exhaustive matches in fork code: add arms that mirror how
    upstream's own code handles the new variant, rather than inventing new behavior.

## Verify Before Touching the Superproject

Run, at minimum: `cargo check --all-features`, the full test suite, clippy, and a fuzz build.

- **Capture exit codes explicitly.** Write `SOMETHING_EXIT=$?` into the log immediately after the
    command; a trailing `echo` or pipeline can mask the real status, and background-task wrappers
    may report the wrapper's exit rather than the command's. Trust only the captured value.
- **Attribute failures before reacting.** A failure after a merge is not automatically a fork
    regression. Check whether the failing tests need a newer tool version than is installed locally
    (compare against what upstream CI pins, and test with that exact version in a scratch
    directory rather than upgrading globally). For build or link failures, run a control build of
    vanilla upstream at the same pin in a separate worktree: if it fails identically, the problem
    is inherited, not introduced.
- Fix genuine fork defects found along the way, but keep each fix a separate, explained commit.

Finish this repository's branch by pointing compatibility examples (plugin crate READMEs) at the
new fork version range `>=0.N.0,<0.(N+1).0`.

## Merge the Superproject

Create the matching `sync/ty-0.0.N` branch and merge the upstream tag. These files conflict on
every merge because the fork and upstream both edit them — resolve each the same way every time:

- **Version files** (`pyproject.toml`, `dist-workspace.toml`, lockfile): keep the fork's package
    identity, set the new fork version, and regenerate the lockfile (`uv lock --check` must pass).
- **`CHANGELOG.md`**: always keep the fork's side. Add a new entry that names the upstream release
    it is built on, states whether the plugin protocol and SDK versions changed, and summarizes
    behaviour changes that reach plugins. Never restore upstream's changelog text.
- **Installation docs**: keep the fork's installer URLs at the new tag; drop upstream sections
    that install the upstream distribution rather than the fork.
- **Version maps** in the README and release docs: append the new upstream-to-fork mapping line.
- **Submodule pointer**: point at the merged fork commit (initially the sync branch; re-point at
    the merged default-branch commit before the superproject merge lands).
- Update the plugin-authoring guide's compatibility range to match the new release.

Write every fork-authored line using **plugin**, never "extension": that is the term the crates
(`ty_plugin_*`), types (`Plugin`, `PluginManifest`), config (`[[plugins.plugin]]`), and manifest
filename (`ty-plugin.json`) already use. Reserve "extension" for editor extensions, LSP protocol
extensions, C extensions, and file extensions.

## Land and Release

1. Open pull requests in both repositories; let CI pass. When polling CI, require a non-empty
    status payload before treating a run as settled — a transient empty API reply is not "done".
1. Merge this repository's PR first, re-point the superproject's submodule at the resulting
    default-branch commit, then merge the superproject PR.
1. Dispatch the release workflow with the new `v0.N.0` tag **only with explicit maintainer
    approval** — publishing is not reversible.
1. Verify artifacts: the package index has the expected file count for the new version, the SDK
    crates on the registry are unchanged unless their surface changed, the release tag equals the
    default branch, the release is not a draft or prerelease, and the installer scripts resolve.

## Hygiene

After both merges land: fast-forward local default branches, delete the sync branches locally and
remotely, and confirm both working trees are clean with zero open PRs.
