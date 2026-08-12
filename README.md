# greentree

**Test the tree, not every commit.**

greentree content-addresses your dirty working tree, caches check verdicts by
tree hash, and refuses to create a commit from any tree that hasn't passed.
Your agent can't push until this exact tree passed — and it never re-runs a
test it already paid for.

```console
$ greentree test
tree 8be03d1  test ✗ (11.2s)          # attempt 1: red

$ greentree test                       # attempt 2 after edits
tree 4f2a91c  test ✓ (10.8s)

$ greentree test                       # attempt 3 reverted attempt 2's edit
tree 4f2a91c  test ✓ (cached)          # same content ⇒ same tree ⇒ no re-run

$ greentree gate -m "implement add()"
  test ✓ (cached)
published commit a1c4e77 from verified tree 4f2a91c on main
```

The published commit is built with `git commit-tree` **from the exact tree
object the checks passed against** — not from a re-checkout, not from a
re-run. What was tested is what ships, byte for byte.

## Why

A coding agent makes 30 attempts inside a workspace. Today each attempt that
looks done becomes a commit, a push, a CI queue, a 15-minute wait, and a red
X — the verification loop lives on the wrong side of the push. greentree
moves it inside the workspace:

- **Content-addressed verdicts.** A test result belongs to a *tree hash*,
  not a commit or a timestamp. Revert an experiment and the previous verdict
  is simply valid again. Verification cost scales with content change, not
  git churn.
- **A publish gate, not a reminder.** `greentree publish` refuses — exit
  code 11 — unless the current tree has a passing, fresh verdict for every
  required check. There is no "oops, pushed before the tests finished."
- **One idempotent verb for agents.** `greentree gate` = run whatever isn't
  cached, then publish if green. Safe to call in a loop.

## Install

```sh
cargo install --git https://github.com/reachpad/greentree
```

Prebuilt binaries are planned (see [ROADMAP](docs/ROADMAP.md)). Linux and
macOS; Windows is currently unsupported.

## Quickstart

```sh
cd your-repo
greentree init      # detects pnpm/npm/cargo/go/uv, writes greentree.yaml
greentree test      # snapshot + run checks (instant when the tree is known)
greentree gate      # verify (cache-aware) then commit the verified tree
greentree watch     # run watch-marked checks whenever the tree settles
```

`watch` kills an in-flight check the moment you edit again (its verdict
could never bind to a tree anyway) and adaptively widens its settle window
so constant editing can't starve verification. `greentree gc` prunes old
snapshot anchors and trims logs.

Every verb takes `--json` for machine-readable output and returns stable
exit codes — the contract is in [docs/SPEC.md](docs/SPEC.md). Agents should
read [docs/AGENTS.md](docs/AGENTS.md).

## Configuration

```yaml
version: 1

checks:
  quick:
    run: pnpm lint && pnpm test
    watch: true         # run from `greentree watch` on each settle
  full:
    run: pnpm test && pnpm build
    required_for_publish: true
    fresh: 30m          # a pass older than this won't satisfy the gate
    timeout: 20m        # default 15m

snapshot:
  exclude: ["docs/generated/**"]   # paths checks may write without
                                   # invalidating their own verdict

inputs: [".env", "pnpm-lock.yaml"] # gitignored files that affect results:
                                   # hashed into the verdict key, itemized
```

No config? `greentree test` auto-detects the project type and runs its
conventional test command.

## How it works

1. **Snapshot** — a persistent *shadow index* (never your real index; no
   `index.lock` contention with your own git usage) is refreshed with
   `git add -A` and hashed with `git write-tree`. Cost is O(changed files)
   after the first run. Tracked + untracked-unignored files are captured.
2. **Verdict cache** — results are keyed by
   `(tree, check, command-hash, env-fingerprint)`. Only `pass`/`fail`
   enter the cache; timeouts, infra errors, and runs during which the tree
   changed are never cached.
3. **Publish** — `git commit-tree <verified-tree> -p HEAD`, a
   compare-and-swap ref update, and (with `--push`) an explicit
   `--force-with-lease` push. Every step is journaled: a publish killed at
   any point resumes exactly where it stopped. Every commit carries a
   `Greentree-Change-Id` trailer — the stable identity that will let stacks
   of verified changes survive rebases (see ROADMAP).

## Why not …

| | |
|---|---|
| **git-test** | Closest prior art — also caches by tree, but per *commit*: it tests ranges of existing commits. greentree tests the dirty tree *before* any commit exists and gates publishing on the result. |
| **Jujutsu** | jj snapshots the working copy as a commit (the same insight) but has no verdict cache and no publish gate. greentree borrows jj's ideas and stays plain git. |
| **pre-commit / husky** | Hooks re-run on every commit regardless of content, and `--no-verify` is a habit. greentree memoizes by content and *creates* the commit itself — there is no hook to skip. |
| **CI (Actions etc.)** | Runs after the push, on a cold runner, once per commit SHA. greentree runs before the push, in your warm workspace, once per unique tree. CI remains the authoritative re-check; greentree makes it boring. |

## Honesty section

Things greentree deliberately does **not** claim:

- The verdict cache is **machine-local and advisory** — it attests "this
  tree passed on this machine," not a tamper-proof supply-chain proof.
- `run:` commands come from repo config and execute with your privileges —
  the same trust model as `npm test` or `make`.
- Published commits bypass commit hooks by construction (`commit-tree`).
  That is intentional; greentree's checks are the hook.
- Dirty **submodules** are refused (their state is invisible to the
  superproject tree hash). Symlink *targets* outside the repo are not
  captured. GitHub **merge queues** are out of scope for now.

## License

Apache-2.0.
