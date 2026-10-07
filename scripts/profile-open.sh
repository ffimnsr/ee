#!/usr/bin/env bash
#
# Profile the editor's file-open pipeline (read --> analysis --> rope/VLF -->
# editor/view init --> first render) without needing an interactive TTY.
#
# Runs `open_path_perf_probe` (crates/xi-core-lib/src/tabs/tests/open_perf.rs):
# a manual, ignored test that drives the real `CoreState::do_new_view` path
# through `XiCore` with a recording peer, reports per-stage wall times, and
# writes a JSON artifact when requested.
#
# Timing mode (default) builds the normal test binary and prints the stage
# table. Profiler mode (--tool) builds the profiling test binary (same flags
# as scripts/profile.sh: frame pointers, v0 mangling) and hands it to
# scripts/profile.sh, so the open pipeline can be sampled with samply, perf,
# flamegraph, callgrind, or heaptrack.
#
# Examples:
#   scripts/profile-open.sh                                   # timing probe, defaults
#   scripts/profile-open.sh --sizes 1,4,8 --shapes code,longline
#   scripts/profile-open.sh --json target/profiling-out/open-probe.json
#   scripts/profile-open.sh --tool samply --iters 1 --sizes 1,4
#   scripts/profile-open.sh --tool callgrind --iters 1 --shapes longline
#   scripts/profile-open.sh --print-only
#
# Options:
#   --tool TOOL      none (default; timing only) | samply | perf |
#                    flamegraph | callgrind | heaptrack
#   --sizes LIST     comma-separated MiB sizes (probe default: 1,4,8)
#   --shapes LIST    comma-separated: code,longline,mixedcrlf (default all)
#   --iters N        warm iterations per fixture (default 5; 1 for profilers)
#   --ext EXT        fixture extension (default txt; rs exercises syntax render)
#   --json PATH      write the probe's machine-readable JSON artifact
#   --profile P      cargo profile: test (default) or profiling
#   --jobs N         cargo jobs cap (default 2; the repo has heavy crates and
#                    parallel rustc jobs are the usual OOM source on 16 GiB
#                    boxes; raise only if memory allows)
#   --bin PATH       use an existing test binary (skips the cargo build)
#   --print-only     print commands without running them
#   -h, --help       show this help
#
# Probe-relevant env is forwarded verbatim:
#   EE_OPEN_PROBE_SIZES EE_OPEN_PROBE_SHAPES EE_OPEN_PROBE_ITERS
#   EE_OPEN_PROBE_EXT EE_OPEN_PROBE_JSON

set -euo pipefail

usage() {
    sed -n '2,35p' "$0" | sed 's/^# \{0,1\}//'
}

fail() {
    printf 'ERROR: %s\n' "$1" >&2
    exit 1
}

warn() {
    printf 'WARNING: %s\n' "$1" >&2
}

run_step() {
    if [[ "$print_only" -eq 1 ]]; then
        printf '[profile-open] would run:'
        printf ' %q' "$@"
        printf '\n'
        return 0
    fi
    "$@"
}

tool="none"
profile_kind=""
sizes=""
shapes=""
iters=""
ext=""
json_path=""
bin_path=""
do_build=1
jobs=2
duration=""
print_only=0
extra=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --tool)
            tool="${2:?missing value for --tool}"
            shift 2
            ;;
        --sizes)
            sizes="${2:?missing value for --sizes}"
            shift 2
            ;;
        --shapes)
            shapes="${2:?missing value for --shapes}"
            shift 2
            ;;
        --iters)
            iters="${2:?missing value for --iters}"
            shift 2
            ;;
        --ext)
            ext="${2:?missing value for --ext}"
            shift 2
            ;;
        --json)
            json_path="${2:?missing value for --json}"
            shift 2
            ;;
        --profile)
            profile_kind="${2:?missing value for --profile}"
            shift 2
            ;;
        --jobs)
            jobs="${2:?missing value for --jobs}"
            shift 2
            ;;
        --bin)
            bin_path="${2:?missing value for --bin}"
            shift 2
            ;;
        --duration)
            duration="${2:?missing value for --duration}"
            shift 2
            ;;
        --print-only)
            print_only=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        --)
            shift
            while [[ $# -gt 0 ]]; do
                extra+=( "$1" )
                shift
            done
            break
            ;;
        -*)
            printf 'unknown option: %s\n' "$1" >&2
            usage >&2
            exit 1
            ;;
        *)
            printf 'unexpected argument: %s\n' "$1" >&2
            usage >&2
            exit 1
            ;;
    esac
done

case "$tool" in
    none|samply|perf|flamegraph|callgrind|heaptrack) ;;
    *) fail "unknown tool '$tool'; use none, samply, perf, flamegraph, callgrind, or heaptrack" ;;
esac

if [[ -n "$duration" && "$tool" != "samply" ]]; then
    fail "--duration only applies to --tool samply (perf/flamegraph need --pid via scripts/profile.sh)"
fi

if [[ -n "$iters" ]] && ! [[ "$iters" =~ ^[0-9]+$ ]]; then
    fail "--iters must be a whole number, got '$iters'"
fi

if [[ -n "$jobs" ]] && ! [[ "$jobs" =~ ^[1-9][0-9]*$ ]]; then
    fail "--jobs must be a positive integer, got '$jobs'"
fi

if [[ -z "$profile_kind" ]]; then
    if [[ "$tool" == "none" ]]; then
        profile_kind="test"
    else
        profile_kind="profiling"
    fi
fi
case "$profile_kind" in
    test|profiling) ;;
    *) fail "--profile must be 'test' or 'profiling', got '$profile_kind'" ;;
esac

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

target_dir="${CARGO_TARGET_DIR:-$repo_root/target}"
case "$target_dir" in
    /*) ;;
    *) target_dir="$repo_root/$target_dir" ;;
esac

# On 16 GiB boxes the parallel rustc job count is the usual OOM trigger, so
# the script caps it by default (see --jobs).
if [[ -n "${CARGO_BUILD_JOBS:-}" ]]; then
    jobs="$CARGO_BUILD_JOBS"
fi

find_test_binary() {
    cargo test -j "$jobs" --profile "$profile_kind" -p ee-xi-core-lib --no-run >/dev/null
    local profile_dir="debug"
    [[ "$profile_kind" == "profiling" ]] && profile_dir="profiling"
    local deps_dir="$target_dir/$profile_dir/deps"
    # JSON messages only describe freshly-compiled targets, so pick the newest
    # executable in the deps dir instead (cargo names it xi_core_lib-<hash>).
    bin_path="$(ls -t "$deps_dir"/xi_core_lib-* 2>/dev/null \
        | grep -E '/xi_core_lib-[0-9a-f]+$' | head -1 || true)"
    if [[ -z "$bin_path" || ! -x "$bin_path" ]]; then
        fail "could not locate an ee-xi-core-lib test binary in $deps_dir; run with --bin instead"
    fi
}

build_env_prefix() {
    env_prefix=()
    if [[ -n "$sizes" ]]; then env_prefix+=( "EE_OPEN_PROBE_SIZES=$sizes" ); fi
    if [[ -n "$shapes" ]]; then env_prefix+=( "EE_OPEN_PROBE_SHAPES=$shapes" ); fi
    if [[ -n "$iters" ]]; then env_prefix+=( "EE_OPEN_PROBE_ITERS=$iters" ); fi
    if [[ -n "$ext" ]]; then env_prefix+=( "EE_OPEN_PROBE_EXT=$ext" ); fi
    if [[ -n "$json_path" ]]; then env_prefix+=( "EE_OPEN_PROBE_JSON=$json_path" ); fi
}

build_env_prefix

probe_args=( open_path_perf_probe --ignored --nocapture --test-threads=1 )
if [[ "${extra[0]+set}" == "set" ]]; then
    probe_args=( "${extra[@]}" "${probe_args[@]}" )
fi

if [[ "$print_only" -eq 1 ]]; then
    printf '[profile-open] would run: cargo test -j %s --profile %s -p ee-xi-core-lib --no-run --message-format=json (then locate the test binary from its output)\n' \
        "$jobs" "$profile_kind"
    if [[ "$tool" == "none" ]]; then
        printf '[profile-open] would run:'
        printf ' %q' env "${env_prefix[@]}" "<test-binary>" "${probe_args[@]}"
        printf '\n'
    else
        printf '[profile-open] would run: %s %s --no-build --bin <test-binary> -- %s\n' \
            "$repo_root/scripts/profile.sh" "$tool" "${probe_args[*]}"
    fi
    exit 0
fi

if [[ -z "$bin_path" ]]; then
    find_test_binary
fi
if [[ ! -x "$bin_path" ]]; then
    fail "no executable at $bin_path; drop --bin or rebuild"
fi

if [[ "$tool" == "none" ]]; then
    mkdir -p "$target_dir/profiling-out"
    run_step env "${env_prefix[@]}" "$bin_path" "${probe_args[@]}"
    exit 0
fi

# Profiler mode: delegate to scripts/profile.sh with the already-built binary.
[[ -z "$iters" ]] && iters=1 && env_prefix+=( "EE_OPEN_PROBE_ITERS=$iters" )
mkdir -p "$target_dir/profiling-out"
prof_cmd=( "$repo_root/scripts/profile.sh" "$tool" --no-build --bin "$bin_path" )
if [[ -n "$duration" ]]; then
    prof_cmd+=( --duration "$duration" )
fi
prof_cmd+=( -- )
prof_cmd+=( "${probe_args[@]}" )
run_step env "${env_prefix[@]}" "${prof_cmd[@]}"