# greentree for coding agents

greentree's primary user is an agent. Everything below is copy-paste
material for wiring one up.

## The loop

Replace "run tests, then commit and push" with:

```sh
greentree gate --json -m "<commit message>"
```

`gate` runs every required check (instant for trees already verified),
then creates the commit from the verified tree. It is idempotent — calling
it twice in a row is a no-op. Branch on exit code, not on output text:

| exit | meaning | agent action |
|---|---|---|
| 0 | published (or already published) | done |
| 10 | a check failed | read `log_tail` in the JSON, fix, rerun |
| 11 | tree changed after its last verification | rerun `gate` |
| 12 | repo mid-merge/rebase or dirty submodule | resolve, rerun |
| 13 | another greentree run in flight | wait, rerun |

While iterating, `greentree test --json` gives the same verdicts without
publishing. `greentree status --json` answers "would publish succeed?"
without running anything new.

## Project-instructions snippet

Add to the repository's `AGENTS.md` / `CLAUDE.md`:

```markdown
## Verification

This repo uses greentree. Do not run `git commit` or `git push` directly.
To land changes: `greentree gate --json -m "<message>"`. If it exits 10,
read the failing check's log_tail, fix, and rerun. Test without
publishing: `greentree test --json`.
```

## Claude Code hook (optional, enforcing)

Blocks manual commits/pushes so the gate is the only door. In
`.claude/settings.json`:

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "command": "jq -r '.tool_input.command' | grep -qE '(^|[;&|[:space:]])git[[:space:]]+(commit|push)' && { echo 'Use greentree gate instead of git commit/push (see AGENTS.md).' >&2; exit 2; } || exit 0"
          }
        ]
      }
    ]
  }
}
```

## Notes for agent authors

- Always pass `--json`; stdout is exactly one JSON object.
- A cached verdict is as authoritative as a fresh run — do not "re-run to
  be sure"; the tree hash already proves nothing changed.
- If your agent edits files while a check runs, the verdict comes back
  `cancelled` (the result would bind to no tree). Pause edits during
  `gate`, or just rerun — the next run starts from the settled tree.
- `GREENTREE_TREE_SHA` and `GREENTREE_CHECK` are set inside check
  processes; `GIT_*` redirection variables are scrubbed.
