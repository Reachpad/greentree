//! Integration tests: the lib driven against real git repos in tempdirs,
//! plus exit-code contracts through the built binary.

use std::path::Path;
use std::process::Command;

use greentree::cache::{JsonStore, Outcome, VerdictStore};
use greentree::config::{Check, Config};
use greentree::git::Git;
use greentree::publish::{publish, PublishOptions, CHANGE_ID_TRAILER};
use greentree::runner::run_check;
use greentree::snapshot::snapshot;
use greentree::Error;
use indexmap::IndexMap;
use tempfile::TempDir;

fn sh(dir: &Path, script: &str) -> String {
    let out = Command::new("sh")
        .arg("-ec")
        .arg(script)
        .current_dir(dir)
        .output()
        .expect("spawn sh");
    assert!(
        out.status.success(),
        "script failed: {script}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn repo() -> (TempDir, Git) {
    let tmp = TempDir::new().expect("tempdir");
    sh(
        tmp.path(),
        "git init -q -b main && git config user.name t && git config user.email t@t \
         && echo base > base.txt && git add -A && git commit -qm base",
    );
    let git = Git::discover(tmp.path()).expect("discover");
    (tmp, git)
}

fn config(run: &str) -> Config {
    let mut checks = IndexMap::new();
    checks.insert(
        "test".to_string(),
        Check {
            run: run.to_string(),
            required_for_publish: true,
            fresh: None,
            timeout: None,
        },
    );
    Config {
        version: 1,
        checks,
        snapshot: Default::default(),
        inputs: Vec::new(),
    }
}

fn check_of(cfg: &Config) -> (&String, &Check) {
    cfg.checks.iter().next().unwrap()
}

#[test]
fn snapshot_is_deterministic_and_content_addressed() {
    let (tmp, git) = repo();
    let cfg = config("true");
    let t1 = snapshot(&git, &cfg).unwrap();
    let t2 = snapshot(&git, &cfg).unwrap();
    assert_eq!(t1, t2, "same content, same tree");

    sh(tmp.path(), "echo dirty > new-file.txt");
    let t3 = snapshot(&git, &cfg).unwrap();
    assert_ne!(t1, t3, "untracked file changes the tree");

    sh(tmp.path(), "rm new-file.txt");
    let t4 = snapshot(&git, &cfg).unwrap();
    assert_eq!(t1, t4, "revert returns the original tree hash");
}

#[test]
fn snapshot_respects_excludes() {
    let (tmp, git) = repo();
    let mut cfg = config("true");
    cfg.snapshot.exclude = vec!["scratch/**".into()];
    let t1 = snapshot(&git, &cfg).unwrap();
    sh(
        tmp.path(),
        "mkdir -p scratch && echo junk > scratch/out.log",
    );
    let t2 = snapshot(&git, &cfg).unwrap();
    assert_eq!(t1, t2, "excluded paths don't move the tree");
}

#[test]
fn snapshot_refuses_mid_merge() {
    let (tmp, git) = repo();
    sh(
        tmp.path(),
        "git checkout -qb other && echo a > c.txt && git add -A && git commit -qm a \
         && git checkout -q main && echo b > c.txt && git add -A && git commit -qm b \
         && git merge other > /dev/null 2>&1 || true",
    );
    let cfg = config("true");
    match snapshot(&git, &cfg) {
        Err(Error::Unsnapshotable(_)) => {}
        other => panic!("expected Unsnapshotable, got {other:?}"),
    }
}

#[test]
fn verdicts_cache_by_tree_and_revert_hits() {
    let (tmp, git) = repo();
    let cfg = config("test -f base.txt");
    let mut store = JsonStore::open(&git.state_dir()).unwrap();
    let (name, check) = check_of(&cfg);

    let r1 = run_check(&git, &cfg, name, check, &mut store, true).unwrap();
    assert_eq!(r1.verdict.outcome, Outcome::Pass);
    assert!(!r1.cached);

    let r2 = run_check(&git, &cfg, name, check, &mut store, true).unwrap();
    assert!(r2.cached, "same tree = cache hit");

    sh(tmp.path(), "echo x > extra.txt");
    let r3 = run_check(&git, &cfg, name, check, &mut store, true).unwrap();
    assert!(!r3.cached, "new tree = cache miss");

    sh(tmp.path(), "rm extra.txt");
    let r4 = run_check(&git, &cfg, name, check, &mut store, true).unwrap();
    assert!(r4.cached, "reverted tree = cache hit again");
}

#[test]
fn failing_check_is_cached_as_fail() {
    let (_tmp, git) = repo();
    let cfg = config("false");
    let mut store = JsonStore::open(&git.state_dir()).unwrap();
    let (name, check) = check_of(&cfg);
    let r = run_check(&git, &cfg, name, check, &mut store, true).unwrap();
    assert_eq!(r.verdict.outcome, Outcome::Fail);
    let r2 = run_check(&git, &cfg, name, check, &mut store, true).unwrap();
    assert!(r2.cached, "fail verdicts are cacheable too");
    assert_eq!(r2.verdict.outcome, Outcome::Fail);
}

#[test]
fn timeout_is_not_cached() {
    let (_tmp, git) = repo();
    let mut cfg = config("sleep 30");
    cfg.checks.get_mut("test").unwrap().timeout = Some("1s".into());
    let mut store = JsonStore::open(&git.state_dir()).unwrap();
    let (name, check) = ("test".to_string(), cfg.checks.get("test").unwrap().clone());
    let r = run_check(&git, &cfg, &name, &check, &mut store, true).unwrap();
    assert_eq!(r.verdict.outcome, Outcome::Timeout);
    let key = r.verdict.key();
    assert!(
        store.get(&key).is_none(),
        "timeout must not enter the cache"
    );
}

#[test]
fn tree_change_during_run_cancels_verdict() {
    let (_tmp, git) = repo();
    // The check itself mutates the repo: tree-after != tree-before.
    let cfg = config("echo mutated > written-by-check.txt");
    let mut store = JsonStore::open(&git.state_dir()).unwrap();
    let (name, check) = check_of(&cfg);
    let r = run_check(&git, &cfg, name, check, &mut store, true).unwrap();
    assert_eq!(r.verdict.outcome, Outcome::Cancelled);
    assert!(store.get(&r.verdict.key()).is_none());
}

#[test]
fn checks_do_not_see_greentree_git_env() {
    let (_tmp, git) = repo();
    // Fails if any GIT_* override leaks; also proves git-in-check sees the real repo.
    let cfg = config(
        "test -z \"$GIT_INDEX_FILE\" && test -z \"$GIT_DIR\" \
         && git status --porcelain > /dev/null && test -n \"$GREENTREE_TREE_SHA\"",
    );
    let mut store = JsonStore::open(&git.state_dir()).unwrap();
    let (name, check) = check_of(&cfg);
    let r = run_check(&git, &cfg, name, check, &mut store, true).unwrap();
    assert_eq!(r.verdict.outcome, Outcome::Pass);
}

#[test]
fn publish_gate_refuses_unverified_tree() {
    let (_tmp, git) = repo();
    let cfg = config("true");
    let store = JsonStore::open(&git.state_dir()).unwrap();
    match publish(&git, &cfg, &store, &PublishOptions::default()) {
        Err(Error::NotVerified { .. }) => {}
        other => panic!("expected NotVerified, got {other:?}"),
    }
}

#[test]
fn publish_creates_commit_from_exact_verified_tree() {
    let (tmp, git) = repo();
    let cfg = config("test -f feature.txt");
    let mut store = JsonStore::open(&git.state_dir()).unwrap();
    sh(tmp.path(), "echo done > feature.txt");
    let (name, check) = check_of(&cfg);
    let r = run_check(&git, &cfg, name, check, &mut store, true).unwrap();
    assert_eq!(r.verdict.outcome, Outcome::Pass);
    let verified_tree = r.verdict.tree.clone();

    let report = publish(&git, &cfg, &store, &PublishOptions::default()).unwrap();
    assert!(!report.noop);
    let commit = report.commit.unwrap();

    let head_tree = sh(tmp.path(), "git rev-parse 'HEAD^{tree}'");
    assert_eq!(
        head_tree, verified_tree,
        "published commit IS the verified tree"
    );
    assert_eq!(sh(tmp.path(), "git rev-parse HEAD"), commit);

    // Change-id trailer stamped (the stack seed).
    let body = sh(tmp.path(), "git log -1 --format=%B");
    assert!(body.contains(CHANGE_ID_TRAILER), "missing trailer: {body}");

    // Working tree reads clean after index sync.
    assert_eq!(sh(tmp.path(), "git status --porcelain"), "");
}

#[test]
fn publish_twice_is_noop() {
    let (tmp, git) = repo();
    let cfg = config("true");
    let mut store = JsonStore::open(&git.state_dir()).unwrap();
    sh(tmp.path(), "echo v > f.txt");
    let (name, check) = check_of(&cfg);
    run_check(&git, &cfg, name, check, &mut store, true).unwrap();
    let first = publish(&git, &cfg, &store, &PublishOptions::default()).unwrap();
    assert!(!first.noop);
    let second = publish(&git, &cfg, &store, &PublishOptions::default()).unwrap();
    assert!(second.noop, "same tree republished must be a no-op");
    assert_eq!(first.commit, second.commit);
}

#[test]
fn interrupted_publish_resumes_from_journal() {
    let (tmp, git) = repo();
    let cfg = config("true");
    let mut store = JsonStore::open(&git.state_dir()).unwrap();
    sh(tmp.path(), "echo v > f.txt");
    let (name, check) = check_of(&cfg);
    let r = run_check(&git, &cfg, name, check, &mut store, true).unwrap();
    let tree = r.verdict.tree.clone();

    // Simulate a publish that crashed after CAS but before index sync:
    // create the commit, move the ref, leave the journal behind.
    let parent = sh(tmp.path(), "git rev-parse HEAD");
    let commit = sh(
        tmp.path(),
        &format!("git commit-tree {tree} -p {parent} -m 'interrupted'"),
    );
    sh(
        tmp.path(),
        &format!("git update-ref refs/heads/main {commit} {parent}"),
    );
    let journal = serde_json::json!({
        "schema_version": 1,
        "tree": tree,
        "branch": "main",
        "parent": parent,
        "change_id": "deadbeefdeadbeefdeadbeefdeadbeef",
        "new_commit": commit,
        "lease": null,
    });
    std::fs::create_dir_all(git.state_dir()).unwrap();
    std::fs::write(
        git.state_dir().join("publish-journal.json"),
        serde_json::to_vec(&journal).unwrap(),
    )
    .unwrap();

    let report = publish(&git, &cfg, &store, &PublishOptions::default()).unwrap();
    assert!(report.resumed, "must take the resume path");
    assert_eq!(report.commit.as_deref(), Some(&*commit));
    assert_eq!(
        report.change_id.as_deref(),
        Some("deadbeefdeadbeefdeadbeefdeadbeef")
    );
    assert_eq!(
        sh(tmp.path(), "git status --porcelain"),
        "",
        "index synced on resume"
    );
    assert!(
        !git.state_dir().join("publish-journal.json").exists(),
        "journal cleared after completion"
    );
}

#[test]
fn changed_declared_input_invalidates_cache() {
    let (tmp, git) = repo();
    let mut cfg = config("true");
    cfg.inputs = vec![".env".into()];
    sh(tmp.path(), "echo A=1 > .env && echo '.env' > .gitignore");
    let mut store = JsonStore::open(&git.state_dir()).unwrap();
    let (name, check) = ("test".to_string(), cfg.checks.get("test").unwrap().clone());
    let r1 = run_check(&git, &cfg, &name, &check, &mut store, true).unwrap();
    assert!(!r1.cached);
    let r2 = run_check(&git, &cfg, &name, &check, &mut store, true).unwrap();
    assert!(r2.cached);
    sh(tmp.path(), "echo A=2 > .env");
    let r3 = run_check(&git, &cfg, &name, &check, &mut store, true).unwrap();
    assert!(
        !r3.cached,
        "ignored-but-declared input changed; the tree is the same but the env fingerprint is not"
    );
}

// ---- exit-code contract through the real binary ----

fn bin(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_greentree"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn greentree");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn exit_codes_are_the_documented_contract() {
    let (tmp, _git) = repo();
    sh(
        tmp.path(),
        "printf 'version: 1\\nchecks:\\n  test:\\n    run: \"test -f ok.txt\"\\n    required_for_publish: true\\n' > greentree.yaml",
    );

    let (code, _, _) = bin(tmp.path(), &["test"]);
    assert_eq!(code, 10, "check failed");

    let (code, _, _) = bin(tmp.path(), &["publish"]);
    assert_eq!(code, 11, "tree not verified");

    sh(tmp.path(), "touch ok.txt");
    let (code, stdout, _) = bin(tmp.path(), &["test", "--json"]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json output");
    assert_eq!(v["ok"], serde_json::Value::Bool(true));

    let (code, stdout, _) = bin(tmp.path(), &["gate", "--json", "-m", "msg"]);
    assert_eq!(code, 0, "gate publishes: {stdout}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(v["gate"], "published");
    assert_eq!(v["publish"]["pushed"], serde_json::Value::Bool(false));
}
