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
use crate::runner::{run_check, short, RunResult};
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
            emit_publish(cli, &report);
            Ok(exit::OK)
        }
        Command::Gate { push, message } => gate(cli, &git, *push, message.clone()),
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

    let mut results: Vec<(String, RunResult)> = Vec::new();
    for (name, check) in &selected {
        let r = run_check(git, &cfg, name, check, &mut store, !no_cache)?;
        results.push((name.clone(), r));
    }

    let worst = results
        .iter()
        .map(|(_, r)| r.verdict.outcome)
        .fold(Outcome::Pass, worse);

    if cli.json {
        println!(
            "{}",
            json!({
                "tree": results.first().map(|(_, r)| r.verdict.tree.clone()),
                "ok": worst == Outcome::Pass,
                "results": results.iter().map(|(name, r)| json!({
                    "check": name,
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
            if r.verdict.outcome != Outcome::Pass && !r.log_tail.is_empty() {
                for line in r
                    .log_tail
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
        }
    }

    Ok(match worst {
        Outcome::Pass => exit::OK,
        Outcome::Fail => exit::CHECK_FAILED,
        _ => exit::ERROR,
    })
}

fn status(cli: &Cli, git: &Git) -> crate::Result<u8> {
    let _lock = lock::acquire(&git.state_dir())?;
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
    for (name, check) in cfg.required_checks() {
        let r = run_check(git, &cfg, name, check, &mut store, true)?;
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
                        "outcome": r.verdict.outcome.as_str(),
                        "log": r.verdict.log_path,
                        "log_tail": r.log_tail,
                    })
                );
            } else {
                println!("gate refused: {name} {}", mark(r.verdict.outcome));
                for line in r
                    .log_tail
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
            return Ok(match outcome {
                Outcome::Fail => exit::CHECK_FAILED,
                _ => exit::ERROR,
            });
        }
    }

    let report = publish(git, &cfg, &store, &PublishOptions { push, message })?;
    if cli.json {
        println!(
            "{}",
            json!({
                "gate": "published",
                "checks": results.iter().map(|(name, r)| json!({
                    "check": name, "cached": r.cached,
                })).collect::<Vec<_>>(),
                "publish": serde_json::to_value(&report)?,
            })
        );
    } else {
        for (name, r) in &results {
            println!("  {name} ✓{}", if r.cached { " (cached)" } else { "" });
        }
        emit_publish(cli, &report);
    }
    Ok(exit::OK)
}

fn emit_publish(cli: &Cli, report: &crate::publish::PublishReport) {
    if cli.json {
        println!(
            "{}",
            serde_json::to_string(report).expect("report serializes")
        );
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
