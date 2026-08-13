//! Integration tests: the lib driven against real git repos in tempdirs,
//! plus exit-code contracts through the built binary.

use std::path::Path;
use std::process::Command;

use greentree::cache::{JsonStore, Outcome, VerdictStore};
use greentree::config::{Check, Config, DiskSize};
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

/// The shared config for tests that are not ABOUT the disk floor. The floor
/// is explicitly disabled: leaving it unset would make every one of these
/// tests silently require the built-in 5 GiB default of free space, and fail
/// with a refusal that has nothing to do with what they assert.
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
            min_free_disk: None,
        },
    );
    Config {
        version: 1,
        checks,
        snapshot: Default::default(),
        inputs: Vec::new(),
        min_free_disk: Some(DiskSize(0)),
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

/// Leave the repo mid-merge with `c.txt` conflicted. Returns (ours, theirs).
fn conflicted_merge(dir: &Path) -> (String, String) {
    sh(
        dir,
        "git checkout -qb other && echo a > c.txt && git add -A && git commit -qm a \
         && git checkout -q main && echo b > c.txt && git add -A && git commit -qm b \
         && git merge other > /dev/null 2>&1 || true",
    );
    (
        sh(dir, "git rev-parse HEAD"),
        sh(dir, "git rev-parse other"),
    )
}

#[test]
fn snapshot_refuses_conflicts_but_not_a_resolved_merge() {
    // The honest-tree rule: only the conflicted index is unsnapshotable. Once
    // the conflict is resolved the tree is ordinary and must hash like any
    // other — being mid-merge is publish's problem, not snapshot's.
    let (tmp, git) = repo();
    conflicted_merge(tmp.path());
    let cfg = config("true");
    match snapshot(&git, &cfg) {
        Err(Error::Unsnapshotable(_)) => {}
        other => panic!("expected Unsnapshotable, got {other:?}"),
    }

    sh(tmp.path(), "echo resolved > c.txt && git add c.txt");
    let tree = snapshot(&git, &cfg).expect("a resolved merge snapshots");
    assert!(
        tmp.path().join(".git/MERGE_HEAD").exists(),
        "still mid-merge — that is the point"
    );
    assert_eq!(
        tree,
        snapshot(&git, &cfg).unwrap(),
        "and caches like any tree"
    );
}

#[test]
fn resolved_merge_publishes_a_two_parent_commit() {
    let (tmp, git) = repo();
    let (ours, theirs) = conflicted_merge(tmp.path());
    let cfg = config("grep -q resolved c.txt");
    let mut store = JsonStore::open(&git.state_dir()).unwrap();
    let (name, check) = check_of(&cfg);

    sh(tmp.path(), "echo resolved > c.txt && git add c.txt");
    let r = run_check(&git, &cfg, name, check, &mut store, true).unwrap();
    assert_eq!(r.verdict.outcome, Outcome::Pass);
    let verified_tree = r.verdict.tree.clone();
    assert!(
        greentree::publish::merge_in_progress(&git).unwrap(),
        "a real merge must read as in progress"
    );

    let report = publish(&git, &cfg, &store, &PublishOptions::default()).unwrap();
    assert!(!report.noop);
    let commit = report.commit.unwrap();

    assert_eq!(
        sh(tmp.path(), "git log -1 --format=%P"),
        format!("{ours} {theirs}"),
        "a merge commit, with HEAD and MERGE_HEAD as its parents"
    );
    assert_eq!(
        sh(tmp.path(), "git rev-parse 'HEAD^{tree}'"),
        verified_tree,
        "published commit IS the verified tree"
    );
    assert_eq!(sh(tmp.path(), "git rev-parse HEAD"), commit);
    // No -m: the message defaults to MERGE_MSG, minus its comment lines.
    assert_eq!(
        sh(tmp.path(), "git log -1 --format=%s"),
        "Merge branch 'other'"
    );
    assert!(sh(tmp.path(), "git log -1 --format=%B").contains(CHANGE_ID_TRAILER));

    for state in ["MERGE_HEAD", "MERGE_MSG", "MERGE_MODE", "AUTO_MERGE"] {
        assert!(
            !tmp.path().join(".git").join(state).exists(),
            "{state} survived the merge commit"
        );
    }
    assert_eq!(
        sh(tmp.path(), "git status --porcelain"),
        "",
        "the merge is finished, not merely committed"
    );
}

#[test]
fn octopus_merge_publishes_every_parent() {
    let (tmp, git) = repo();
    sh(
        tmp.path(),
        "git checkout -qb a && echo a > a.txt && git add -A && git commit -qm a \
         && git checkout -q main && git checkout -qb b && echo b > b.txt \
         && git add -A && git commit -qm b && git checkout -q main \
         && echo m > m.txt && git add -A && git commit -qm m \
         && git merge --no-commit a b > /dev/null 2>&1 || true",
    );
    let heads: Vec<String> = ["HEAD", "a", "b"]
        .iter()
        .map(|r| sh(tmp.path(), &format!("git rev-parse {r}")))
        .collect();
    assert_eq!(
        std::fs::read_to_string(tmp.path().join(".git/MERGE_HEAD"))
            .unwrap()
            .lines()
            .count(),
        2,
        "octopus MERGE_HEAD lists one SHA per line"
    );

    let cfg = config("test -f a.txt && test -f b.txt");
    let mut store = JsonStore::open(&git.state_dir()).unwrap();
    let (name, check) = check_of(&cfg);
    assert_eq!(
        run_check(&git, &cfg, name, check, &mut store, true)
            .unwrap()
            .verdict
            .outcome,
        Outcome::Pass
    );

    publish(&git, &cfg, &store, &PublishOptions::default()).unwrap();
    assert_eq!(
        sh(tmp.path(), "git log -1 --format=%P"),
        heads.join(" "),
        "three parents: HEAD then both MERGE_HEAD lines"
    );
    assert!(!tmp.path().join(".git/MERGE_HEAD").exists());
}

#[test]
fn resolved_cherry_pick_publishes_a_single_parent_commit() {
    let (tmp, git) = repo();
    sh(
        tmp.path(),
        "git checkout -qb other && echo a > c.txt && git add -A \
         && git commit -qm 'the picked commit' && git checkout -q main \
         && echo b > c.txt && git add -A && git commit -qm b \
         && git cherry-pick other > /dev/null 2>&1 || true",
    );
    assert!(tmp.path().join(".git/CHERRY_PICK_HEAD").exists());
    let head = sh(tmp.path(), "git rev-parse HEAD");

    let cfg = config("grep -q resolved c.txt");
    let mut store = JsonStore::open(&git.state_dir()).unwrap();
    let (name, check) = check_of(&cfg);
    sh(tmp.path(), "echo resolved > c.txt && git add c.txt");
    let r = run_check(&git, &cfg, name, check, &mut store, true).unwrap();
    assert_eq!(r.verdict.outcome, Outcome::Pass);

    publish(&git, &cfg, &store, &PublishOptions::default()).unwrap();
    assert_eq!(
        sh(tmp.path(), "git log -1 --format=%P"),
        head,
        "a cherry-pick is an ordinary single-parent commit"
    );
    assert_eq!(
        sh(tmp.path(), "git log -1 --format=%s"),
        "the picked commit",
        "the picked commit's message carries over from MERGE_MSG"
    );
    assert!(
        !tmp.path().join(".git/CHERRY_PICK_HEAD").exists(),
        "CHERRY_PICK_HEAD survived the commit"
    );
    assert_eq!(sh(tmp.path(), "git status --porcelain"), "");
}

#[test]
fn squash_merge_publishes_one_commit_with_the_squash_message() {
    // `git merge --squash` leaves SQUASH_MSG + AUTO_MERGE and NO MERGE_HEAD:
    // one ordinary commit, whose message git takes from SQUASH_MSG.
    let (tmp, git) = repo();
    sh(
        tmp.path(),
        "git checkout -qb other && echo a > a.txt && git add -A \
         && git commit -qm 'the squashed commit' && git checkout -q main \
         && echo m > m.txt && git add -A && git commit -qm m \
         && git merge --squash other > /dev/null",
    );
    let head = sh(tmp.path(), "git rev-parse HEAD");
    assert!(tmp.path().join(".git/SQUASH_MSG").exists());
    assert!(
        !tmp.path().join(".git/MERGE_HEAD").exists(),
        "a squash merge records no second parent"
    );

    let cfg = config("test -f a.txt");
    let mut store = JsonStore::open(&git.state_dir()).unwrap();
    let (name, check) = check_of(&cfg);
    let r = run_check(&git, &cfg, name, check, &mut store, true).unwrap();
    assert_eq!(r.verdict.outcome, Outcome::Pass);

    publish(&git, &cfg, &store, &PublishOptions::default()).unwrap();
    assert_eq!(
        sh(tmp.path(), "git log -1 --format=%P"),
        head,
        "a squash is a single-parent commit"
    );
    assert_eq!(
        sh(tmp.path(), "git log -1 --format=%s"),
        "Squashed commit of the following:",
        "the squash message must not be thrown away"
    );
    assert!(sh(tmp.path(), "git log -1 --format=%B").contains("the squashed commit"));
    for state in ["SQUASH_MSG", "MERGE_MSG", "AUTO_MERGE"] {
        assert!(
            !tmp.path().join(".git").join(state).exists(),
            "{state} survived the squash commit"
        );
    }
    assert_eq!(sh(tmp.path(), "git status --porcelain"), "");
}

#[test]
fn a_stale_merge_head_never_mints_a_second_merge_commit() {
    // A MERGE_HEAD left behind by a crashed cleanup names a commit HEAD
    // already contains. Publishing must read that as "no merge in progress",
    // not as a merge to redo — and must retire the stale file.
    let (tmp, git) = repo();
    let cfg = config("test -f feature.txt");
    let mut store = JsonStore::open(&git.state_dir()).unwrap();
    let head = sh(tmp.path(), "git rev-parse HEAD");
    std::fs::write(tmp.path().join(".git/MERGE_HEAD"), format!("{head}\n")).unwrap();
    std::fs::write(
        tmp.path().join(".git/MERGE_MSG"),
        "Merge branch 'already-merged'\n",
    )
    .unwrap();

    assert!(
        !greentree::publish::merge_in_progress(&git).unwrap(),
        "a MERGE_HEAD HEAD already contains is not a merge in progress"
    );

    sh(tmp.path(), "echo done > feature.txt");
    let (name, check) = check_of(&cfg);
    let r = run_check(&git, &cfg, name, check, &mut store, true).unwrap();
    assert_eq!(r.verdict.outcome, Outcome::Pass);

    let report = publish(&git, &cfg, &store, &PublishOptions::default()).unwrap();
    assert!(!report.noop);
    assert_eq!(
        sh(tmp.path(), "git log -1 --format=%P"),
        head,
        "the stale MERGE_HEAD became a second parent"
    );
    assert!(
        sh(tmp.path(), "git log -1 --format=%s").starts_with("greentree: verified tree"),
        "a stale merge's message must not be inherited either"
    );
    assert!(
        !tmp.path().join(".git/MERGE_HEAD").exists(),
        "stale MERGE_HEAD survived the publish"
    );
    assert_eq!(sh(tmp.path(), "git status --porcelain"), "");
}

#[test]
fn comment_char_auto_keeps_a_message_whose_lines_start_with_hash() {
    // `core.commentChar = auto` picks the first character no line of the
    // message starts with; stripping `#` regardless would eat this subject.
    let (tmp, git) = repo();
    sh(
        tmp.path(),
        "git config core.commentChar auto \
         && git checkout -qb other && echo a > c.txt && git add -A \
         && git commit -qm '#42: the picked commit' && git checkout -q main \
         && echo b > c.txt && git add -A && git commit -qm b \
         && git cherry-pick other > /dev/null 2>&1 || true",
    );
    assert!(tmp.path().join(".git/CHERRY_PICK_HEAD").exists());

    let cfg = config("grep -q resolved c.txt");
    let mut store = JsonStore::open(&git.state_dir()).unwrap();
    let (name, check) = check_of(&cfg);
    sh(tmp.path(), "echo resolved > c.txt && git add c.txt");
    run_check(&git, &cfg, name, check, &mut store, true).unwrap();

    publish(&git, &cfg, &store, &PublishOptions::default()).unwrap();
    assert_eq!(
        sh(tmp.path(), "git log -1 --format=%s"),
        "#42: the picked commit",
        "the subject was stripped as a comment"
    );
}

#[test]
fn rebase_tests_but_refuses_to_publish() {
    let (tmp, git) = repo();
    sh(
        tmp.path(),
        "git checkout -qb feature && echo f > c.txt && git add -A && git commit -qm f \
         && git checkout -q main && echo m > c.txt && git add -A && git commit -qm m \
         && git checkout -q feature && git rebase main > /dev/null 2>&1 || true",
    );
    sh(tmp.path(), "echo resolved > c.txt && git add c.txt");
    assert!(
        tmp.path().join(".git/rebase-merge").exists()
            || tmp.path().join(".git/rebase-apply").exists(),
        "expected a rebase in progress"
    );

    // Testing is unaffected: the tree is honest, and the verdict is cached
    // for the tree the rebase will end up producing.
    let cfg = config("grep -q resolved c.txt");
    let mut store = JsonStore::open(&git.state_dir()).unwrap();
    let (name, check) = check_of(&cfg);
    let r = run_check(&git, &cfg, name, check, &mut store, true).unwrap();
    assert_eq!(r.verdict.outcome, Outcome::Pass);

    // Publishing is not: the next commit belongs to the rebase's sequencer.
    match publish(&git, &cfg, &store, &PublishOptions::default()) {
        Err(Error::Unpublishable(msg)) => {
            assert!(msg.contains("rebase --continue"), "unhelpful: {msg}")
        }
        other => panic!("expected Unpublishable, got {other:?}"),
    }

    // …and `gate` refuses with the same exit code, before running anything.
    sh(
        tmp.path(),
        "printf 'version: 1\\nmin_free_disk: \"0\"\\nchecks:\\n  test:\\n    run: \"true\"\\n' \
         > greentree.yaml",
    );
    let (code, _, stderr) = bin(tmp.path(), &["gate"]);
    assert_eq!(code, 12, "gate during a rebase: {stderr}");
    assert!(stderr.contains("rebase"), "unhelpful: {stderr}");

    // …and `status` says so instead of promising a publish that would refuse.
    let (code, stdout, _) = bin(tmp.path(), &["status", "--json"]);
    assert_eq!(code, 0, "status keeps working: {stdout}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(v["publishable"], serde_json::Value::Bool(false));
    assert!(
        v["publish_blocked"]
            .as_str()
            .unwrap_or("")
            .contains("rebase"),
        "status hid the rebase: {stdout}"
    );
    // A rebase detaches HEAD: an empty string reads like a branch we failed
    // to print, so the contract is null.
    assert_eq!(
        v["branch"],
        serde_json::Value::Null,
        "detached HEAD must report a null branch: {stdout}"
    );
}

#[test]
fn interrupted_merge_publish_replays_the_full_parent_list() {
    // Crash between commit-tree and update-ref: the journal is the only
    // record of the merge's parents, and the retry must rebuild the SAME
    // commit rather than a single-parent one.
    let (tmp, git) = repo();
    let (ours, theirs) = conflicted_merge(tmp.path());
    let cfg = config("grep -q resolved c.txt");
    let mut store = JsonStore::open(&git.state_dir()).unwrap();
    let (name, check) = check_of(&cfg);
    sh(tmp.path(), "echo resolved > c.txt && git add c.txt");
    let tree = run_check(&git, &cfg, name, check, &mut store, true)
        .unwrap()
        .verdict
        .tree;

    let commit = sh(
        tmp.path(),
        &format!("git commit-tree {tree} -p {ours} -p {theirs} -m 'interrupted merge'"),
    );
    let journal = serde_json::json!({
        "schema_version": 2, "tree": tree, "branch": "main",
        "parents": [ours, theirs],
        "change_id": "deadbeefdeadbeefdeadbeefdeadbeef", "new_commit": commit, "lease": null,
    });
    std::fs::write(
        git.state_dir().join("publish-journal.json"),
        serde_json::to_vec(&journal).unwrap(),
    )
    .unwrap();

    let report = publish(&git, &cfg, &store, &PublishOptions::default()).unwrap();
    assert_eq!(
        report.commit.as_deref(),
        Some(&*commit),
        "the journaled commit was reused, not re-minted"
    );
    assert_eq!(
        report.change_id.as_deref(),
        Some("deadbeefdeadbeefdeadbeefdeadbeef")
    );
    assert_eq!(
        sh(tmp.path(), "git log -1 --format=%P"),
        format!("{ours} {theirs}")
    );
    assert!(!tmp.path().join(".git/MERGE_HEAD").exists());
    assert!(!git.state_dir().join("publish-journal.json").exists());
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
    // Deliberately the schema-1 shape: a journal left in flight by an older
    // greentree must still be readable, its single `parent` becoming the
    // one-element parent list.
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
    assert_eq!(
        greentree::publish::load_journal(&git)
            .unwrap()
            .unwrap()
            .parents,
        vec![parent.clone()],
        "schema-1 journals migrate to the parent list"
    );

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
fn housekeeping_that_fails_after_the_commit_warns_instead_of_failing() {
    // A stale `index.lock` (a crashed git, or one still running) makes the
    // post-commit index sync impossible. The commit exists and the branch
    // already moved, so reporting failure would invite a retry of a publish
    // that already happened: it is a warning on a successful report.
    let (tmp, git) = repo();
    let cfg = config("true");
    let mut store = JsonStore::open(&git.state_dir()).unwrap();
    sh(tmp.path(), "echo v > f.txt");
    let (name, check) = check_of(&cfg);
    run_check(&git, &cfg, name, check, &mut store, true).unwrap();

    let lock = tmp.path().join(".git/index.lock");
    std::fs::write(&lock, b"").unwrap();
    let report = publish(&git, &cfg, &store, &PublishOptions::default()).unwrap();
    std::fs::remove_file(&lock).unwrap();

    let commit = report.commit.clone().expect("the commit was still created");
    assert_eq!(sh(tmp.path(), "git rev-parse HEAD"), commit);
    assert!(
        report.warnings.iter().any(|w| w.contains("index sync")),
        "the failed sync went unreported: {:?}",
        report.warnings
    );
    assert!(
        !git.state_dir().join("publish-journal.json").exists(),
        "the publish completed, so its journal is spent"
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
        "printf 'version: 1\\nmin_free_disk: \"0\"\\nchecks:\\n  test:\\n    run: \"test -f base.txt\"\\n    watch: true\\n' > greentree.yaml",
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
fn watch_survives_a_disk_floor_refusal() {
    // A pre-start disk refusal must not be fatal to a long-lived watcher:
    // it is reported like any other cycle outcome and the next edit is still
    // picked up. (One-shot `test`/`gate` still exit 16 — see
    // `a_floor_no_disk_can_meet_refuses_the_run`.)
    let (tmp, git) = repo();
    sh(
        tmp.path(),
        "printf 'version: 1\\nmin_free_disk: \"100T\"\\nchecks:\\n  test:\\n    run: \"true\"\\n    watch: true\\n' > greentree.yaml",
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_greentree"))
        .args(["watch", "--json"])
        .current_dir(tmp.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn watch");
    let stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead as _;
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            let _ = tx.send(line);
        }
    });

    let wait = std::time::Duration::from_secs(20);
    std::thread::sleep(std::time::Duration::from_millis(700));
    sh(tmp.path(), "echo edit > watched.txt");
    let first = rx.recv_timeout(wait).expect("no refusal line from watch");
    let v: serde_json::Value = serde_json::from_str(&first).expect("json cycle line");
    assert_eq!(
        v["results"][0]["outcome"], "disk_exhausted",
        "refusal line: {first}"
    );
    assert!(
        v["error"].as_str().unwrap_or("").contains("min_free_disk"),
        "refusal line names no reason: {first}"
    );

    // Still watching: a later edit is still verified (here, refused again).
    sh(tmp.path(), "echo again > watched.txt");
    let second = rx
        .recv_timeout(wait)
        .expect("watch stopped watching after a refusal");
    assert!(second.contains("disk_exhausted"), "second line: {second}");
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "watch exited on a disk refusal instead of continuing"
    );

    let _ = child.kill();
    let _ = child.wait();
    let _ = git;
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
                min_free_disk: None,
            },
        );
    }
    let cfg = Config {
        version: 1,
        checks,
        snapshot: Default::default(),
        inputs: Vec::new(),
        min_free_disk: Some(DiskSize(0)),
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

#[test]
fn attest_refuses_dirty_and_stamps_committed_state() {
    let (tmp, git) = repo();
    let cfg = config("test -f feature.txt");
    let mut store = JsonStore::open(&git.state_dir()).unwrap();

    // The normal-git flow: edit, verify while dirty, commit with plain git.
    sh(tmp.path(), "echo done > feature.txt");
    let (name, check) = check_of(&cfg);
    let r = run_check(&git, &cfg, name, check, &mut store, true).unwrap();
    assert_eq!(r.verdict.outcome, Outcome::Pass);

    // Dirty tree: attest must refuse (HEAD's tree is not the verified one).
    match greentree::publish::attest_target(&git, &cfg, &store) {
        Err(Error::NotVerified { reason, .. }) => {
            assert!(reason.contains("HEAD"), "reason: {reason}")
        }
        other => panic!("attest on dirty tree: {other:?}"),
    }

    // Plain git commit of the same content: attest now targets HEAD with
    // the verdict recorded while the tree was dirty.
    sh(
        tmp.path(),
        "git add -A && git commit -qm 'plain git commit'",
    );
    let target = greentree::publish::attest_target(&git, &cfg, &store).unwrap();
    assert_eq!(target.tree, r.verdict.tree);
    assert_eq!(target.checks, vec!["test".to_string()]);
    assert_eq!(target.commit, sh(tmp.path(), "git rev-parse HEAD"));
}

#[test]
fn verdict_log_appends_and_gc_compacts() {
    // put() must append (O(1) write, not full rewrite), survive reopen, and
    // gc must collapse repeated writes of the same key to one line.
    let (tmp, git) = repo();
    let cfg = config("true");
    let (name, check) = check_of(&cfg);

    for _ in 0..5 {
        let mut store = JsonStore::open(&git.state_dir()).unwrap();
        run_check(&git, &cfg, name, check, &mut store, false).unwrap(); // no_cache: re-run each time
    }
    let log = git.state_dir().join("verdicts.jsonl");
    let before = std::fs::read_to_string(&log).unwrap();
    assert_eq!(before.lines().count(), 5, "each put appends one line");

    // The verdict is still readable after all those reopens.
    let store = JsonStore::open(&git.state_dir()).unwrap();
    let tree = snapshot(&git, &cfg).unwrap();
    let key = greentree::cache::VerdictKey {
        tree,
        check: name.to_string(),
        check_hash: check.hash(),
        env_fingerprint: greentree::config::env_fingerprint(&git.root, &cfg.inputs)
            .unwrap()
            .0,
    };
    assert!(store.get(&key).is_some());

    greentree::gc::gc(&git, &greentree::gc::GcOptions::default()).unwrap();
    let after = std::fs::read_to_string(&log).unwrap();
    assert_eq!(after.lines().count(), 1, "gc compacts to one line per key");
    let _ = tmp;
}

#[test]
fn check_cannot_read_the_github_token() {
    // The check subprocess must never see greentree's credentials, so that
    // running verification can never leak the token that attests it.
    let (_tmp, git) = repo();
    let cfg = config("test -z \"$GITHUB_TOKEN\" && test -z \"$GREENTREE_GITHUB_TOKEN\"");
    let mut store = JsonStore::open(&git.state_dir()).unwrap();
    std::env::set_var("GITHUB_TOKEN", "ghp_scrub_me");
    std::env::set_var("GREENTREE_GITHUB_TOKEN", "ghp_scrub_me_too");
    let (name, check) = check_of(&cfg);
    let r = run_check(&git, &cfg, name, check, &mut store, true).unwrap();
    std::env::remove_var("GITHUB_TOKEN");
    std::env::remove_var("GREENTREE_GITHUB_TOKEN");
    assert_eq!(
        r.verdict.outcome,
        Outcome::Pass,
        "token leaked into the check environment"
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
fn a_floor_no_disk_can_meet_refuses_the_run() {
    // No box has 100T free, so the floor fires deterministically; setting it
    // to "0" is the documented escape hatch and must let the same check run.
    let (tmp, _git) = repo();
    let cfg = |floor: &str| {
        format!(
            "printf 'version: 1\\nmin_free_disk: \"{floor}\"\\nchecks:\\n  \
             test:\\n    run: \"true\"\\n' > greentree.yaml"
        )
    };

    sh(tmp.path(), &cfg("100T"));
    let (code, _, stderr) = bin(tmp.path(), &["test"]);
    assert_eq!(code, 16, "disk floor exit code; stderr: {stderr}");
    for needle in ["test", "100", "min_free_disk"] {
        assert!(
            stderr.contains(needle),
            "refusal must name {needle:?}: {stderr}"
        );
    }

    let (code, stdout, _) = bin(tmp.path(), &["test", "--json"]);
    assert_eq!(code, 16);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json error object");
    assert_eq!(v["exit_code"], 16);
    assert!(
        v["error"].as_str().unwrap().contains("min_free_disk"),
        "json error names the config key: {stdout}"
    );

    sh(tmp.path(), &cfg("0"));
    let (code, stdout, stderr) = bin(tmp.path(), &["test"]);
    assert_eq!(code, 0, "a zero floor disables it: {stdout}{stderr}");
}

#[test]
fn exit_codes_are_the_documented_contract() {
    let (tmp, _git) = repo();
    sh(
        tmp.path(),
        "printf 'version: 1\\nmin_free_disk: \"0\"\\nchecks:\\n  test:\\n    run: \"test -f ok.txt\"\\n    required_for_publish: true\\n' > greentree.yaml",
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
