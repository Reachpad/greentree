//! GitHub commit statuses (v0.3).
//!
//! Statuses — not Check Runs — because the Checks API is GitHub-App-only,
//! while statuses work with a plain PAT (classic `repo:status` scope, or
//! fine-grained "Commit statuses" read/write) and still satisfy required
//! status checks in branch protection. Posting is best-effort: a failed
//! post never fails the publish; rerunning `publish --push` re-posts
//! (same context = overwrite = idempotent).

use serde::Serialize;

use crate::git::Git;
use crate::runner::short;
use crate::{Error, Result};

pub const TOKEN_ENVS: &[&str] = &["GREENTREE_GITHUB_TOKEN", "GITHUB_TOKEN"];

#[derive(Debug, PartialEq, Eq)]
pub struct RepoSlug {
    pub owner: String,
    pub repo: String,
}

/// Extract owner/repo from a github.com remote URL. Returns None for
/// non-GitHub remotes (posting is silently skipped for those).
pub fn parse_github_remote(url: &str) -> Option<RepoSlug> {
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.strip_prefix("git://github.com/"))?;
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let (owner, repo) = rest.split_once('/')?;
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }
    Some(RepoSlug {
        owner: owner.to_string(),
        repo: repo.to_string(),
    })
}

pub fn token_from_env() -> Option<String> {
    TOKEN_ENVS
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
}

/// URL of the repo's first remote, if any.
pub fn remote_url(git: &Git) -> Option<String> {
    let remotes = git.run(["remote"]).ok()?;
    let remote = remotes.lines().next()?;
    git.run(["config", "--get", &format!("remote.{remote}.url")])
        .ok()
        .filter(|u| !u.is_empty())
}

#[derive(Serialize)]
struct StatusBody<'a> {
    state: &'a str,
    context: String,
    description: String,
}

/// One check's outcome to report on a commit.
pub struct StatusEntry {
    pub check: String,
    pub success: bool,
    pub description: String,
}

/// Post one `greentree/<check>` status per entry on the commit. `publish`
/// and `attest` only ever post successes (unverified trees are refused
/// before this point); `serve` reports real outcomes, because a failing
/// pushed commit deserves a red X, not silence. Returns contexts posted.
pub fn post_entries(git: &Git, commit: &str, entries: &[StatusEntry]) -> Result<Vec<String>> {
    let token = token_from_env().ok_or_else(|| {
        Error::Publish("no GREENTREE_GITHUB_TOKEN or GITHUB_TOKEN in the environment".into())
    })?;
    let url = remote_url(git)
        .ok_or_else(|| Error::Publish("no git remote with a URL configured".into()))?;
    let slug = parse_github_remote(&url)
        .ok_or_else(|| Error::Publish(format!("remote {url:?} is not a github.com repo")))?;

    let api = format!(
        "https://api.github.com/repos/{}/{}/statuses/{}",
        slug.owner, slug.repo, commit
    );
    let mut posted = Vec::new();
    for entry in entries {
        let body = StatusBody {
            state: if entry.success { "success" } else { "failure" },
            context: format!("greentree/{}", entry.check),
            description: entry.description.clone(),
        };
        let response = ureq::post(&api)
            .set("Authorization", &format!("Bearer {token}"))
            .set("Accept", "application/vnd.github+json")
            .set("User-Agent", &format!("greentree/{}", crate::VERSION))
            .send_json(&body);
        match response {
            Ok(_) => posted.push(body.context),
            Err(e) => {
                return Err(Error::Publish(format!(
                    "posting status {} failed: {e} ({} already posted)",
                    body.context,
                    posted.len()
                )))
            }
        }
    }
    Ok(posted)
}

/// Success statuses for verified checks (the publish/attest path).
pub fn post_statuses(
    git: &Git,
    commit: &str,
    tree: &str,
    checks: &[String],
) -> Result<Vec<String>> {
    let entries: Vec<StatusEntry> = checks
        .iter()
        .map(|check| StatusEntry {
            check: check.clone(),
            success: true,
            description: format!("verified tree {}", short(tree)),
        })
        .collect();
    post_entries(git, commit, &entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_github_remote_forms() {
        for url in [
            "https://github.com/reachpad/greentree",
            "https://github.com/reachpad/greentree.git",
            "git@github.com:reachpad/greentree.git",
            "ssh://git@github.com/reachpad/greentree",
            "git://github.com/reachpad/greentree.git",
        ] {
            let slug = parse_github_remote(url).unwrap_or_else(|| panic!("failed on {url}"));
            assert_eq!(slug.owner, "reachpad");
            assert_eq!(slug.repo, "greentree");
        }
    }

    #[test]
    fn rejects_non_github_remotes() {
        for url in [
            "https://gitlab.com/o/r.git",
            "git@bitbucket.org:o/r.git",
            "https://github.com/only-owner",
            "/local/path/repo.git",
        ] {
            assert!(parse_github_remote(url).is_none(), "accepted {url}");
        }
    }
}
