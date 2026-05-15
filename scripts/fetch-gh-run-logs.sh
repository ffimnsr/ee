#!/usr/bin/env bash

set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/fetch-gh-run-logs.sh [options] <run-id>

Download GitHub Actions job logs for one workflow run via `gh api`.

Options:
  --repo <owner/repo>  Repository to query. Defaults to current gh repo.
  --out-dir <dir>      Output directory. Defaults to gh-run-<run-id>-logs.
  --job <job-id>       Restrict download to one job id. Repeatable.
  --failed-only        Download only failed jobs.
  -h, --help           Show this help.

Examples:
  scripts/fetch-gh-run-logs.sh 25656075149
  scripts/fetch-gh-run-logs.sh --failed-only 25656075149
  scripts/fetch-gh-run-logs.sh --repo ffimnsr/ee --job 75304774332 25656075149
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

slugify() {
  printf '%s' "$1" | tr '[:upper:]' '[:lower:]' | sed -E 's/[^a-z0-9._-]+/-/g; s/^-+//; s/-+$//'
}

resolve_repo() {
  local current_repo

  current_repo="$(gh repo view --json nameWithOwner --jq .nameWithOwner 2>/dev/null || true)"
  [[ -n "$current_repo" ]] || die "unable to resolve repository; pass --repo <owner/repo>"
  printf '%s\n' "$current_repo"
}

repo=""
out_dir=""
run_id=""
failed_only=false
declare -a requested_jobs=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --repo)
      [[ $# -ge 2 ]] || die "--repo requires value"
      repo="$2"
      shift 2
      ;;
    --out-dir)
      [[ $# -ge 2 ]] || die "--out-dir requires value"
      out_dir="$2"
      shift 2
      ;;
    --job)
      [[ $# -ge 2 ]] || die "--job requires value"
      requested_jobs+=("$2")
      shift 2
      ;;
    --failed-only)
      failed_only=true
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    --)
      shift
      break
      ;;
    -*)
      die "unknown option: $1"
      ;;
    *)
      if [[ -z "$run_id" ]]; then
        run_id="$1"
        shift
      else
        die "unexpected argument: $1"
      fi
      ;;
  esac
done

if [[ -z "$run_id" && $# -gt 0 ]]; then
  run_id="$1"
  shift
fi

[[ $# -eq 0 ]] || die "unexpected extra arguments: $*"
[[ -n "$run_id" ]] || die "missing required <run-id>"

need_cmd gh
need_cmd python3

if [[ -z "$repo" ]]; then
  repo="$(resolve_repo)"
fi

if [[ -z "$out_dir" ]]; then
  out_dir="gh-run-${run_id}-logs"
fi

mkdir -p "$out_dir"

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

run_json_file="$tmp_dir/run.json"
jobs_json_file="$tmp_dir/jobs.json"
selection_file="$tmp_dir/selection.tsv"

gh api -H 'Accept: application/vnd.github+json' "repos/$repo/actions/runs/$run_id" >"$run_json_file"
gh api -H 'Accept: application/vnd.github+json' --paginate "repos/$repo/actions/runs/$run_id/jobs?per_page=100" \
  --jq '.jobs[]' | python3 -c 'import sys,json; jobs=[json.loads(l) for l in sys.stdin]; print(json.dumps({"jobs":jobs}))' >"$jobs_json_file"

python3 - "$run_json_file" "$jobs_json_file" "$failed_only" "$selection_file" "${requested_jobs[@]+"${requested_jobs[@]}"}" <<'PY'
import json
import re
import sys

run = json.load(open(sys.argv[1], encoding="utf-8"))
jobs_payload = json.load(open(sys.argv[2], encoding="utf-8"))
failed_only = sys.argv[3] == "true"
selection_path = sys.argv[4]
requested = set(sys.argv[5:])

jobs = jobs_payload.get("jobs", [])

def job_id_of(job: dict) -> str:
    raw = job.get("id", job.get("databaseId"))
    if raw is None:
        raise KeyError("id")
    return str(raw)

known_ids = {job_id_of(job) for job in jobs}
missing = sorted(requested - known_ids)
if missing:
    print("missing job ids: " + ", ".join(missing), file=sys.stderr)
    sys.exit(2)

def slugify(value: str) -> str:
    slug = re.sub(r"[^A-Za-z0-9._-]+", "-", value.lower()).strip("-")
    return slug or "job"

selected = []
for job in jobs:
    job_id = job_id_of(job)
    conclusion = job.get("conclusion") or ""
    status = job.get("status") or ""
    if requested and job_id not in requested:
        continue
    if failed_only and conclusion != "failure":
        continue
    selected.append((job_id, status, conclusion, slugify(job.get("name") or job_id), job.get("name") or job_id))

with open(selection_path, "w", encoding="utf-8") as handle:
    handle.write(
        "RUN\t{status}\t{conclusion}\t{title}\n".format(
            status=run.get("status") or "",
            conclusion=run.get("conclusion") or "",
            title=(run.get("display_title") or "").replace("\t", " "),
        )
    )
    for row in selected:
        handle.write("JOB\t{}\t{}\t{}\t{}\t{}\n".format(*[field.replace("\t", " ") for field in row]))
PY

run_status=""
run_conclusion=""
run_title=""
downloaded_count=0
declare -a downloaded_rows=()

while IFS=$'\t' read -r kind col1 col2 col3 col4 col5; do
  case "$kind" in
    RUN)
      run_status="$col1"
      run_conclusion="$col2"
      run_title="$col3"
      ;;
    JOB)
      job_id="$col1"
      job_status="$col2"
      job_conclusion="$col3"
      job_slug="$col4"
      job_name="$col5"
      destination="$out_dir/${job_id}-${job_slug}.log"
      if ! gh api -H 'Accept: application/vnd.github+json' "repos/$repo/actions/jobs/$job_id/logs" >"$destination" 2>"$tmp_dir/log_err_${job_id}"; then
        err_msg="$(cat "$tmp_dir/log_err_${job_id}" 2>/dev/null || true)"
        printf 'warning: skipping job %s (%s): %s\n' "$job_id" "$job_name" "$err_msg" >&2
        rm -f "$destination"
        continue
      fi
      bytes_written="$(wc -c <"$destination" | tr -d '[:space:]')"
      downloaded_rows+=("$job_id|$job_status|$job_conclusion|$bytes_written|$destination|$job_name")
      downloaded_count=$((downloaded_count + 1))
      ;;
  esac
done <"$selection_file"

if [[ "$downloaded_count" -eq 0 ]]; then
  die "no jobs matched selection"
fi

printf 'run: %s\n' "$run_id"
printf 'repo: %s\n' "$repo"
printf 'title: %s\n' "$run_title"
printf 'status: %s\n' "$run_status"
printf 'conclusion: %s\n' "$run_conclusion"
printf 'output_dir: %s\n' "$out_dir"

if [[ "$run_status" != "completed" ]]; then
  printf 'warning: run still %s; fetched logs may be incomplete\n' "$run_status"
fi

printf 'downloaded_jobs: %s\n' "$downloaded_count"
for row in "${downloaded_rows[@]}"; do
  IFS='|' read -r job_id job_status job_conclusion bytes_written destination job_name <<<"$row"
  printf '%s\t%s\t%s\t%s bytes\t%s\t%s\n' "$job_id" "$job_status" "$job_conclusion" "$bytes_written" "$destination" "$job_name"
done

