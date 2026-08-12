#!/usr/bin/env bash
# The greentree primitive, demonstrated with nothing but git plumbing.
#
# An agent makes three attempts at a change. greentree's model:
#   1. hash the dirty working tree (content-addressed, no commit, no index touch)
#   2. cache the test verdict by tree hash
#   3. publish = create the commit FROM the exact tested tree object
#
# Run anywhere: it builds its own throwaway repo under $TMPDIR.
set -euo pipefail

say() { printf '\n\033[1m%s\033[0m\n' "$*"; }

demo=$(mktemp -d "${TMPDIR:-/tmp}/greentree-demo.XXXXXX")
trap 'rm -rf "$demo"' EXIT
cd "$demo"
git init -q -b main
git config user.name demo
git config user.email demo@example.invalid

# The project: a "test suite" that requires add() to exist in lib.txt.
cat > lib.txt <<'EOF'
mul
EOF
cat > check.sh <<'EOF'
grep -q '^add$' lib.txt
EOF
git add -A && git commit -qm "base"
base=$(git rev-parse HEAD)

# --- the primitive: hash the dirty working tree without touching the real index
shadow="$demo/.git/greentree-index"
tree_hash() {
  GIT_INDEX_FILE="$shadow" git add -A
  GIT_INDEX_FILE="$shadow" git write-tree
}
cp .git/index "$shadow"

# --- a verdict cache keyed by tree hash
declare -A verdict
run_check() {
  local tree=$1
  if [[ -n "${verdict[$tree]:-}" ]]; then
    echo "  tree ${tree:0:12}  CACHE HIT: ${verdict[$tree]}"
    return
  fi
  if bash check.sh; then verdict[$tree]=pass; else verdict[$tree]=fail; fi
  echo "  tree ${tree:0:12}  ran check: ${verdict[$tree]}"
}

say "Attempt 1: agent writes the wrong thing"
echo "sub" >> lib.txt
t1=$(tree_hash); run_check "$t1"

say "Attempt 2: agent fixes it"
sed -i.bak 's/^sub$/add/' lib.txt && rm lib.txt.bak
t2=$(tree_hash); run_check "$t2"

say "Attempt 3: agent tries something, then reverts it"
echo "extra" >> lib.txt
t3=$(tree_hash); run_check "$t3"
sed -i.bak '/^extra$/d' lib.txt && rm lib.txt.bak
t4=$(tree_hash)
echo "  reverted tree ${t4:0:12} == attempt-2 tree ${t2:0:12}: $([ "$t4" = "$t2" ] && echo yes)"
run_check "$t4"   # <- no test process runs; the verdict is content-addressed

say "Publish: commit is created FROM the verified tree object"
[[ "${verdict[$t4]}" == pass ]] || { echo "gate refuses: tree not verified"; exit 1; }
commit=$(git commit-tree "$t4" -p "$base" -m "verified: add() implemented")
git update-ref refs/heads/main "$commit" "$base"
git reset -q  # sync the real index to the new HEAD

echo "  published commit: $(git rev-parse --short HEAD)"
echo "  commit's tree:    $(git rev-parse HEAD^{tree} | cut -c1-12)"
echo "  verified tree:    ${t4:0:12}"
[[ $(git rev-parse "HEAD^{tree}") == "$t4" ]] && echo "  identical — the pushed commit needs no re-test"

say "History: 3 attempts, 1 test execution per unique tree, 1 commit"
git log --oneline
[[ -z $(git status --porcelain) ]] && echo "  working tree clean after publish"
