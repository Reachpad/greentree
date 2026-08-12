---
name: greentree
description: Land code changes through greentree instead of raw git. Use in any repo that has a greentree.yaml or the greentree binary on PATH, whenever about to commit, push, run tests, or land a change, or when asked "is it green" / "did it pass". greentree verifies the working tree, caches the result by content, and only lets a verified tree become a commit.
---

# greentree: verify the tree before it becomes a commit

greentree runs this repo's checks against the current working tree, caches
the verdict by content (tree hash), and creates the commit from the exact
tree that passed. Use it instead of running tests by hand and then
`git commit`/`git push`.

Applies when the repo has a `greentree.yaml`, or `greentree` is on PATH and
`greentree status` succeeds.

## Landing a change: one command

```sh
greentree gate --json -m "<commit message>"
```

`gate` runs the required checks (instant when this exact content already
passed), then commits the verified tree. It is idempotent: running it twice
in a row is a no-op. This replaces `git add`/`git commit`/`git push`.

Add `--push` to also push. Without it, `gate` commits locally only.

## Branch on the exit code, never on the text

| exit | meaning | do this |
|---|---|---|
| 0 | verified and committed | done |
| 10 | a check failed | read `.log_tail` in the JSON, fix the code, run `gate` again |
| 11 | tree not verified yet | run `greentree test --json`, then `gate` |
| 12 | repo mid-merge/rebase, or a dirty submodule | resolve it, then `gate` |
| 13 | another greentree run is in progress | wait a moment, then retry |
| 14 | config error | read `.error`; fix `greentree.yaml` |
| 15 | publish/push failed | read `.error` (e.g. remote rejected, no token) |

## While iterating

- `greentree test --json` runs the checks without committing. `.ok` is the
  answer. Per-check detail is in `.results`.
- `greentree status --json` answers "would a commit succeed right now?"
  without running anything new. Read `.publishable`.

## Rules that keep it correct

- Do not run `git commit` or `git push` directly in a greentree repo. The
  commit must be built from the tested tree; `gate` does that. Bypassing it
  produces an unverified commit.
- A cache hit is as authoritative as a fresh run. The tree hash proves the
  content is identical, so do NOT re-run "to be sure" — that is wasted work.
- Do not edit files while `gate` or `test` is running. If the tree changes
  mid-run the verdict is `cancelled` (it binds to no tree) and you will
  just rerun it. Make your edits, then run the check.
- When a check fails (exit 10), the output is in the JSON: `.log` is the
  full log file path, `.log_tail` is the last lines. Read it before
  guessing at a fix.

## Gate before GitHub

The published commit is built from the exact verified tree, so it needs no
re-check. If the repo enforces a `greentree/<check>` required status, a
plain `git push` can be made mergeable by attesting the pushed commit:

```sh
git push
greentree attest --json    # posts greentree/<check> on HEAD if verified
```

`gate --push` does the commit, push, and status in one step.
