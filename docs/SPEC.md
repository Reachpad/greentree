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

**The honest-tree rule.** Snapshot answers one question — *is there an honest
tree here?* — so it is refused only when there is no single tree to name: the
real index holds unmerged (conflicted) entries, or a submodule is dirty (exit
12 either way). An unfinished merge, cherry-pick, revert or rebase step whose
conflicts have all been resolved is an ordinary tree: it snapshots, tests and
caches like any other. Whether a *commit* may be created from it is a separate
question, answered under [Publish](#publish).

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
`absent`). Outcomes: `pass | fail | error | timeout | cancelled |
disk_exhausted`. Only `pass` and `fail` are cacheable. A run during which
the tree hash changed is `cancelled`: its result binds to no tree.

Checks run as `/bin/sh -c <run>` from the repo root, in their own process
group, with all `GIT_*` redirection variables scrubbed and
`GREENTREE_TREE_SHA` / `GREENTREE_CHECK` added. Timeout: SIGTERM to the
group, 5 s grace, SIGKILL. The process group is also killed when the check
exits while something it backgrounded still holds its output pipes — a
check is not a way to leave daemons running. Output is streamed to
`<git-dir>/greentree/logs/`, capped (head + tail retained), with a blake3
digest computed over the *full* stream.

### Disk

Disk is governed like time. `min_free_disk` — per check, or top level for
all of them; default 5 GiB; `"0"` disables — is a floor of free space on the
filesystem holding the repo root, read from statvfs's *available* blocks
(`f_bavail`). Accepted values: a plain decimal byte count (`0`, `500`), or a
plain decimal with a `K`/`M`/`G`/`T` suffix in powers of 1024, optionally
followed by `B` and in any case (`60G`, `1.5G`, `2gb`). Nothing else —
exponent forms (`1e3`), `inf`/`nan`, and any value that does not fit in a
`u64` are configuration errors (exit 14) rather than a silently saturated
floor no filesystem could ever meet.

- Below the floor at start, the check is **not started**: exit 16, no
  verdict recorded. Like an unsnapshotable tree, this is a refusal, not an
  outcome. A cache hit is never refused — no process runs.
- Falling below the floor *during* a run kills the process group through
  the same SIGTERM → 5 s → SIGKILL escalation a timeout uses, and records
  `disk_exhausted`: not cacheable, because the tree was never judged. Free
  space is sampled at most once a second.
- With the floor disabled (`"0"`) free space is never sampled at all — not
  even once — so a filesystem whose statvfs fails cannot refuse a run
  through a floor that is switched off. The verdict's two disk fields are
  then `0`, meaning "not observed".
- A refusal is fatal only to a one-shot verb. `watch` reports it like any
  other cycle outcome and keeps watching: the disk that is full now may not
  be in a minute, and dropping the watcher would leave the agent with no
  verification at all.

Verdict record fields (JSON, `schema_version: 2`): `tree`, `check`,
`command`, `shell`, `check_hash`, `env_fingerprint`, `env_inputs` (itemized),
`outcome`, `exit_code`, `signal`, `started`/`finished` (RFC 3339 UTC),
`duration_ms`, `disk_free_start_bytes`, `disk_free_min_bytes`,
`finished_unix`, `os`, `arch`, `git_version`, `greentree_version`,
`snapshot_ref`, `log_path`, `log_bytes`, `log_digest`, `log_truncated`.

The two disk fields are filesystem-level observations — free bytes when the
check started and the least seen while it ran — not a measurement of what
the check itself consumed: any other writer on the same filesystem confounds
attribution (and both are `0` when the floor is disabled: nothing was
observed). A record whose `schema_version` is not the current one is skipped
on load — checked explicitly, not left to whether it happens to still
deserialize — so a bump costs one cache miss per key, never an error.

The store is an append-only JSONL log at `<git-dir>/greentree/verdicts.jsonl`
(one record per line, last line wins per key, loaded into memory on open).
A write appends one line, so it stays O(one verdict) no matter how many
have accumulated; `gc` compacts the log to one line per live key. The
store is machine-local and advisory, not a tamper-proof attestation.

### Publish

`publish` (and the publish half of `gate`) answers the other half of the
question snapshot leaves open: *can I create the commit git would create?*

0. Refuse if a rebase is in progress (`rebase-merge`/`rebase-apply` — the
   latter is also where `git am` keeps its state, so the refusal names both
   continuations): the next commit belongs to that sequencer, which would
   replay its todo list over ours. Exit 12, the same "fix the repository
   state, then retry" class as an unsnapshotable tree. `test`, `status` and `watch` are
   unaffected — their verdicts are tree-keyed, so they are still there for
   `gate` or `attest` once `git rebase --continue` finishes. `gate` refuses
   before running any check, not after.
1. Snapshot; require a `pass` verdict — fresh within the check's `fresh:`
   window, under the *current* env fingerprint — for every
   `required_for_publish` check (or every check if none is marked). This
   gate runs unconditionally, **including when resuming an interrupted
   publish from the journal** — a journal never bypasses verification. A
   resume is also refused if HEAD has moved to a different branch, and an
   unparseable journal is a loud error, never treated as "no pending
   publish".
2. If the tree already equals `HEAD^{tree}`: no-op (push-only if `--push`).
   Exempt while a merge is in progress — a merge commit records history, not
   content, so an "ours" resolution still produces one.
3. `git commit-tree <tree> -p <parent>…` with the message plus a
   `Greentree-Change-Id: <32-hex>` trailer. The parents are recorded
   explicitly in a journal before any ref moves.
4. Compare-and-swap `update-ref refs/heads/<branch> <new> <expected-old>` —
   fails cleanly if anything else moved the branch.
5. Sync the real index to the new HEAD (`git read-tree HEAD`; retried under
   `index.lock` contention). Cost: staged-vs-unstaged distinction is dropped.
   From step 4 on, the publish has happened: retiring the sequencer state and
   syncing the index are housekeeping, and a failure in either is reported in
   `warnings` (and on stderr) with the publish still reported as a success —
   calling it a failure would invite a retry of a commit that already exists.
6. With `--push`: record the remote-tracking SHA first, then push with an
   explicit `--force-with-lease=<ref>:<recorded-sha>` (a bare lease is
   fooled by background fetches).

Every step is journaled in `<git-dir>/greentree/publish-journal.json`
(schema-versioned; `schema_version: 2` records `parents` as a list, and a
schema-1 journal's single `parent` is still read). A rerun after a crash at
any step resumes idempotently and never mints a second change-id for the same
publish; the reuse check matches on tree **and** the whole parent list.
Detached HEAD is refused. Publishing bypasses commit hooks by construction.

#### Finishing a merge, squash, cherry-pick or revert

With one of those in progress and its conflicts resolved, publish writes the
commit `git commit` would have written:

- **Parents**: HEAD, then every line of `MERGE_HEAD` — an octopus merge lists
  one SHA per line, so a three-way octopus publishes a three-parent commit.
  Exact repeats are dropped (`commit-tree` would drop them anyway). The
  compare-and-swap expectation stays the first parent, the HEAD publish read.
- **Stale merge state is not a merge.** A `MERGE_HEAD` line naming a commit
  HEAD already contains (`git merge-base --is-ancestor <line> HEAD`) is
  dropped: it is a file that outlived the commit which consumed it, and
  honoring it would mint a second merge commit with a parent that adds
  nothing. If every line is dropped, the publish is an ordinary
  single-parent one — and the stale files are retired on success anyway.
  `git merge --squash` is the same shape by design: no `MERGE_HEAD` at all,
  so it publishes one single-parent commit.
- **Message precedence**: an explicit `-m` wins; otherwise the sequencer's
  own message — `SQUASH_MSG` when it exists (what `git merge --squash`
  leaves, and what `git commit` prefers), else `MERGE_MSG` (git writes it for
  merges *and* for conflicted cherry-picks and reverts, where it carries the
  picked commit's message) — with `core.commentChar` lines removed; otherwise
  greentree's own `greentree: verified tree <short-sha>`. The change-id
  trailer is appended either way.
- **`core.commentChar`** is honored as git honors it: unset means `#`, an
  explicit single character means itself, and `auto` is resolved *per
  message* — the first of `#;@!$%^&|:` that no line of that message starts
  with — so a message whose lines begin with `#` keeps them, exactly as
  `git commit` would.
- **State retired on success**, once the commit exists and the branch points
  at it: `MERGE_HEAD`, `MERGE_MSG`, `SQUASH_MSG`, `MERGE_MODE`,
  `CHERRY_PICK_HEAD`, `REVERT_HEAD`, `AUTO_MERGE`. `.git/sequencer/` is
  deliberately left alone, so `git cherry-pick --continue` walks on through
  the remaining commits.
- A resolution that reproduces HEAD's tree exactly is a no-op for a
  cherry-pick or revert (nothing is committed, the state files stay put for
  `git cherry-pick --skip`), and a real merge commit for a merge. On that
  no-op path a stale `MERGE_HEAD`/`MERGE_MODE` is still removed; nothing
  else is touched.

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
- A refusal is not fatal: a cycle the disk floor stops (or any other
  pre-start refusal) is reported and the watcher keeps watching, rerunning
  on the next trigger.
- The global flock is held only during a cycle; `test`/`gate` interleave
  between cycles. A second watcher is refused via
  `<git-dir>/greentree/watch.pid` (stale pidfiles are detected and
  replaced). `--once` processes one completed cycle then exits.

### Gc (v0.2)

`greentree gc [--keep N] [--ttl DUR] [--log-budget-mb M]` deletes snapshot
anchors beyond the newest N or older than the TTL (defaults 50 / 14d), and
trims logs oldest-first to the byte budget (default 256 MB). Deleting an
anchor unpins objects; disk returns at the repository's next `git gc`.
gc also compacts the verdict log to one line per live key. Verdicts
themselves are never pruned — they are tree-keyed and stay valid without
their anchor.

### GitHub statuses (v0.3)

After a pushed publish, greentree posts one commit status per verified
check — context `greentree/<check>`, state `success`, description
`verified tree <short-sha>` — on the pushed commit. Mechanics:

- Compiled in via the default `github` cargo feature;
  `--no-default-features` builds the pure-git tool with no HTTP/TLS stack.
- Token, resolved in order: `GREENTREE_GITHUB_TOKEN` env, then
  `GITHUB_TOKEN` env, then (if a `gh` binary is on PATH) `gh auth token
  --hostname github.com` — greentree only supports github.com remotes, so
  the hostname is fixed. An env var always wins over `gh`. The `gh` step
  is bounded by a short timeout and any failure (missing binary, non-zero
  exit, empty output, timeout) falls through silently to the next source.
  A classic PAT needs `repo:status`; fine-grained needs "Commit statuses"
  read/write. No token from any of the three sources, or a non-github.com
  remote → posting is silently skipped (`publish`) or exit 15 (`attest`,
  which requires posting since that's its whole job).
- Statuses, not Check Runs: the Checks API is GitHub-App-only; statuses
  work with a PAT and satisfy branch-protection required status checks
  (match on the context string).
- Best-effort: a failed post never fails the publish (`statuses_error` in
  the JSON, warning on stderr). Rerunning `publish --push` re-posts; the
  same context overwrites, so retries are idempotent.
- `publish` and `attest` post only `success` (they refuse unverified trees
  before posting).
- Checks never receive the token: `GREENTREE_GITHUB_TOKEN`/`GITHUB_TOKEN`
  are scrubbed from every check subprocess (alongside the `GIT_*` vars).
  The token reaches only the status API call.

### Attest (the normal-git-push half of the loop)

`greentree attest` posts `greentree/<check>` statuses for HEAD, given:
the working tree is byte-identical to HEAD's tree (attest stamps only
committed state), and every required check holds a passing fresh verdict
for that tree. No commit is created; nothing is pushed. Flow: verify
while working, commit and push with plain git, attest. Refusals use exit
11; a missing token is exit 15.

## Exit codes (stable contract)

| code | meaning |
|---|---|
| 0 | success (checks green / published / no-op) |
| 1 | unexpected or infrastructure error |
| 2 | CLI usage error |
| 10 | a check ran and failed |
| 11 | publish refused: tree not verified |
| 12 | the repository state blocks the verb: conflicted index or dirty submodule (no honest tree), or publish during a rebase |
| 13 | another greentree process holds the lock |
| 14 | configuration error |
| 15 | publish machinery failed (CAS refused, push rejected, no remote) |
| 16 | free disk below `min_free_disk`: check refused, or aborted mid-run |

## JSON output

Every verb accepts `--json` and prints exactly one JSON object to stdout.
Errors in JSON mode print `{"error": "...", "exit_code": N}`. Shapes:

Exit 16 has two shapes from `test`, and an agent branching on the code must
handle both: a **pre-start refusal** never ran anything, so it prints the
error object `{"error", "exit_code": 16}`; a check killed **mid-run** ran, so
it prints the full results object with that check's `outcome` set to
`disk_exhausted` (and `ok: false`).

- `test`: `{tree, ok, results: [{check, tree, outcome, cached, exit_code,
  duration_ms, log, log_tail?}]}`. `ok` is true only when every check
  passed **and** all verdicts bind to the same tree (an edit between
  checks makes `ok` false even if each check passed on its own tree).
- `status`: `{tree, branch, head, tree_at_head, merge_in_progress,
  publishable, publish_blocked, checks: [{check, state, finished}],
  pending_publish}`, where `publish_blocked` is null or the reason publish
  would refuse regardless of the verdicts (a rebase in progress), in which
  case `publishable` is false even with every check green, and `state` ∈
  `pass | fail | stale | missing` — status reads the verdict store, which
  holds only the cacheable outcomes, so a check that timed out or was
  cancelled reads as `missing` until it is rerun. `branch` is `null` when
  HEAD is detached (mid-rebase, or a bare checkout); the human line prints
  `(detached)`. `merge_in_progress` is true only while `MERGE_HEAD` names a
  commit HEAD does not already contain — the state in which publish writes a
  merge commit even though `tree_at_head` is true.
  `status` waits up to 3 s for the lock instead of failing with exit 13
  while a check is running.
- `publish`: `{tree, branch, noop, commit, change_id, pushed, resumed,
  verified_by, statuses_posted, statuses_error}`, plus `warnings: [string]`
  when post-commit housekeeping failed (omitted when empty)
- `gate`: `{gate: "published", checks: [{check, tree, outcome, cached,
  duration_ms}], publish}` — `publish` is the same object the `publish`
  verb prints (statuses fields included) — or
  `{gate: "refused", check, tree, outcome, log, log_tail}`
- `attest`: `{commit, tree, checks, statuses_posted}`
- `init`: `{config_written, config_path, checks, tree}`
- `watch --json`: one line per visible cycle:
  `{tree, results: [{check, outcome, cached, duration_ms}]}`. A cycle refused
  before it started (free disk below the floor) prints the same shape with
  `tree: null`, the refused check's `outcome` as `disk_exhausted`, and an
  extra `error` naming the reason; the watcher keeps running.
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
