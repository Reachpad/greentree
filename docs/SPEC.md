# greentree specification (v0.1)

The contracts in this document — exit codes, JSON shapes, the verdict
schema, the ref namespace, the trailer — are stable interfaces. Everything
else is implementation detail.

## The protocol

### Snapshot

The dirty working tree is content-addressed as a git tree object:

1. A **shadow index** at `<git-dir>/greentree/index` is seeded from the real
   index (honoring `skip-worktree`/`assume-unchanged`) and re-seeded whenever
   the real index changes. The real index is never written.
2. `git add -A` (with `core.untrackedCache=true`) refreshes the shadow
   index against the working tree; `snapshot.exclude` patterns are then
   removed from the shadow index (`git rm --cached`, glob pathspecs);
   `git write-tree` emits the tree SHA.
3. Captured: tracked files + untracked files not ignored by `.gitignore`,
   minus `snapshot.exclude`. Not captured: ignored files (declare relevant
   ones in `inputs:`), symlink targets outside the repo, dirty submodule
   contents (refused, exit 12).

Snapshotting is refused mid-merge/rebase/cherry-pick/revert and when the
real index holds unmerged entries.

Trees a check actually runs against are **anchored** as commits at
`refs/greentree/snapshots/<tree-sha>` so the exact tested state survives
`git gc` and can be diffed or materialized later.

### Verdicts

A verdict binds an outcome to a **key**:

```
(tree_sha, check_name, blake3(shell + command), env_fingerprint)
```

`env_fingerprint` is a blake3 over the itemized digests of every file
matching the config's `inputs:` globs (a missing input is recorded as
`absent`). Outcomes: `pass | fail | error | timeout | cancelled`. Only
`pass` and `fail` are cacheable. A run during which the tree hash changed is
`cancelled`: its result binds to no tree.

Checks run as `/bin/sh -c <run>` from the repo root, in their own process
group, with all `GIT_*` redirection variables scrubbed and
`GREENTREE_TREE_SHA` / `GREENTREE_CHECK` added. Timeout: SIGTERM to the
group, 5 s grace, SIGKILL. The process group is also killed when the check
exits while something it backgrounded still holds its output pipes — a
check is not a way to leave daemons running. Output is streamed to
`<git-dir>/greentree/logs/`, capped (head + tail retained), with a blake3
digest computed over the *full* stream.

Verdict record fields (JSON, `schema_version: 1`): `tree`, `check`,
`command`, `shell`, `check_hash`, `env_fingerprint`, `env_inputs` (itemized),
`outcome`, `exit_code`, `signal`, `started`/`finished` (RFC 3339 UTC),
`duration_ms`, `finished_unix`, `os`, `arch`, `git_version`,
`greentree_version`, `snapshot_ref`, `log_path`, `log_bytes`, `log_digest`,
`log_truncated`.

The store is machine-local and advisory. It is not a tamper-proof
attestation.

### Publish

`publish` (and the publish half of `gate`):

1. Snapshot; require a `pass` verdict — fresh within the check's `fresh:`
   window, under the *current* env fingerprint — for every
   `required_for_publish` check (or every check if none is marked). This
   gate runs unconditionally, **including when resuming an interrupted
   publish from the journal** — a journal never bypasses verification. A
   resume is also refused if HEAD has moved to a different branch, and an
   unparseable journal is a loud error, never treated as "no pending
   publish".
2. If the tree already equals `HEAD^{tree}`: no-op (push-only if `--push`).
3. `git commit-tree <tree> -p <parent>` with the message plus a
   `Greentree-Change-Id: <32-hex>` trailer. The parent is recorded
   explicitly in a journal before any ref moves.
4. Compare-and-swap `update-ref refs/heads/<branch> <new> <expected-old>` —
   fails cleanly if anything else moved the branch.
5. Sync the real index to the new HEAD (`git read-tree HEAD`; retried under
   `index.lock` contention). Cost: staged-vs-unstaged distinction is dropped.
6. With `--push`: record the remote-tracking SHA first, then push with an
   explicit `--force-with-lease=<ref>:<recorded-sha>` (a bare lease is
   fooled by background fetches).

Every step is journaled in `<git-dir>/greentree/publish-journal.json`
(schema-versioned); a rerun after a crash at any step resumes idempotently
and never mints a second change-id for the same publish. Detached HEAD is
refused. Publishing bypasses commit hooks by construction.

### Watch (v0.2)

`greentree watch` runs every `watch: true` check when the tree settles.
Semantics:

- **Kill-on-edit**: a mutation event (create/modify/remove/close-write —
  never read/access events) on a relevant path kills the in-flight check's
  process group; the cycle records `cancelled` and is never cached.
- **Adaptive quiet-window**: 300 ms, doubling after each cancelled cycle,
  capped at 5 s; reset on a completed cycle. Prevents starvation under a
  continuously editing agent.
- Relevance filter: paths under the git dir or matching `snapshot.exclude`
  never trigger or cancel; everything else does, and snapshot dedupe
  absorbs ignored-file noise (same tree = cache hit = no visible cycle).
- The global flock is held only during a cycle; `test`/`gate` interleave
  between cycles. A second watcher is refused via
  `<git-dir>/greentree/watch.pid` (stale pidfiles are detected and
  replaced). `--once` processes one completed cycle then exits.

### Gc (v0.2)

`greentree gc [--keep N] [--ttl DUR] [--log-budget-mb M]` deletes snapshot
anchors beyond the newest N or older than the TTL (defaults 50 / 14d), and
trims logs oldest-first to the byte budget (default 256 MB). Deleting an
anchor unpins objects; disk returns at the repository's next `git gc`.
Verdicts are never pruned — they are tree-keyed and stay valid without
their anchor.

### GitHub statuses (v0.3)

After a pushed publish, greentree posts one commit status per verified
check — context `greentree/<check>`, state `success`, description
`verified tree <short-sha>` — on the pushed commit. Mechanics:

- Compiled in via the default `github` cargo feature;
  `--no-default-features` builds the pure-git tool with no HTTP/TLS stack.
- Token: `GREENTREE_GITHUB_TOKEN` or `GITHUB_TOKEN` (classic PAT needs
  `repo:status`; fine-grained needs "Commit statuses" read/write). No
  token, or a non-github.com remote → posting is silently skipped.
- Statuses, not Check Runs: the Checks API is GitHub-App-only; statuses
  work with a PAT and satisfy branch-protection required status checks
  (match on the context string).
- Best-effort: a failed post never fails the publish (`statuses_error` in
  the JSON, warning on stderr). Rerunning `publish --push` re-posts; the
  same context overwrites, so retries are idempotent.
- `publish` and `attest` post only `success` (they refuse unverified trees
  before posting). `serve` posts real outcomes, including `failure` — a
  failing pushed commit deserves a red X, not silence.

### Attest (the normal-git-push half of the loop)

`greentree attest` posts `greentree/<check>` statuses for HEAD, given:
the working tree is byte-identical to HEAD's tree (attest stamps only
committed state), and every required check holds a passing fresh verdict
for that tree. No commit is created; nothing is pushed. Flow: verify
while working, commit and push with plain git, attest. Refusals use exit
11; a missing token is exit 15.

### Serve (the persistent warm runner)

`greentree serve [--remote R] [--branch B] [--interval 30s] [--once]`
turns a dedicated clone on an always-on machine into the CI:

- Polls `R/B`; on a new SHA: `checkout --detach` + `reset --hard` +
  `git clean -fd` (ignored files — the warm caches — survive), runs the
  required checks in the warm working copy, and posts one status per
  outcome.
- serve OWNS its clone and refuses to start over uncommitted changes.
- Verdicts are tree-keyed as everywhere else: re-pushed content, reverts,
  and message-only rewrites are cache hits and re-attest instantly.
- Statuses are only posted when every verdict binds to the commit's own
  tree (a check that mutates the repo forfeits attestation for that run).
- A commit that cannot be verified at all (broken config, infra error) is
  logged, recorded as seen, and skipped — the loop never wedges.
- State: `<git-dir>/greentree/serve-last` (last handled SHA).

## Exit codes (stable contract)

| code | meaning |
|---|---|
| 0 | success (checks green / published / no-op) |
| 1 | unexpected or infrastructure error |
| 2 | CLI usage error |
| 10 | a check ran and failed |
| 11 | publish refused: tree not verified |
| 12 | unsnapshotable state (mid-merge, conflicted index, dirty submodule) |
| 13 | another greentree process holds the lock |
| 14 | configuration error |
| 15 | publish machinery failed (CAS refused, push rejected, no remote) |

## JSON output

Every verb accepts `--json` and prints exactly one JSON object to stdout.
Errors in JSON mode print `{"error": "...", "exit_code": N}`. Shapes:

- `test`: `{tree, ok, results: [{check, tree, outcome, cached, exit_code,
  duration_ms, log, log_tail?}]}`. `ok` is true only when every check
  passed **and** all verdicts bind to the same tree (an edit between
  checks makes `ok` false even if each check passed on its own tree).
- `status`: `{tree, branch, head, tree_at_head, publishable,
  checks: [{check, state, finished}], pending_publish}`
  where `state` ∈ `pass | fail | stale | missing | error | timeout | cancelled`.
  `status` waits up to 3 s for the lock instead of failing with exit 13
  while a check is running.
- `publish`: `{tree, branch, noop, commit, change_id, pushed, resumed,
  verified_by, statuses_posted, statuses_error}`
- `gate`: `{gate: "published", checks: [{check, tree, outcome, cached,
  duration_ms}], publish}` — `publish` is the same object the `publish`
  verb prints (statuses fields included) — or
  `{gate: "refused", check, tree, outcome, log, log_tail}`
- `attest`: `{commit, tree, checks, statuses_posted}`
- `serve --json`: one line per handled commit:
  `{commit, tree, ok, results: [{check, outcome, cached, duration_ms}],
  statuses_posted}`
- `init`: `{config_written, config_path, checks, tree}`
- `watch --json`: one line per visible cycle:
  `{tree, results: [{check, outcome, cached, duration_ms}]}`
- `gc`: `{snapshots_pruned, snapshots_kept, logs_deleted, log_bytes_freed}`

## Namespaces owned by greentree

- Refs: everything under `refs/greentree/`.
  - `refs/greentree/snapshots/<tree-sha>` — anchored tested snapshots.
  - `refs/greentree/changes/<change-id>` — **reserved** for the stack
    index: a rebuildable projection over `Greentree-Change-Id` trailers,
    never authoritative.
- Trailer: `Greentree-Change-Id` — the stable identity of a logical change
  across rewrites. 32 lowercase hex characters.
- State: `<git-dir>/greentree/` (shadow index, verdicts, logs, journal,
  lock). Safe to delete entirely; everything is a cache except an
  in-flight publish journal.

## Known limitations (documented, not hidden)

- Trees observed mid-run only via before/after hashing (v0.1): an
  edit-and-revert *during* a check run (ABA) is undetectable until the
  watcher (v0.2) and the worktree executor (v0.4) land.
- Ignored files affect results only if declared in `inputs:` — undeclared
  environment (toolchain versions, system libraries) is the user's honor.
- File mode bits beyond the executable bit are not represented by git and
  therefore not by greentree.
- GitHub merge queues synthesize their own commits and are incompatible
  with pre-push verification; out of scope.
