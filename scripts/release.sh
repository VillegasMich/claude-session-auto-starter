#!/usr/bin/env bash
# Create a GitHub release for the version in Cargo.toml.
#
#   scripts/release.sh [--dry-run] [-y|--yes]
#
#   --dry-run  run all checks and print what would be released, without creating anything.
#   -y, --yes  don't ask for confirmation.
#
# Tags the current commit of main as v<version> and creates the release with --generate-notes.
# Publishing the release triggers CI, which pushes the Docker image (docs/repository-setup.md).
# For a release made with the workflow's GITHUB_TOKEN no event fires, so release.yml dispatches CI
# itself.
# Versions with a pre-release suffix (e.g. 1.2.0-rc.1) are published as pre-releases.
#
# Requirements: git, gh (logged in). Must run on an up-to-date, clean main branch. Fails if the
# version was already released: bump `version` in Cargo.toml (and Cargo.lock) first.
set -euo pipefail

readonly BRANCH=main
readonly PACKAGE=claude-session-starter

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
readonly ROOT

log() { printf '\033[1m==>\033[0m %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "'$1' is required but not installed${2:+ ($2)}"; }

dry_run=false
yes=false
for arg in "$@"; do
  case $arg in
    --dry-run) dry_run=true ;;
    -y | --yes) yes=true ;;
    -h | --help) sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown argument '$arg' (try --help)" ;;
  esac
done

need git
need gh "https://cli.github.com"
cd "$ROOT"
# Rather than `gh auth status`, which needs a user token: also works with the GITHUB_TOKEN of the
# release workflow (.github/workflows/release.yml).
gh repo view --json name >/dev/null 2>&1 \
  || die "gh can't access this repository; run 'gh auth login' (or set GH_TOKEN)"

# --- Version ---------------------------------------------------------------------------------
# First `version = "..."` of the [package] table.
version=$(awk '
  /^\[/ { in_pkg = ($0 == "[package]") }
  in_pkg && /^version[[:space:]]*=/ { gsub(/.*=[[:space:]]*"|".*/, ""); print; exit }
' Cargo.toml)
[[ -n $version ]] || die "could not read the package version from Cargo.toml"
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$ ]] \
  || die "Cargo.toml version '$version' is not semver"
readonly tag=v$version

# CI builds with --locked, so Cargo.lock must already carry the new version.
lock_version=$(awk -v pkg="$PACKAGE" '
  $0 == "name = \"" pkg "\"" { found = 1; next }
  found && /^version/ { gsub(/.*=[[:space:]]*"|".*/, ""); print; exit }
' Cargo.lock)
[[ $lock_version == "$version" ]] \
  || die "Cargo.lock has version '$lock_version', Cargo.toml has '$version'; run 'cargo build' and commit Cargo.lock"

# --- Branch state ----------------------------------------------------------------------------
current=$(git symbolic-ref --quiet --short HEAD || true)
[[ $current == "$BRANCH" ]] || die "releases are made from '$BRANCH' (current: '${current:-detached HEAD}')"
[[ -z $(git status --porcelain) ]] || die "working tree is not clean; commit or discard your changes"

log "Fetching origin"
git fetch --quiet --tags origin "$BRANCH"
head=$(git rev-parse HEAD)
[[ $head == "$(git rev-parse "origin/$BRANCH")" ]] \
  || die "local '$BRANCH' differs from 'origin/$BRANCH'; pull or push first"

# --- Already released? -----------------------------------------------------------------------
bump_hint="bump 'version' in Cargo.toml (e.g. $version -> next patch/minor/major), run 'cargo build' to update Cargo.lock, commit, push and retry"
if git rev-parse --quiet --verify "refs/tags/$tag" >/dev/null \
  || [[ -n $(git ls-remote --tags origin "refs/tags/$tag") ]]; then
  die "tag $tag already exists; $bump_hint"
fi
if gh release view "$tag" >/dev/null 2>&1; then
  die "release $tag already exists; $bump_hint"
fi

# --- Release ---------------------------------------------------------------------------------
args=(release create "$tag" --target "$head" --title "$tag" --generate-notes)
kind=release
if [[ $version == *-* ]]; then
  args+=(--prerelease)
  kind=pre-release
fi

log "Version:  $version ($kind)"
log "Tag:      $tag -> $(git log -1 --format='%h %s' "$head")"
if $dry_run; then
  log "Dry run, would run: gh ${args[*]}"
  exit 0
fi
if ! $yes; then
  read -r -p "Create $kind $tag? [y/N] " answer
  [[ $answer =~ ^[Yy]$ ]] || die "aborted"
fi

gh "${args[@]}"
log "Released $tag; CI will now publish the Docker image"
