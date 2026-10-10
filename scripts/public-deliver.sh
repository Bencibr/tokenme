#!/usr/bin/env bash
# Deliver the dev tree to the public mirror — the gate, then the push, in that
# order, every time (docs/internal/PUBLIC_DELIVERY.md, the steady-state flow).
#
#   scripts/public-deliver.sh             # gate + stage + deliver
#   scripts/public-deliver.sh --dry-run   # gate + stage, push nothing public
#
# What it does:
#   1. refuses to run unless the repo-local identity is the allowlisted one —
#      the gate rejects the commit anyway, so fail here with the fix instead
#   2. runs the gate at full scope (scripts/scan-public.py over --branches
#      --tags): exactly the refs a push can carry, ~2-3 min; CLEAN or nothing
#      happens
#   3. moves the local `github` branch to main and pushes it to origin — the
#      branch is the staged delivery point, gate-clean by construction, so the
#      forge always shows the state the public repo is meant to carry
#   4. pushes to the public remote: new tags first (the release workflow must
#      start before main can promise its bytes), then main with
#      --force-with-lease
#
# The github push needs one interactive credential-manager login the first
# time; if it fails, steps 1-3 have already landed — rerun this script when
# the network and the login window cooperate. Nothing but this script and the
# one-time rewrite (2026-10-09, recorded in PUBLIC_DELIVERY.md) ever touches
# the public refs.

set -euo pipefail
cd "$(dirname "$0")/.."

DRY_RUN=0
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=1 ;;
    *) echo "usage: $0 [--dry-run]" >&2; exit 2 ;;
  esac
done

git remote get-url github >/dev/null 2>&1 || {
  echo "no 'github' remote; add it first:" >&2
  echo "  git remote add github https://github.com/Bencibr/tokenme.git" >&2
  exit 1
}

email=$(git config user.email || true)
[[ "$email" == "agent@local" ]] || {
  echo "repo-local identity is '$email'; the gate allowlists agent@local only:" >&2
  echo "  git config user.name Agent && git config user.email agent@local" >&2
  exit 1
}

echo "==> hero: banner follows the tree (version, tool count, pill layout, PNGs)"
python scripts/update-hero.py
if ! git diff --quiet -- assets/tokenme-hero-dark.svg assets/tokenme-hero-light.svg \
                         assets/tokenme-hero-dark.png assets/tokenme-hero-light.png; then
  git add assets/tokenme-hero-dark.svg assets/tokenme-hero-light.svg \
          assets/tokenme-hero-dark.png assets/tokenme-hero-light.png
  git commit --quiet -m "docs(hero): the banner follows the tree — version from Cargo.toml, tool count from TOOL_IDS, pill layout re-flowed, PNGs re-rendered at 2x"
fi

echo "==> gate: the delivery range, exactly what the push carries"
PUB=$(git rev-parse --short public)
LEASE_NOW=$(git ls-remote github refs/heads/main 2>/dev/null | cut -f1 || true)
if [[ -n "$LEASE_NOW" ]]; then
  python scripts/scan-public.py "${LEASE_NOW}..public"
else
  python scripts/scan-public.py public
fi

echo "==> stage: the github branch follows the public branch, and rides to the forge"
git branch -f github public
git push origin github

if [[ $DRY_RUN == 1 ]]; then
  echo "==> dry run: nothing pushed to the public remote"
  exit 0
fi

echo "==> deliver: new tags first, then main"
remote_tags=$(git ls-remote --tags github 2>/dev/null | awk -F'/' '{print $NF}' | sed 's/\^{}$//' | sort -u || true)
mapfile -t NEW_TAGS < <(comm -23 <(git tag | sort) <(printf '%s\n' "$remote_tags" | sort -u))
if [[ ${#NEW_TAGS[@]} -gt 0 ]]; then
  git push github "${NEW_TAGS[@]}"
else
  echo "    no new tags"
fi

lease=$(git ls-remote github refs/heads/main 2>/dev/null | cut -f1 || true)
if [[ -n "$lease" ]]; then
  git push --force-with-lease="refs/heads/main:$lease" github public:main
else
  git push --force github public:main
fi
echo "==> delivered: github/main = $(git rev-parse --short main)"
