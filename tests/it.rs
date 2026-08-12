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
            watch: false,
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
fn snapshot_excludes_work_on_gitignored_paths() {
    // Excluding a path that is ALSO gitignored (target/ in a Rust repo)
    // must not fail the snapshot: `git add` exits 1 when a pathspec names
    // an ignored path.
    let (tmp, git) = repo();
    let mut cfg = config("true");
    cfg.snapshot.exclude = vec!["target".into()];
    sh(
        tmp.path(),
        "echo target > .gitignore && mkdir target && echo junk > target/out",
    );
    let t1 = snapshot(&git, &cfg).expect("snapshot with ignored exclude");
    sh(tmp.path(), "echo more >> target/out");
    let t2 = snapshot(&git, &cfg).unwrap();
    assert_eq!(t1, t2, "excluded+ignored churn must not move the tree");
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

#[test]
fn cancel_flag_kills_run_and_nothing_is_cached() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let (_tmp, git) = repo();
    let cfg = config("sleep 30");
    let mut store = JsonStore::open(&git.state_dir()).unwrap();
    let (name, check) = check_of(&cfg);

    let cancel = Arc::new(AtomicBool::new(false));
    let setter = {
        let cancel = Arc::clone(&cancel);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            cancel.store(true, Ordering::Relaxed);
        })
    };
    let started = std::time::Instant::now();
    let r = greentree::runner::run_check_with(
        &git,
        &cfg,
        name,
        check,
        &mut store,
        true,
        Some(&cancel),
        None,
    )
    .unwrap();
    setter.join().unwrap();

    assert_eq!(r.verdict.outcome, Outcome::Cancelled);
    assert!(store.get(&r.verdict.key()).is_none());
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "cancel must kill promptly, not wait out the sleep"
    );
}

#[test]
fn gc_prunes_anchors_and_trims_logs() {
    let (tmp, git) = repo();
    let cfg = config("echo some-log-output");
    let mut store = JsonStore::open(&git.state_dir()).unwrap();
    let (name, check) = check_of(&cfg);

    run_check(&git, &cfg, name, check, &mut store, true).unwrap();
    sh(tmp.path(), "echo two > second.txt");
    run_check(&git, &cfg, name, check, &mut store, true).unwrap();

    let refs = sh(
        tmp.path(),
        "git for-each-ref refs/greentree/snapshots/ | wc -l",
    );
    assert_eq!(refs, "2", "each tested tree gets one anchor");

    let report = greentree::gc::gc(
        &git,
        &greentree::gc::GcOptions {
            keep: 1,
            ttl: std::time::Duration::from_secs(3600),
            log_budget: 0,
        },
    )
    .unwrap();
    assert_eq!(report.snapshots_pruned, 1);
    assert_eq!(report.snapshots_kept, 1);
    assert!(report.logs_deleted >= 2, "zero budget deletes all logs");

    let refs = sh(
        tmp.path(),
        "git for-each-ref refs/greentree/snapshots/ | wc -l",
    );
    assert_eq!(refs, "1");
}

#[test]
fn watch_once_verifies_a_settling_tree() {
    let (tmp, git) = repo();
    sh(
        tmp.path(),
        "printf 'version: 1\\nchecks:\\n  test:\\n    run: \"test -f base.txt\"\\n    watch: true\\n' > greentree.yaml",
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_greentree"))
        .args(["watch", "--once", "--json"])
        .current_dir(tmp.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn watch");

    // Let the watcher install, then make an edit for it to see.
    std::thread::sleep(std::time::Duration::from_millis(700));
    sh(tmp.path(), "echo edit > watched.txt");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let status = loop {
        if let Some(s) = child.try_wait().expect("try_wait") {
            break s;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("watch --once did not complete a cycle within 15s");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    assert!(status.success());

    let mut stdout = String::new();
    use std::io::Read as _;
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    let v: serde_json::Value =
        serde_json::from_str(stdout.lines().last().expect("one cycle line")).unwrap();
    assert_eq!(v["results"][0]["outcome"], "pass", "cycle output: {stdout}");
    assert!(
        !git.state_dir().join("watch.pid").exists(),
        "pidfile removed on exit"
    );
}

#[test]
fn resume_path_still_requires_verification() {
    // A leftover journal must never bypass the gate: same setup as the
    // resume test, but the store holds NO verdict for the tree.
    let (tmp, git) = repo();
    let cfg = config("true");
    let store = JsonStore::open(&git.state_dir()).unwrap();
    sh(tmp.path(), "echo v > f.txt");
    let tree = snapshot(&git, &cfg).unwrap();

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
        "schema_version": 1, "tree": tree, "branch": "main", "parent": parent,
        "change_id": "deadbeefdeadbeefdeadbeefdeadbeef", "new_commit": commit, "lease": null,
    });
    std::fs::create_dir_all(git.state_dir()).unwrap();
    std::fs::write(
        git.state_dir().join("publish-journal.json"),
        serde_json::to_vec(&journal).unwrap(),
    )
    .unwrap();

    match publish(&git, &cfg, &store, &PublishOptions::default()) {
        Err(Error::NotVerified { .. }) => {}
        other => panic!("journal bypassed the gate: {other:?}"),
    }
}

#[test]
fn resume_on_wrong_branch_is_refused() {
    let (tmp, git) = repo();
    let cfg = config("true");
    let mut store = JsonStore::open(&git.state_dir()).unwrap();
    sh(tmp.path(), "echo v > f.txt");
    let (name, check) = check_of(&cfg);
    let r = run_check(&git, &cfg, name, check, &mut store, true).unwrap();
    let tree = r.verdict.tree.clone();

    let parent = sh(tmp.path(), "git rev-parse HEAD");
    let commit = sh(
        tmp.path(),
        &format!("git commit-tree {tree} -p {parent} -m x"),
    );
    sh(
        tmp.path(),
        &format!("git update-ref refs/heads/main {commit} {parent}"),
    );
    let journal = serde_json::json!({
        "schema_version": 1, "tree": tree, "branch": "main", "parent": parent,
        "change_id": "deadbeefdeadbeefdeadbeefdeadbeef", "new_commit": commit, "lease": null,
    });
    std::fs::write(
        git.state_dir().join("publish-journal.json"),
        serde_json::to_vec(&journal).unwrap(),
    )
    .unwrap();
    sh(tmp.path(), "git checkout -qb other && git reset -q");

    match publish(&git, &cfg, &store, &PublishOptions::default()) {
        Err(Error::Publish(msg)) => assert!(msg.contains("branch"), "wrong error: {msg}"),
        other => panic!("resume crossed branches: {other:?}"),
    }
}

#[test]
fn corrupt_journal_fails_loudly() {
    let (tmp, git) = repo();
    let cfg = config("true");
    let mut store = JsonStore::open(&git.state_dir()).unwrap();
    sh(tmp.path(), "echo v > f.txt");
    let (name, check) = check_of(&cfg);
    run_check(&git, &cfg, name, check, &mut store, true).unwrap();
    std::fs::write(git.state_dir().join("publish-journal.json"), b"{garbage").unwrap();
    match publish(&git, &cfg, &store, &PublishOptions::default()) {
        Err(Error::Publish(msg)) => assert!(msg.contains("parse"), "wrong error: {msg}"),
        other => panic!("corrupt journal was swallowed: {other:?}"),
    }
}

#[test]
fn backgrounded_grandchild_does_not_hang_the_run() {
    let (_tmp, git) = repo();
    let cfg = config("sleep 20 & exit 0");
    let mut store = JsonStore::open(&git.state_dir()).unwrap();
    let (name, check) = check_of(&cfg);
    let started = std::time::Instant::now();
    let r = run_check(&git, &cfg, name, check, &mut store, true).unwrap();
    assert_eq!(r.verdict.outcome, Outcome::Pass);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "drain join blocked on the backgrounded process's pipe: {:?}",
        started.elapsed()
    );
}

#[test]
fn push_lease_works_with_non_origin_remote() {
    let (tmp, git) = repo();
    let remote_dir = TempDir::new().unwrap();
    sh(remote_dir.path(), "git init -q --bare -b main");
    sh(
        tmp.path(),
        &format!("git remote add upstream {}", remote_dir.path().display()),
    );

    let cfg = config("true");
    let mut store = JsonStore::open(&git.state_dir()).unwrap();
    sh(tmp.path(), "echo one > f.txt");
    let (name, check) = check_of(&cfg);
    run_check(&git, &cfg, name, check, &mut store, true).unwrap();
    let first = publish(
        &git,
        &cfg,
        &store,
        &PublishOptions {
            push: true,
            message: None,
        },
    )
    .unwrap();
    assert!(first.pushed);

    // Second publish: the remote-tracking ref now exists — the lease must
    // be read for the remote we push to, not a hardcoded `origin`.
    sh(tmp.path(), "echo two > f.txt");
    run_check(&git, &cfg, name, check, &mut store, true).unwrap();
    let second = publish(
        &git,
        &cfg,
        &store,
        &PublishOptions {
            push: true,
            message: None,
        },
    )
    .unwrap();
    assert!(second.pushed);
    let remote_head = sh(remote_dir.path(), "git rev-parse main");
    assert_eq!(Some(remote_head), second.commit);
}

#[test]
fn colliding_check_names_get_distinct_logs() {
    let (_tmp, git) = repo();
    let mut checks = IndexMap::new();
    for (name, run) in [("a.b", "echo FROM-DOT; false"), ("a b", "echo FROM-SPACE")] {
        checks.insert(
            name.to_string(),
            Check {
                run: run.to_string(),
                required_for_publish: false,
                fresh: None,
                timeout: None,
                watch: false,
            },
        );
    }
    let cfg = Config {
        version: 1,
        checks,
        snapshot: Default::default(),
        inputs: Vec::new(),
    };
    let mut store = JsonStore::open(&git.state_dir()).unwrap();
    let dot = run_check(&git, &cfg, "a.b", &cfg.checks["a.b"], &mut store, true).unwrap();
    let space = run_check(&git, &cfg, "a b", &cfg.checks["a b"], &mut store, true).unwrap();
    assert_ne!(
        dot.verdict.log_path, space.verdict.log_path,
        "sanitized names collided onto one log file"
    );
    assert!(dot.log_tail.contains("FROM-DOT"), "{}", dot.log_tail);
    assert!(space.log_tail.contains("FROM-SPACE"), "{}", space.log_tail);
}

#[test]
fn directory_inputs_are_fingerprinted() {
    let (tmp, git) = repo();
    let mut cfg = config("true");
    cfg.inputs = vec!["conf".into()];
    sh(
        tmp.path(),
        "mkdir conf && echo A=1 > conf/x.env && echo conf/ > .gitignore",
    );
    let mut store = JsonStore::open(&git.state_dir()).unwrap();
    let (name, check) = ("test".to_string(), cfg.checks.get("test").unwrap().clone());
    let r1 = run_check(&git, &cfg, &name, &check, &mut store, true).unwrap();
    assert!(
        r1.verdict.env_inputs.keys().any(|k| k.contains("x.env")),
        "directory input contributed nothing: {:?}",
        r1.verdict.env_inputs
    );
    let r2 = run_check(&git, &cfg, &name, &check, &mut store, true).unwrap();
    assert!(r2.cached);
    sh(tmp.path(), "echo A=2 > conf/x.env");
    let r3 = run_check(&git, &cfg, &name, &check, &mut store, true).unwrap();
    assert!(!r3.cached, "edit inside a directory input must invalidate");
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
