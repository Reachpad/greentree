# Roadmap

v0.1 is the primitive: snapshot, verdict cache, publish gate, `gate`. Each
milestone after it stays small and composes with the last.

## v0.2 — watch (shipped)

- `greentree watch`: filesystem watcher + adaptive debounce; runs
  `watch: true` checks as the tree settles; kill-on-edit; single-instance
  pidfile; `--once` for scripting.
- `greentree gc`: snapshot-ref retention (keep-last-N + TTL), log budget.
- (Deferred: sqlite store — the flock still serializes all writers.)

## v0.3 — GitHub statuses (shipped)

- `publish --push` posts a `greentree/<check>` commit status on the pushed
  SHA (PAT: classic `repo:status` or fine-grained "Commit statuses" RW);
  best-effort and idempotent. Statuses satisfy branch-protection required
  checks; the Checks API is GitHub-App-only and comes later.

## v0.4 — attest, the gate before GitHub (shipped)

- `greentree attest` posts `greentree/<check>` statuses for HEAD when its
  tree is verified, so a plain `git commit && git push` can end attested
  without `gate` building the commit. Verify in the workspace, push with
  git, attest; a required status check enforces it server-side with no CI
  compute.
- The token is scrubbed from every check subprocess: verification can
  never leak the credential that attests it.
- `action/` remains the cold-isolation fallback for pull requests from
  forks (untrusted code belongs in an ephemeral runner, never inline).

## v0.5 — remote executor: point verification at a warm workspace

The OSS core runs verification wherever it is invoked. This adds an
executor endpoint so `greentree test`/`gate` can dispatch the check run to
a persistent warm workspace instead of the local machine, with a shared,
authoritative verdict cache and a single controlled environment (which
removes the per-machine "works on my machine" gap in the env fingerprint).
The tool stays standalone; the remote is opt-in. [reachpad](https://reachpad.dev)
is the managed warm workspace this points at.

## v0.6 — worktree executor

- Materialize *any* tree (current or anchored snapshot) into an ephemeral
  `git worktree` and run checks there: `test --tree <sha>`.
- Kills the ABA blind spot (checks no longer share the live working copy)
  and is the functional prerequisite for verifying interior stack levels.

## v0.7 — stacks, local

A stack is an ordered chain of *changes*; a change is the stable
`Greentree-Change-Id` already stamped on every published commit. Verdicts
stay tree-keyed — which makes restack re-verification exact by
construction: levels whose trees changed re-verify, levels whose trees
didn't stay verified, message/reorder churn costs zero.

- `greentree stack`: the change graph reconstructed from trailers.
- `greentree restack`: rebase survivors, drop content-empty changes
  (post-squash-merge), re-verify only changed trees.

## v0.8 — GitHub projection for stacks

- One deterministic branch (`greentree/<change-id>`) + one base-chained PR
  per change, rebuilt idempotently from local truth (`gh` first, REST
  fallback); per-level commit statuses; post-merge restack.

## v0.9+ — the review loop, and isolation

- GitHub App: real Check Runs; `pull_request_review_comment` webhooks
  routed to the agent's live workspace keyed by change-id; one verified
  update per review batch. Shared org verdict cache behind the same store
  trait.
- Container executor (`docker`/`podman`) for hermetic checks.

## Non-goals

No GitHub Actions YAML interpreter. No hosted runner fleet. No pipeline
UI. No multi-agent scheduling. No Kubernetes or Firecracker orchestration.
Those obscure the primitive; greentree stays a sharp tool that existing
CI, stack tooling, and workspaces can build on.
