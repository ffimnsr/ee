#!/usr/bin/env bash

set -euo pipefail

readonly RELEASE_MANIFEST="crates/ee-cli/Cargo.toml"
readonly REMOTE_NAME="origin"
readonly CHANGELOG_FILE="CHANGELOG.md"

# Publish order for the release package.
readonly PUBLISH_ORDER=(
  ee-cli
)

usage() {
  cat <<'EOF'
Usage: scripts/release.sh [options] [<version>]

Bump all workspace crate versions, refresh Cargo.lock, run release quality gates,
create release commit, create v-prefixed git tag, and optionally push commit/tag.

Release version source of truth: crates/ee-cli/Cargo.toml
Updated manifests: all workspace package Cargo.toml files and workspace dependencies in Cargo.toml

Options:
  --major      Increment major version and reset minor/patch to zero.
  --minor      Increment minor version and reset patch to zero.
  --patch      Increment patch version.
  --skip-push  Skip pushing release commit and tag to origin.
  --publish    Publish ee-cli to crates.io.
  -h, --help   Show this help message.

Examples:
  scripts/release.sh --patch
  scripts/release.sh --minor --skip-push
  scripts/release.sh 0.2.0
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

append_section_entry() {
  local section_name="$1"
  local entry="$2"

  printf -v "$section_name" '%s- %s\n' "${!section_name}" "$entry"
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

manifest_version() {
  local manifest="$1"

  awk '
    BEGIN { in_package = 0 }
    /^\[package\]$/ { in_package = 1; next }
    /^\[/ && $0 != "[package]" && in_package { in_package = 0 }
    in_package && /^version = "/ {
      gsub(/^version = "/, "", $0)
      gsub(/"$/, "", $0)
      print
      exit
    }
  ' "$manifest"
}

ensure_clean_worktree() {
  git diff --quiet --exit-code || die "working tree has unstaged changes"
  git diff --cached --quiet --exit-code || die "index has staged but uncommitted changes"
}

previous_release_tag() {
  git describe --tags --abbrev=0 --match 'v[0-9]*.[0-9]*.[0-9]*' 2>/dev/null || true
}

render_changelog_group() {
  local title="$1"
  local content="$2"

  [[ -n "$content" ]] || return 0

  printf '### %s\n\n' "$title"
  printf '%s\n' "$content"
  printf '\n'
}

update_changelog() {
  local version="$1"
  local release_date="$2"
  local previous_tag="$3"
  local log_range
  local features=""
  local fixes=""
  local docs=""
  local tests=""
  local ci=""
  local maintenance=""
  local other=""
  local entry_count=0
  local conventional_commit_regex='^([[:alnum:]_-]+)(\([^)]+\))?(!)?:[[:space:]]*(.+)$'

  if [[ -n "$previous_tag" ]]; then
    log_range="${previous_tag}..HEAD"
  else
    log_range="HEAD"
  fi

  while IFS=$'\t' read -r commit_sha subject; do
    local short_sha category message commit_type
    [[ -n "$commit_sha" ]] || continue

    short_sha="$(git rev-parse --short "$commit_sha")"
    category="other"
    message="$subject"

    if [[ "$subject" =~ $conventional_commit_regex ]]; then
      commit_type="${BASH_REMATCH[1]}"
      message="${BASH_REMATCH[4]}"

      case "$commit_type" in
        feat)
          category="features"
          ;;
        fix)
          category="fixes"
          ;;
        docs)
          category="docs"
          ;;
        test)
          category="tests"
          ;;
        ci)
          category="ci"
          ;;
        build|style|refactor|perf|chore)
          category="maintenance"
          ;;
      esac
    fi

    case "$category" in
      features)
        append_section_entry features "${message} (\`${short_sha}\`)"
        ;;
      fixes)
        append_section_entry fixes "${message} (\`${short_sha}\`)"
        ;;
      docs)
        append_section_entry docs "${message} (\`${short_sha}\`)"
        ;;
      tests)
        append_section_entry tests "${message} (\`${short_sha}\`)"
        ;;
      ci)
        append_section_entry ci "${message} (\`${short_sha}\`)"
        ;;
      maintenance)
        append_section_entry maintenance "${message} (\`${short_sha}\`)"
        ;;
      *)
        append_section_entry other "${message} (\`${short_sha}\`)"
        ;;
    esac

    ((entry_count += 1))
  done < <(git log --reverse --format='%H%x09%s' "$log_range")

  ((entry_count > 0)) || die "no commits found for changelog range: ${log_range}"

  if [[ -f "$CHANGELOG_FILE" ]] && grep -Eq "^## ${version//./\\.}([[:space:]]|$)" "$CHANGELOG_FILE"; then
    die "$CHANGELOG_FILE already contains an entry for version $version"
  fi

  local tmp preface existing_releases
  tmp="$(mktemp)"
  preface=""
  existing_releases=""

  if [[ -f "$CHANGELOG_FILE" ]]; then
    preface="$(awk '
      BEGIN { seen_release = 0 }
      /^## [0-9]+\.[0-9]+\.[0-9]+([[:space:]]-|$)/ {
        seen_release = 1
        exit
      }
      { print }
    ' "$CHANGELOG_FILE")"

    existing_releases="$(awk '
      BEGIN { seen_release = 0 }
      /^## [0-9]+\.[0-9]+\.[0-9]+([[:space:]]-|$)/ {
        seen_release = 1
      }
      seen_release { print }
    ' "$CHANGELOG_FILE")"
  fi

  {
    if [[ -n "$preface" ]]; then
      printf '%s\n' "$preface"
      [[ "$preface" == *$'\n' ]] || printf '\n'
    else
      printf '# Changelog\n\n'
    fi

    printf '## %s - %s\n\n' "$version" "$release_date"
    render_changelog_group "Features" "$features"
    render_changelog_group "Fixes" "$fixes"
    render_changelog_group "Documentation" "$docs"
    render_changelog_group "Tests" "$tests"
    render_changelog_group "CI" "$ci"
    render_changelog_group "Maintenance" "$maintenance"
    render_changelog_group "Other Changes" "$other"

    if [[ -n "$existing_releases" ]]; then
      printf '%s\n' "$existing_releases"
    fi
  } >"$tmp"

  mv "$tmp" "$CHANGELOG_FILE"
}

current_version() {
  manifest_version "$RELEASE_MANIFEST"
}

increment_version() {
  local current="$1"
  local bump_kind="$2"
  local major minor patch

  IFS='.' read -r major minor patch <<<"$current"

  case "$bump_kind" in
    major)
      ((major += 1))
      minor=0
      patch=0
      ;;
    minor)
      ((minor += 1))
      patch=0
      ;;
    patch)
      ((patch += 1))
      ;;
    *)
      die "unsupported bump kind: $bump_kind"
      ;;
  esac

  printf '%s.%s.%s\n' "$major" "$minor" "$patch"
}

update_manifest_version() {
  local manifest="$1"
  local version="$2"
  local tmp

  tmp="$(mktemp)"

  awk -v version="$version" '
    BEGIN { in_package = 0; replaced = 0 }
    /^\[package\]$/ { in_package = 1 }
    /^\[/ && $0 != "[package]" && in_package { in_package = 0 }
    in_package && /^version = "/ && !replaced {
      print "version = \"" version "\""
      replaced = 1
      next
    }
    { print }
    END {
      if (!replaced) {
        exit 1
      }
    }
  ' "$manifest" >"$tmp" || {
    rm -f "$tmp"
    die "failed to update version in $manifest"
  }

  mv "$tmp" "$manifest"
}

workspace_manifests() {
  awk '
    BEGIN { in_workspace = 0; in_members = 0 }
    /^\[workspace\]$/ { in_workspace = 1; next }
    in_workspace && /^\[/ && $0 != "[workspace]" { exit }
    in_workspace {
      if ($0 ~ /^[[:space:]]*members[[:space:]]*=/) {
        line = $0
        sub(/^[^=]*=[[:space:]]*/, "", line)
        gsub(/[][]|"/, "", line)
        n = split(line, parts, /,[[:space:]]*/)
        for (i = 1; i <= n; i++) if (length(parts[i])) print parts[i]
        in_members = 1
      } else if (in_members) {
        line = $0
        gsub(/[][]|"/, "", line)
        n = split(line, parts, /,[[:space:]]*/)
        for (i = 1; i <= n; i++) if (length(parts[i])) print parts[i]
        if ($0 ~ /\]/) in_members = 0
      }
    }
  ' Cargo.toml
}

update_workspace_manifest_versions() {
  local version="$1"
  local member

  for member in $(workspace_manifests); do
    local manifest="${member%/}/Cargo.toml"
    if [[ -f "$manifest" ]]; then
      update_manifest_version "$manifest" "$version"
    fi
  done
}

update_root_workspace_dependency_versions() {
  local version="$1"
  local tmp

  tmp="$(mktemp)"

  awk -v version="$version" '
    BEGIN { in_table = 0 }
    /^\[workspace\.dependencies\]$/ { in_table = 1 }
    /^\[/ && $0 != "[workspace.dependencies]" && in_table { in_table = 0 }
    {
      if (in_table && /path[[:space:]]*=.*"crates\// && /version[[:space:]]*=/) {
        sub(/version[[:space:]]*=[[:space:]]*"[^"]*"/, "version = \"" version "\"")
      }
      print
    }
  ' Cargo.toml >"$tmp" || {
    rm -f "$tmp"
    die "failed to update workspace dependency versions in Cargo.toml"
  }

  mv "$tmp" Cargo.toml
}

stage_workspace_manifests() {
  local member manifest

  for member in $(workspace_manifests); do
    manifest="${member%/}/Cargo.toml"
    if [[ -f "$manifest" ]]; then
      git add "$manifest"
    fi
  done
}

ensure_remote_exists() {
  git remote get-url "$REMOTE_NAME" >/dev/null 2>&1 || die "git remote '$REMOTE_NAME' is not configured"
}

publish_workspace() {
  local pkg

  for pkg in "${PUBLISH_ORDER[@]}"; do
    printf 'Publishing %s...\n' "$pkg"
    cargo publish -p "$pkg"
  done
}

ensure_tag_absent() {
  local tag_name="$1"

  git rev-parse --verify "refs/tags/$tag_name" >/dev/null 2>&1 &&
    die "tag '$tag_name' already exists locally"

  if git remote get-url "$REMOTE_NAME" >/dev/null 2>&1; then
    git ls-remote --exit-code --tags "$REMOTE_NAME" "refs/tags/$tag_name" >/dev/null 2>&1 &&
      die "tag '$tag_name' already exists on '$REMOTE_NAME'" || true
  fi
}

main() {
  local run_push=1
  local run_publish=0
  local version=""
  local bump_kind=""

  while (($# > 0)); do
    case "$1" in
      --major|--minor|--patch)
        [[ -z "$bump_kind" ]] || die "only one of --major, --minor, or --patch may be used"
        bump_kind="${1#--}"
        shift
        ;;
      --skip-push)
        run_push=0
        shift
        ;;
      --publish)
        run_publish=1
        shift
        ;;
      -h|--help)
        usage
        exit 0
        ;;
      -*)
        die "unknown option: $1"
        ;;
      *)
        [[ -z "$version" ]] || die "version may only be provided once"
        version="$1"
        shift
        ;;
    esac
  done

  if [[ -z "$version" && -z "$bump_kind" ]]; then
    usage
    exit 1
  fi

  [[ -z "$version" || -z "$bump_kind" ]] || die "pass either an explicit version or one bump flag"

  need_cmd awk
  need_cmd cargo
  need_cmd date
  need_cmd git
  need_cmd mktemp
  need_cmd bash

  local repo_root
  repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" || die "must be run inside a git repository"
  cd "$repo_root"

  ensure_clean_worktree

  local old_version
  old_version="$(current_version)"
  [[ -n "$old_version" ]] || die "failed to read current release version"
  [[ "$old_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "current version must match x.y.z"

  if [[ -n "$bump_kind" ]]; then
    version="$(increment_version "$old_version" "$bump_kind")"
  fi

  [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "version must match x.y.z"
  [[ "$old_version" != "$version" ]] || die "version is already $version"

  local tag_name="v$version"
  local previous_tag
  local release_date

  if (( run_push )); then
    ensure_remote_exists
  fi

  ensure_tag_absent "$tag_name"

  previous_tag="$(previous_release_tag)"
  release_date="$(date +%Y-%m-%d)"

  update_workspace_manifest_versions "$version"
  update_root_workspace_dependency_versions "$version"
  update_changelog "$version" "$release_date" "$previous_tag"

  bash scripts/build-runtime.sh --force --source-root target/release-runtime-sources --output-root target/release-runtime

  cargo check --workspace --all-targets --quiet
  cargo fmt --all --check
  cargo clippy --workspace --all-targets --all-features -- -D warnings
  cargo test --workspace

  git add Cargo.lock Cargo.toml "$CHANGELOG_FILE"
  stage_workspace_manifests
  git commit -m "release: $tag_name"

  git tag -a "$tag_name" -m "release: $tag_name"

  if (( run_push )); then
    git push "$REMOTE_NAME" HEAD
    git push "$REMOTE_NAME" "$tag_name"
  fi

  if (( run_publish )); then
    publish_workspace
  fi

  printf 'Released %s -> %s (%s)\n' "$old_version" "$version" "$tag_name"
}

main "$@"