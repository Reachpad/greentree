# greentree action

Runs `greentree test --json` on the checkout, with the verdict cache
(`.git/greentree`) persisted through `actions/cache`.

Because verdicts are keyed by **tree content**, a commit whose tree was
already verified — locally before the push, or by a previous run — is a
cache hit and runs nothing. A rebase or squash that didn't change content
costs zero; changed content genuinely re-runs.

```yaml
jobs:
  verify:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: reachpad/greentree/action@main
        # with:
        #   check: full     # one named check instead of all
        #   fresh: "true"   # distrust restored verdicts; always re-run
```

Notes:

- `fresh: "true"` is for teams that use this job as the authoritative
  required check and don't want restored verdicts trusted.
- The stateful loop (warm processes, kill-on-edit) doesn't apply on a
  fresh runner — this action is the *re-check* side of greentree, not a
  replacement for running it in your workspace.
- Installation currently builds from source (`cargo install`); prebuilt
  binaries will remove that cost.
