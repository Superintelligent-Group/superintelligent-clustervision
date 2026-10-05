# SIG fork

Origin is Superintelligent-Group/superintelligent-clustervision; upstream is emberian/cv.
Ember is a collaborator. Keep reusable features general and suitable for upstream review.

- main mirrors upstream main. Never commit fork changes or push work to main.
- sig is the thin fork branch. Propose general changes upstream first; the apex sends
  upstream PRs. Direct upstream pushes are disabled.
- Consumers pin immutable sig-v<workspace-version>-<revision> tags rather than the
  moving branch. The initial release is sig-v0.13.0-1.
- This document belongs only on sig; omit it from upstream patches.

## Sync and rebase

Start with a clean checkout; preserve unrelated work before switching branches.

    git fetch upstream --tags
    git fetch origin
    git switch main
    git merge --ff-only upstream/main
    git switch sig
    git rebase main

Resolve conflicts with upstream behavior in mind. Drop patches upstream has accepted.
Run formatting, affected crate tests, and a release build before publishing.
For a rewritten published branch, coordinate consumers and use:

    git push --force-with-lease origin sig

Otherwise use git push origin sig. Never rewrite a published tag.

    git tag -a sig-v<version>-<next-revision> -m "SIG fork release"
    git push origin sig-v<version>-<next-revision>

## Project scope

In ~/.config/clustervision/config.toml (or $XDG_CONFIG_HOME/clustervision/config.toml):

    allow_roots = ["C:/Github/my-project", "C:/work/shared"]

cv --scope <root> overrides config; repeat the flag for multiple roots.
CV_ALLOW_ROOTS overrides config for CLI, MCP, and cvd. It is an OS path list
(semicolon on Windows, colon on Unix). An empty list restores unrestricted discovery.
CLI flags take precedence over the environment.

Scoped discovery excludes unknown cwd and projects outside those directories,
including cached and indexed results. Prefixes match directory boundaries, normalize
separators and dot components, and ignore case on Windows. Matching is lexical:
it does not resolve symlinks or require directories to exist. This controls visibility;
it does not delete existing catalogs or indexes.
