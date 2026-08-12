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

`attest` and `publish --push` post commit statuses with a GitHub token
read from `GREENTREE_GITHUB_TOKEN` or `GITHUB_TOKEN`. Two guarantees:

- The token is scrubbed from every check subprocess, alongside the `GIT_*`
  redirection variables. A `run:` command cannot read it. The token
  reaches only the status API call.
- Use a fine-grained token scoped to the single repository with "Commit
  statuses: write" and nothing else. That is the minimum blast radius if a
  run is ever compromised. A classic PAT needs `repo:status`.

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
