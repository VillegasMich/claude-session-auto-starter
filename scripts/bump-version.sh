#!/usr/bin/env bash
# Print or change the package version in Cargo.toml and Cargo.lock.
#
#   scripts/bump-version.sh                      print the current version
#   scripts/bump-version.sh auto                 bump from the Conventional Commits since the
#                                                v<version> tag (see below)
#   scripts/bump-version.sh patch|minor|major    bump it (patch of a pre-release, e.g. 1.3.0-rc.1,
#                                                gives its release, 1.3.0)
#   scripts/bump-version.sh <version>            set it, e.g. 1.2.0 or 1.3.0-rc.1
#
# auto: the biggest bump any commit (merges excluded) since v<version> asks for:
#   breaking (`type!:` or a `BREAKING CHANGE:` footer)  major (minor while still 0.x.y)
#   feat, chore                                          minor
#   anything else (fix, docs, ..., non-conventional)     patch
# The decision and each commit's bump are logged to stderr.
#
# Prints the resulting version. Only edits the files; committing is up to the caller (the
# release workflow, .github/workflows/release.yml, commits and pushes the bump to main).
#
# Requirements: cargo (to update Cargo.lock), git (for auto).
set -euo pipefail

readonly PACKAGE=claude-session-starter

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
readonly ROOT

die() { printf 'error: %s\n' "$*" >&2; exit 1; }

case ${1-} in
  -h | --help) sed -n '2,19p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
esac
(($# <= 1)) || die "expected at most one argument (try --help)"

cd "$ROOT"

# First `version = "..."` of the [package] table.
current=$(awk '
  /^\[/ { in_pkg = ($0 == "[package]") }
  in_pkg && /^version[[:space:]]*=/ { gsub(/.*=[[:space:]]*"|".*/, ""); print; exit }
' Cargo.toml)
[[ -n $current ]] || die "could not read the package version from Cargo.toml"

if (($# == 0)); then
  echo "$current"
  exit 0
fi

# Prints the bump (patch|minor|major) the commits since tag $1 call for.
auto_bump() {
  local tag=$1 header_re='^([a-z]+)(\([^)]*\))?(!)?: ' msg header level max=1 count=0
  local -a names=(none patch minor major)
  git rev-parse --quiet --verify "refs/tags/$tag" >/dev/null \
    || die "tag $tag not found; fetch tags ('git fetch --tags') or pass patch|minor|major"
  while IFS= read -r -d '' msg; do
    header=${msg%%$'\n'*}
    level=1
    if [[ $msg =~ (^|$'\n')BREAKING[\ -]CHANGE:\  ]]; then
      level=3
    elif [[ $header =~ $header_re ]]; then
      if [[ -n ${BASH_REMATCH[3]} ]]; then
        level=3
      elif [[ ${BASH_REMATCH[1]} == feat || ${BASH_REMATCH[1]} == chore ]]; then
        level=2
      fi
    fi
    printf '  %-5s  %s\n' "${names[level]}" "$header" >&2
    ((level > max)) && max=$level
    count=$((count + 1))
  done < <(git log -z --no-merges --format=%B "$tag..HEAD")
  ((count > 0)) || die "no commits since $tag; nothing to release"
  if ((max == 3)) && [[ $current == 0.* ]]; then
    printf '%s: breaking change while 0.x.y, so a minor bump\n' "$tag" >&2
    max=2
  fi
  printf '%s: %s bump from %d commit(s)\n' "$tag" "${names[max]}" "$count" >&2
  echo "${names[max]}"
}

semver='^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$'
case $1 in
  auto | patch | minor | major)
    [[ $current =~ $semver ]] || die "Cargo.toml version '$current' is not semver"
    bump=$1
    [[ $bump == auto ]] && bump=$(auto_bump "v$current")
    base=${current%%[-+]*}
    IFS=. read -r major minor patch <<<"$base"
    case $bump in
      patch) [[ $base != "$current" ]] && new=$base || new=$major.$minor.$((patch + 1)) ;;
      minor) new=$major.$((minor + 1)).0 ;;
      major) new=$((major + 1)).0.0 ;;
    esac
    ;;
  *)
    new=${1#v}
    [[ $new =~ $semver ]] || die "'$1' is not a semver version (e.g. 1.2.3 or 1.3.0-rc.1)"
    ;;
esac

if [[ $new != "$current" ]]; then
  # Rewrite only the [package] version line.
  awk -v new="$new" '
    /^\[/ { in_pkg = ($0 == "[package]") }
    in_pkg && !done && /^version[[:space:]]*=/ { print "version = \"" new "\""; done = 1; next }
    { print }
  ' Cargo.toml >Cargo.toml.tmp
  mv Cargo.toml.tmp Cargo.toml
  # Updates the workspace package's own entry; dependencies already in the lockfile are kept.
  cargo update --workspace --quiet
fi

lock_version=$(awk -v pkg="$PACKAGE" '
  $0 == "name = \"" pkg "\"" { found = 1; next }
  found && /^version/ { gsub(/.*=[[:space:]]*"|".*/, ""); print; exit }
' Cargo.lock)
[[ $lock_version == "$new" ]] || die "Cargo.lock has version '$lock_version' after the update, expected '$new'"

echo "$new"
