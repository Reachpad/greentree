# Security

## Trust model

greentree verifies code you already trust, before it is pushed. It runs
`run:` commands from your repository's config on the machine where you
invoke greentree, with your privileges: the same trust model as `npm
test`, `make`, or a git hook.

greentree is not a sandbox, and it does not pull in and run other people's
commits. Verifying untrusted contributions (pull requests from forks) is
the job of an isolated, ephemeral runner, not greentree. Do not build a
setup where greentree runs commit-supplied code from an untrusted author
on a machine that holds credentials or persistent state.

## Credentials

`attest` and `publish --push` post commit statuses with a GitHub token,
resolved in order:

1. `GREENTREE_GITHUB_TOKEN` env
2. `GITHUB_TOKEN` env
3. `gh auth token --hostname github.com`, if a `gh` binary is on PATH

An explicit env var always wins over `gh`, so it stays the way to pin or
override the token used. The `gh` fallback exists so a box where you have
already run `gh auth login` just works, without the token being pasted into
a shell profile, an exported environment variable, or your shell history.

Where `gh` keeps that credential is `gh`'s business, and it varies: a
keyring where one is available, and a **plaintext `hosts.yml`** under
`~/.config/gh/` on a headless Linux box with no keyring — so treat the token
as on-disk unless you know otherwise. What using the fallback does buy is
that greentree never asks you to copy the token anywhere else, and never
puts it on a command line. Any failure resolving through `gh` (not
installed, not logged in, times out) falls through silently to "no token",
the same as before this chain existed.

Two guarantees:

- The token is scrubbed from every check subprocess, alongside the `GIT_*`
  redirection variables. A `run:` command cannot read it. The token
  reaches only the status API call.
- Use a fine-grained token scoped to the single repository with "Commit
  statuses: write" and nothing else. That is the minimum blast radius if a
  run is ever compromised. A classic PAT needs `repo:status`. This applies
  whichever of the three sources supplies the token.

## What a green status means

A `greentree/<check>` success status means: that check passed on this
exact commit's tree, on the machine that ran greentree, under the
environment declared in `inputs:`. It is a machine-local, advisory record,
not a tamper-proof supply chain attestation. Anyone who can run greentree
against the repo with a write-capable token can post one, so a required
status check is only as trustworthy as who can post it.

## Reporting

Report vulnerabilities through GitHub private security advisories on this
repository.
