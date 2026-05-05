#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage:
  scripts/test-workspace-summary.sh [cargo test args...]
  scripts/test-workspace-summary.sh --parse-only <log-file>
  scripts/test-workspace-summary.sh --print-command [cargo test args...]

Runs `cargo test` with provided args. If no args passed, defaults to `--workspace`.
Hides raw cargo output, then prints merged totals for:
passed, failed, ignored, measured, filtered out.

Also prints failed test names and failed crates.
EOF
}

print_heartbeat() {
    local start_ts="$1"
    local cargo_pid="$2"
    local elapsed spinner_index spinner_char
    local spinner_chars='|/-\'

    spinner_index=0

    while kill -0 "$cargo_pid" 2>/dev/null; do
        sleep 1
        if ! kill -0 "$cargo_pid" 2>/dev/null; then
            break
        fi
        elapsed=$(( $(date +%s) - start_ts ))
        if [ -t 1 ]; then
            spinner_char="${spinner_chars:spinner_index:1}"
            spinner_index=$(( (spinner_index + 1) % 4 ))
            printf '\rcargo test running... %s %ss elapsed' "$spinner_char" "$elapsed"
        elif [ $(( elapsed % 10 )) -eq 0 ]; then
            printf 'cargo test still running... %ss elapsed\n' "$elapsed"
        fi
    done
}

parse_log() {
    local log_file="$1"

    awk '
BEGIN {
    passed = 0
    failed = 0
    ignored = 0
    measured = 0
    filtered_out = 0
}

function trim(value) {
    gsub(/^[[:space:]]+|[[:space:]]+$/, "", value)
    return value
}

function add_failed_test(name) {
    name = trim(name)
    if (name == "" || (name in seen_failed_tests)) {
        return
    }
    seen_failed_tests[name] = 1
    failed_test_order[++failed_test_count] = name
}

function add_failed_crate(name) {
    name = trim(name)
    if (name == "" || (name in seen_failed_crates)) {
        return
    }
    seen_failed_crates[name] = 1
    failed_crate_order[++failed_crate_count] = name
}

function extract_count(line, label, matches, pattern) {
    pattern = "([0-9]+) " label
    if (match(line, pattern, matches)) {
        return matches[1] + 0
    }
    return 0
}

{
    if (in_failures) {
        if ($0 ~ /^test result:/ || $0 ~ /^error:/ || $0 ~ /^     Running / || $0 ~ /^   Doc-tests /) {
            in_failures = 0
        } else if ($0 ~ /^[[:space:]]*$/) {
            next
        } else {
            add_failed_test($0)
            next
        }
    }

    if ($0 ~ /^failures:$/) {
        in_failures = 1
        next
    }

    if ($0 ~ /^error: test failed, to rerun pass /) {
        crate_part = substr($0, index($0, "-p ") + 3)
        split(crate_part, crate_fields, /[[:space:]]+/)
        crate_name = crate_fields[1]
        gsub(/[`'"'"'",]/, "", crate_name)
        add_failed_crate(crate_name)
        next
    }

    if ($0 ~ /^test result:/) {
        passed += extract_count($0, "passed;")
        failed += extract_count($0, "failed;")
        ignored += extract_count($0, "ignored;")
        measured += extract_count($0, "measured;")
        filtered_out += extract_count($0, "filtered out;")
    }
}

END {
    print "Merged test summary:"
    print "passed: " passed
    print "failed: " failed
    print "ignored: " ignored
    print "measured: " measured
    print "filtered out: " filtered_out
    print ""
    print "Failed tests:"
    if (failed_test_count == 0) {
        print "- none"
    } else {
        for (i = 1; i <= failed_test_count; i++) {
            print "- " failed_test_order[i]
        }
    }
    print ""
    print "Failed crates:"
    if (failed_crate_count == 0) {
        print "- none"
    } else {
        for (i = 1; i <= failed_crate_count; i++) {
            print "- " failed_crate_order[i]
        }
    }
}
' "$log_file"
}

run_tests() {
    local log_file cargo_pid heartbeat_pid start_ts cargo_status
    local cargo_args=()
    log_file="$(mktemp)"
    trap "rm -f '$log_file'; if [ -n \"\${heartbeat_pid-}\" ]; then kill \"\$heartbeat_pid\" 2>/dev/null || true; fi" EXIT
    resolve_cargo_args cargo_args "$@"

    start_ts="$(date +%s)"
    printf 'cargo test started...\n'

    set +e
    cargo test "${cargo_args[@]}" >"$log_file" 2>&1 &
    cargo_pid=$!
    print_heartbeat "$start_ts" "$cargo_pid" &
    heartbeat_pid=$!
    wait "$cargo_pid"
    cargo_status=$?
    kill "$heartbeat_pid" 2>/dev/null || true
    wait "$heartbeat_pid" 2>/dev/null || true
    set -e

    if [ -t 1 ]; then
        printf '\r'
    fi
    printf 'cargo test finished. building summary...\n'
    parse_log "$log_file"

    return "$cargo_status"
}

resolve_cargo_args() {
    local -n out_args="$1"
    shift

    if [ "$#" -eq 0 ]; then
        out_args=(--workspace)
        return
    fi

    out_args=("$@")
}

print_command() {
    local cargo_args=()
    resolve_cargo_args cargo_args "$@"
    printf 'cargo test'
    if [ "${#cargo_args[@]}" -gt 0 ]; then
        printf ' %q' "${cargo_args[@]}"
    fi
    printf '\n'
}

main() {
    case "${1-}" in
        --parse-only)
            shift
            if [ "$#" -ne 1 ]; then
                usage >&2
                exit 2
            fi
            parse_log "$1"
            ;;
        --print-command)
            shift
            print_command "$@"
            ;;
        -h|--help)
            usage
            ;;
        *)
            run_tests "$@"
            ;;
    esac
}

main "$@"
