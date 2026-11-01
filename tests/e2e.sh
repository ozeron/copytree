#!/usr/bin/env bash
# End-to-end test against real package managers.
#
#   tests/e2e.sh <work-dir>
#
# Builds sample npm, pnpm and uv repos inside <work-dir>, then exercises
# `copytree new`, `list`, `rm`, hook mode and rollback. Run it once with a
# work dir on a copy-on-write volume (APFS, btrfs, XFS reflink=1) and once on
# one without (ext4) to cover both clone paths. Needs network the first time
# so npm, pnpm and uv can fill their caches.
set -euo pipefail

ROOT="$(cd "$1" && pwd)/ct-e2e-$$"
BIN="${COPYTREE_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/debug/copytree}"
mkdir -p "$ROOT"
trap 'rm -rf "$ROOT"' EXIT
pass() { printf '  \033[32mok\033[0m  %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; exit 1; }
ct() { "$BIN" -q "$@"; }

init_repo() {
  mkdir -p "$1" && cd "$1"
  git init -q -b main
  git config user.email t@example.com && git config user.name test
  printf 'node_modules/\n.env\n.turbo/\n.venv/\n.mypy_cache/\n__pycache__/\n' > .gitignore
}

echo "== npm repo"
init_repo "$ROOT/npm-app"
cat > package.json <<'EOF'
{ "name": "npm-app", "version": "1.0.0", "private": true, "dependencies": { "is-number": "7.0.0" } }
EOF
npm install --silent --no-audit --no-fund >/dev/null
echo 'SECRET=hunter2-not-real' > .env
mkdir -p .turbo && echo cache > .turbo/blob
git add -A && git commit -qm init

P=$(ct new feat-a --print-path)
[ -d "$P" ] || fail "workspace dir missing"
[ "$(git -C "$P" branch --show-current)" = ct/feat-a ] && pass "branch ct/feat-a"
[ -f "$P/node_modules/is-number/package.json" ] && pass "node_modules present" || fail "node_modules missing"
[ "$(cat "$P/.env")" = "SECRET=hunter2-not-real" ] && pass ".env carried" || fail ".env not carried"
[ "$(cat "$P/.turbo/blob")" = cache ] && pass ".turbo seeded" || fail ".turbo not seeded"
ct list | grep -q 'feat-a.*ready' && pass "list shows ready" || fail "list"
ct list --json | python3 -c 'import json,sys; assert json.load(sys.stdin)[0]["status"]=="ready"' && pass "list --json"

echo "dirty" >> "$P/package.json"
echo "new" > "$P/untracked.txt"
ct rm feat-a 2>/dev/null && fail "rm should refuse untracked files" || pass "rm refuses untracked files"
rm "$P/untracked.txt"
ct rm feat-a --wait >/dev/null
[ ! -e "$P" ] && pass "workspace removed" || fail "still there"
git show refs/copytree/feat-a:package.json | grep -q dirty && pass "uncommitted edit saved in refs/copytree/feat-a"
git worktree list | grep -q feat-a && fail "git still lists worktree" || pass "git worktree metadata pruned"

echo "== rollback on failure"
printf '[hooks]\npost_create = ["exit 3"]\n' > .copytree.toml
ct new broken 2>/dev/null && fail "should fail" || true
[ ! -e "$(dirname "$P")/broken" ] && ! git rev-parse -q --verify ct/broken >/dev/null && pass "failed workspace rolled back"
rm .copytree.toml

echo "== hook mode (plain git worktree add)"
ct install-hook >/dev/null
git worktree add -q -b other "$ROOT/npm-other" 2>/dev/null
[ -f "$ROOT/npm-other/node_modules/is-number/package.json" ] && pass "hook installed deps" || fail "hook deps"
[ -f "$ROOT/npm-other/.env" ] && pass "hook carried .env" || fail "hook carry"
ct list | grep -q 'npm-other.*ready' && pass "hook worktree listed as ready"
git checkout -q -b scratch && git checkout -q main && pass "ordinary checkout untouched"
ct uninstall-hook >/dev/null
git worktree remove --force "$ROOT/npm-other"
ct list --json | grep -q npm-other && fail "stale state kept" || pass "state cleaned after git worktree remove"

echo "== pnpm repo"
init_repo "$ROOT/pnpm-app"
cat > package.json <<'EOF'
{ "name": "pnpm-app", "version": "1.0.0", "private": true, "dependencies": { "is-number": "7.0.0" } }
EOF
pnpm install --silent >/dev/null
git add -A && git commit -qm init
P=$(ct new p1 --print-path)
node -e "require('$P/node_modules/is-number')" && pass "pnpm deps resolve in workspace"

echo "== uv repo (editable install must point at the workspace)"
init_repo "$ROOT/uv-app"
mkdir -p src/uvapp && echo 'WHO = "main"' > src/uvapp/__init__.py
cat > pyproject.toml <<'EOF'
[project]
name = "uvapp"
version = "0.1.0"
requires-python = ">=3.9"
dependencies = ["six"]
[build-system]
requires = ["hatchling"]
build-backend = "hatchling.build"
EOF
uv sync -q 2>/dev/null
git add -A && git commit -qm init
P=$(ct new u1 --print-path)
echo 'WHO = "workspace"' > "$P/src/uvapp/__init__.py"
[ "$("$P/.venv/bin/python" -c 'import uvapp; print(uvapp.WHO)')" = workspace ] \
  && pass "workspace venv imports the workspace's own source" || fail "venv points at main"
"$P/.venv/bin/python" -c 'import six' && pass "uv deps installed"
ct rm u1 --force --wait >/dev/null && pass "rm uv workspace"

echo "all e2e checks passed"
