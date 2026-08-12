//! Command dispatch. All printing and exit-code mapping lives here; the lib
//! modules return data. JSON goes to stdout; human text and tracing to
//! stderr-adjacent channels stay out of JSON's way.

use std::path::Path;
use std::time::SystemTime;

use serde_json::json;

use crate::cache::{JsonStore, Outcome, VerdictKey, VerdictStore};
use crate::cli::{Cli, Command};
use crate::config::{Config, CONFIG_FILE};
use crate::git::Git;
use crate::publish::{load_journal, publish, PublishOptions};
use crate::runner::{short, RunResult};
use crate::snapshot::snapshot;
use crate::{exit, lock, Error};

pub fn run(cli: Cli) -> u8 {
    let dir = cli
        .dir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| Path::new(".").into()));
    match dispatch(&cli, &dir) {
        Ok(code) => code,
        Err(e) => {
            let code = e.exit_code();
            if cli.json {
                println!("{}", json!({ "error": e.to_string(), "exit_code": code }));
            } else {
                eprintln!("error: {e}");
            }
            code
        }
    }
}

fn dispatch(cli: &Cli, dir: &Path) -> crate::Result<u8> {
    let git = Git::discover(dir)
        .map_err(|e| Error::Config(format!("not a git repository ({e}); run `git init` first")))?;

    match &cli.command {
        Command::Init => init(cli, &git),
        Command::Test { check, no_cache } => test(cli, &git, check.as_deref(), *no_cache),
        Command::Status => status(cli, &git),
        Command::Publish { push, message } => {
            let _lock = lock::acquire(&git.state_dir())?;
            let cfg = Config::effective(&git.root)?;
            let store = JsonStore::open(&git.state_dir())?;
            let report = publish(
                &git,
                &cfg,
                &store,
                &PublishOptions {
                    push: *push,
                    message: message.clone(),
                },
            )?;
            let statuses = maybe_post_statuses(&git, &cfg, &report);
            emit_publish(cli, &report, &statuses);
            Ok(exit::OK)
        }
        Command::Gate { push, message } => gate(cli, &git, *push, message.clone()),
        Command::Attest => attest(cli, &git),
        Command::Watch { once } => {
            crate::watch::watch(
                &git,
                &crate::watch::WatchOptions {
                    once: *once,
                    json: cli.json,
                },
            )?;
            Ok(exit::OK)
        }
        Command::Gc {
            keep,
            ttl,
            log_budget_mb,
        } => {
            let _lock = lock::acquire(&git.state_dir())?;
            let opts = crate::gc::GcOptions {
                keep: *keep,
                ttl: humantime::parse_duration(ttl)
                    .map_err(|e| Error::Config(format!("invalid ttl {ttl:?}: {e}")))?,
                log_budget: log_budget_mb * 1024 * 1024,
            };
            let report = crate::gc::gc(&git, &opts)?;
            if cli.json {
                println!("{}", serde_json::to_string(&report)?);
            } else {
                println!(
                    "pruned {} snapshot anchors (kept {}), deleted {} logs ({} bytes)",
                    report.snapshots_pruned,
                    report.snapshots_kept,
                    report.logs_deleted,
                    report.log_bytes_freed
                );
            }
            Ok(exit::OK)
        }
    }
}

fn init(cli: &Cli, git: &Git) -> crate::Result<u8> {
    let _lock = lock::acquire(&git.state_dir())?;
    let path = git.root.join(CONFIG_FILE);
    let mut wrote = false;
    let cfg = match Config::load(&git.root)? {
        Some(cfg) => cfg,
        None => {
            let cfg = Config::detect(&git.root).ok_or_else(|| {
                Error::Config(
                    "no recognized project type; write a greentree.yaml by hand \
                     (see docs/SPEC.md)"
                        .into(),
                )
            })?;
            std::fs::write(&path, cfg.to_yaml())?;
            wrote = true;
            cfg
        }
    };

    if !cli.json {
        eprintln!("warming snapshot index (first run scans the whole tree)...");
    }
    let tree = snapshot(git, &cfg)?;

    if cli.json {
        println!(
            "{}",
            json!({
                "config_written": wrote,
                "config_path": path.display().to_string(),
                "checks": cfg.checks.keys().collect::<Vec<_>>(),
                "tree": tree,
            })
        );
    } else {
        if wrote {
            println!("wrote {CONFIG_FILE} with checks: {}", names(&cfg));
        } else {
            println!("using existing {CONFIG_FILE} (checks: {})", names(&cfg));
        }
        println!("tree {}  — run `greentree test`", short(&tree));
    }
    Ok(exit::OK)
}

fn test(cli: &Cli, git: &Git, only: Option<&str>, no_cache: bool) -> crate::Result<u8> {
    let _lock = lock::acquire(&git.state_dir())?;
    let cfg = Config::effective(&git.root)?;
    let mut store = JsonStore::open(&git.state_dir())?;

    let selected: Vec<(String, crate::config::Check)> = match only {
        Some(name) => {
            let check = cfg.checks.get(name).ok_or_else(|| {
                Error::Config(format!("no check named {name:?} (have: {})", names(&cfg)))
            })?;
            vec![(name.to_string(), check.clone())]
        }
        None => cfg
            .checks
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    };

    // Thread the tree hash through the run: each check's after-hash is the
    // next check's before-hash, so an unchanged tree is hashed once, not
    // once per check.
    let mut results: Vec<(String, RunResult)> = Vec::new();
    let mut pre_tree: Option<String> = None;
    for (name, check) in &selected {
        let r = crate::runner::run_check_with(
            git, &cfg, name, check, &mut store, !no_cache, None, pre_tree,
        )?;
        pre_tree = Some(r.tree_after.clone());
        results.push((name.clone(), r));
    }

    let worst = results
        .iter()
        .map(|(_, r)| r.verdict.outcome)
        .fold(Outcome::Pass, worse);
    // `ok` promises "this tree is verified" — it must not be true when the
    // verdicts span different trees (an edit between checks).
    let consistent = results
        .windows(2)
        .all(|w| w[0].1.verdict.tree == w[1].1.verdict.tree);

    if cli.json {
        println!(
            "{}",
            json!({
                "tree": results.last().map(|(_, r)| r.tree_after.clone()),
                "ok": worst == Outcome::Pass && consistent,
                "results": results.iter().map(|(name, r)| json!({
                    "check": name,
                    "tree": r.verdict.tree,
                    "outcome": r.verdict.outcome.as_str(),
                    "cached": r.cached,
                    "exit_code": r.verdict.exit_code,
                    "duration_ms": r.verdict.duration_ms,
                    "log": r.verdict.log_path,
                    "log_tail": if r.verdict.outcome == Outcome::Pass { None } else { Some(&r.log_tail) },
                })).collect::<Vec<_>>(),
            })
        );
    } else {
        for (name, r) in &results {
            println!(
                "tree {}  {name} {}{}",
                short(&r.verdict.tree),
                mark(r.verdict.outcome),
                if r.cached {
                    " (cached)".to_string()
                } else {
                    format!(" ({:.1}s)", r.verdict.duration_ms as f64 / 1000.0)
                }
            );
            if r.verdict.outcome != Outcome::Pass {
                print_tail(&r.log_tail);
            }
        }
    }

    Ok(match worst {
        Outcome::Pass => exit::OK,
        Outcome::Fail => exit::CHECK_FAILED,
        _ => exit::ERROR,
    })
}

fn status(cli: &Cli, git: &Git) -> crate::Result<u8> {
    // status is what agents poll while a check runs — wait briefly for the
    // lock instead of failing with exit 13 during every cycle.
    let _lock = lock::acquire_wait(&git.state_dir(), std::time::Duration::from_secs(3))?;
    let cfg = Config::effective(&git.root)?;
    let store = JsonStore::open(&git.state_dir())?;
    let tree = snapshot(git, &cfg)?;
    let (env_fp, _) = crate::config::env_fingerprint(&git.root, &cfg.inputs)?;

    let head = git.rev_parse_opt("HEAD")?;
    let head_tree = match &head {
        Some(h) => git.rev_parse_opt(&format!("{h}^{{tree}}"))?,
        None => None,
    };
    let branch = git
        .run_unchecked(["symbolic-ref", "-q", "--short", "HEAD"])?
        .stdout;
    let branch = String::from_utf8_lossy(&branch).trim().to_string();
    let now = SystemTime::now();

    let mut checks = Vec::new();
    let mut publishable = true;
    for (name, check) in cfg.required_checks() {
        let key = VerdictKey {
            tree: tree.clone(),
            check: name.clone(),
            check_hash: check.hash(),
            env_fingerprint: env_fp.clone(),
        };
        let v = store.get(&key);
        let state = match &v {
            None => "missing",
            Some(v) if v.outcome == Outcome::Pass => {
                if v.is_fresh(check.fresh_duration()?, now) {
                    "pass"
                } else {
                    "stale"
                }
            }
            Some(v) => v.outcome.as_str(),
        };
        if state != "pass" {
            publishable = false;
        }
        checks.push((name.clone(), state.to_string(), v));
    }

    let journal = load_journal(git)?;
    let at_head = head_tree.as_deref() == Some(&*tree);

    if cli.json {
        println!(
            "{}",
            json!({
                "tree": tree,
                "branch": branch,
                "head": head,
                "tree_at_head": at_head,
                "publishable": publishable,
                "checks": checks.iter().map(|(name, state, v)| json!({
                    "check": name,
                    "state": state,
                    "finished": v.as_ref().map(|v| v.finished.clone()),
                })).collect::<Vec<_>>(),
                "pending_publish": journal.as_ref().map(|j| json!({
                    "tree": j.tree, "commit": j.new_commit,
                })),
            })
        );
    } else {
        println!("tree {}  branch {}", short(&tree), branch);
        for (name, state, _) in &checks {
            println!("  {name}: {state}");
        }
        if at_head {
            println!("  tree already committed at HEAD");
        }
        if let Some(j) = &journal {
            println!(
                "  pending publish (tree {}): rerun `greentree publish`",
                short(&j.tree)
            );
        }
        println!(
            "  publish: {}",
            if publishable {
                "would succeed"
            } else {
                "would refuse"
            }
        );
    }
    Ok(exit::OK)
}

fn gate(cli: &Cli, git: &Git, push: bool, message: Option<String>) -> crate::Result<u8> {
    let _lock = lock::acquire(&git.state_dir())?;
    let cfg = Config::effective(&git.root)?;
    let mut store = JsonStore::open(&git.state_dir())?;

    let mut results: Vec<(String, RunResult)> = Vec::new();
    let mut pre_tree: Option<String> = None;
    for (name, check) in cfg.required_checks() {
        let r = crate::runner::run_check_with(
            git, &cfg, name, check, &mut store, true, None, pre_tree,
        )?;
        pre_tree = Some(r.tree_after.clone());
        let outcome = r.verdict.outcome;
        results.push((name.clone(), r));
        if outcome != Outcome::Pass {
            let (name, r) = results.last().unwrap();
            if cli.json {
                println!(
                    "{}",
                    json!({
                        "gate": "refused",
                        "check": name,
                        "tree": r.verdict.tree,
                        "outcome": r.verdict.outcome.as_str(),
                        "log": r.verdict.log_path,
                        "log_tail": r.log_tail,
                    })
                );
            } else {
                println!("gate refused: {name} {}", mark(r.verdict.outcome));
                print_tail(&r.log_tail);
            }
            return Ok(match outcome {
                Outcome::Fail => exit::CHECK_FAILED,
                _ => exit::ERROR,
            });
        }
    }

    let report = publish(git, &cfg, &store, &PublishOptions { push, message })?;
    let statuses = maybe_post_statuses(git, &cfg, &report);
    if cli.json {
        println!(
            "{}",
            json!({
                "gate": "published",
                "checks": results.iter().map(|(name, r)| json!({
                    "check": name,
                    "tree": r.verdict.tree,
                    "outcome": r.verdict.outcome.as_str(),
                    "cached": r.cached,
                    "duration_ms": r.verdict.duration_ms,
                })).collect::<Vec<_>>(),
                "publish": publish_json(&report, &statuses),
            })
        );
    } else {
        for (name, r) in &results {
            println!("  {name} ✓{}", if r.cached { " (cached)" } else { "" });
        }
        emit_publish(cli, &report, &statuses);
    }
    Ok(exit::OK)
}

/// Post statuses for HEAD if its tree is verified. The half of the loop
/// that lets a NORMAL `git push` end attested: verify while working, push
/// with plain git, then attest (locally or from `serve`).
#[cfg(not(feature = "github"))]
fn attest(_cli: &Cli, _git: &Git) -> crate::Result<u8> {
    Err(Error::Config(
        "this greentree was built without the `github` feature; attest cannot post".into(),
    ))
}

#[cfg(feature = "github")]
fn attest(cli: &Cli, git: &Git) -> crate::Result<u8> {
    let _lock = lock::acquire(&git.state_dir())?;
    let cfg = Config::effective(&git.root)?;
    let store = JsonStore::open(&git.state_dir())?;
    let target = crate::publish::attest_target(git, &cfg, &store)?;
    let posted = crate::github::post_statuses(git, &target.commit, &target.tree, &target.checks)?;

    if cli.json {
        println!(
            "{}",
            json!({
                "commit": target.commit,
                "tree": target.tree,
                "checks": target.checks,
                "statuses_posted": posted,
            })
        );
    } else {
        println!(
            "attested commit {} (tree {}): {}",
            short(&target.commit),
            short(&target.tree),
            posted.join(", ")
        );
    }
    Ok(exit::OK)
}

/// Statuses outcome of a pushed publish, flattened into the publish JSON.
#[derive(Default, serde::Serialize)]
struct Statuses {
    #[serde(rename = "statuses_posted")]
    posted: Vec<String>,
    #[serde(rename = "statuses_error")]
    error: Option<String>,
}

fn publish_json(report: &crate::publish::PublishReport, statuses: &Statuses) -> serde_json::Value {
    let mut value = serde_json::to_value(report).expect("report serializes");
    if let Some(obj) = value.as_object_mut() {
        obj.insert("statuses_posted".into(), json!(statuses.posted));
        obj.insert("statuses_error".into(), json!(statuses.error));
    }
    value
}

fn print_tail(log_tail: &str) {
    for line in log_tail
        .lines()
        .rev()
        .take(15)
        .collect::<Vec<_>>()
        .iter()
        .rev()
    {
        println!("    {line}");
    }
}

/// Post `greentree/<check>` statuses after a pushed publish — best-effort:
/// skipped silently without a token or a github.com remote; a failed post
/// warns instead of failing the publish (rerun `publish --push` to re-post;
/// same context overwrites).
#[cfg(feature = "github")]
fn maybe_post_statuses(
    git: &Git,
    cfg: &Config,
    report: &crate::publish::PublishReport,
) -> Statuses {
    if !report.pushed {
        return Statuses::default();
    }
    let Some(commit) = &report.commit else {
        return Statuses::default();
    };
    if crate::github::token_from_env().is_none() {
        tracing::debug!("no GitHub token in env; skipping status posting");
        return Statuses::default();
    }
    match crate::github::remote_url(git) {
        Some(url) if crate::github::parse_github_remote(&url).is_some() => {}
        _ => return Statuses::default(),
    }
    // The gate always runs before publish (including resumes); post for the
    // checks it verified, or every required check when the report predates
    // this run's verification list.
    let checks: Vec<String> = if report.verified_by.is_empty() {
        cfg.required_checks()
            .into_iter()
            .map(|(n, _)| n.clone())
            .collect()
    } else {
        report.verified_by.clone()
    };
    match crate::github::post_statuses(git, commit, &report.tree, &checks) {
        Ok(posted) => Statuses {
            posted,
            error: None,
        },
        Err(e) => {
            tracing::warn!(error = %e, "status posting failed (publish itself succeeded)");
            Statuses {
                posted: Vec::new(),
                error: Some(e.to_string()),
            }
        }
    }
}

#[cfg(not(feature = "github"))]
fn maybe_post_statuses(
    _git: &Git,
    _cfg: &Config,
    _report: &crate::publish::PublishReport,
) -> Statuses {
    Statuses::default()
}

fn emit_publish(cli: &Cli, report: &crate::publish::PublishReport, statuses: &Statuses) {
    if cli.json {
        println!("{}", publish_json(report, statuses));
    } else if report.noop {
        println!(
            "tree {} already committed at HEAD{}",
            short(&report.tree),
            if report.pushed {
                "; pushed"
            } else {
                " — nothing to publish"
            }
        );
    } else {
        println!(
            "published commit {} from verified tree {} on {}{}",
            report.commit.as_deref().map(short).unwrap_or("?"),
            short(&report.tree),
            report.branch,
            if report.pushed { " (pushed)" } else { "" }
        );
    }
}

fn names(cfg: &Config) -> String {
    cfg.checks.keys().cloned().collect::<Vec<_>>().join(", ")
}

fn mark(o: Outcome) -> &'static str {
    match o {
        Outcome::Pass => "✓",
        Outcome::Fail => "✗",
        Outcome::Error => "error",
        Outcome::Timeout => "timeout",
        Outcome::Cancelled => "cancelled (tree changed mid-run)",
    }
}

fn worse(a: Outcome, b: Outcome) -> Outcome {
    use Outcome::*;
    let rank = |o: Outcome| match o {
        Pass => 0,
        Fail => 2,
        Error | Timeout | Cancelled => 1,
    };
    if rank(b) > rank(a) {
        b
    } else {
        a
    }
}
