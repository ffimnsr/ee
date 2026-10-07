#!/usr/bin/env bash
#
# Build `ee` with profiling symbols and run it under a profiler.
#
# The build uses `--profile profiling` (see the root Cargo.toml): release codegen
# plus `debug = "line-tables-only"`, so samples carry function and line names
# without changing inlining. This script adds two codegen flags on top:
#
#   -C force-frame-pointers=yes   keeps stack unwinding cheap and complete.
#                                 This does change codegen slightly, so the
#                                 profiled binary is not byte-identical to
#                                 `release`. Debug info alone does not.
#   -C symbol-mangling-version=v0 makes `perf`/samply frames readable.
#
# `CFLAGS`/`CXXFLAGS` carry the frame-pointer flag to C dependencies
# (tree-sitter), which do not read `RUSTFLAGS`.
#
# Run profiling builds through this script. A plain `cargo build --profile
# profiling` writes the same `target/profiling` directory without these flags,
# so the two commands would rebuild over each other.
#
# Artifacts are written under `<target-dir>/profiling-out/`, which is gitignored.
#
# Examples:
#   scripts/profile.sh                                    # samply, live TUI
#   scripts/profile.sh samply --duration 30 -- test_assets/big.yaml
#   scripts/profile.sh perf --pid 12345 --duration 20     # attach to running ee
#   scripts/profile.sh flamegraph -- test_assets/big.yaml
#   scripts/profile.sh callgrind -- do file head test_assets/vbig-10gb.txt
#   scripts/profile.sh --print-only perf -- do doctor    # show the command only
#
# The syntax-span probe runs the same way by pointing `--bin` at the test binary:
#   cargo test --profile profiling -p ee-xi-core-lib --no-run
#   scripts/profile.sh samply --no-build --bin target/profiling/deps/xi_core_lib-<hash> \
#       -- syntax_span_render_perf_probe --ignored --nocapture

set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/profile.sh [TOOL] [OPTIONS] [-- ARGS...]

Build the profiling binary and run it under TOOL.

Tools:
  samply       (default) sampling profiler, opens the Firefox Profiler UI
  perf         Linux sampling profiler, writes perf.data
  flamegraph   perf record plus inferno-flamegraph, writes an SVG
  callgrind    Valgrind call counts and instruction counts (10-50x slowdown)
  heaptrack    allocation and peak-memory profiler

Options:
  --bin PATH      binary to profile (default: <target-dir>/profiling/ee)
  --no-build      skip the profiling build; requires an existing --bin
  --pid PID       attach to a running process (samply, perf, flamegraph)
  --duration SECS stop after SECS (samply; perf and flamegraph need --pid)
  --out PATH      artifact path (default: <target-dir>/profiling-out/<tool>.*)
  --freq N        sampling frequency in Hz (perf/flamegraph; samply when set)
  --dwarf         use DWARF call graphs instead of frame pointers (perf)
  --print-only    print the commands without running them
  -h, --help      show this help

Everything after `--` is passed to the profiled binary, for example:
  scripts/profile.sh samply -- do file head test_assets/vbig-10gb.txt

Notes:
  - The live TUI needs a TTY. For unattended runs prefer `ee do ...` paths or
    the probe test binary, since those are reproducible.
  - callgrind and heaptrack are far too slow for interactive editing; use them
    on headless paths.
  - samply and perf need unprivileged sampling; if
    `kernel.perf_event_paranoid` is above 1, samply refuses to start:
      sudo sysctl kernel.perf_event_paranoid=1
EOF
}

fail() {
    printf 'ERROR: %s\n' "$1" >&2
    exit 1
}

warn() {
    printf 'WARNING: %s\n' "$1" >&2
}

require_tool() {
    local name="$1"
    local hint="$2"

    if command -v "$name" >/dev/null 2>&1; then
        return 0
    fi

    # A dry run only needs the command line, so a missing tool is not fatal.
    if [[ "$print_only" -eq 1 ]]; then
        warn "'$name' is not installed. $hint"
        return 0
    fi

    fail "'$name' is not installed. $hint"
}

# Feature-detect a flag so the script keeps working across tool versions
# instead of passing an option the installed binary does not understand.
tool_supports_flag() {
    local tool="$1"
    local subcommand="$2"
    local flag="$3"

    if ! command -v "$tool" >/dev/null 2>&1; then
        # A dry run has no way to ask, and printing the intended command is more
        # useful than printing a silently degraded one.
        [[ "$print_only" -eq 1 ]]
        return
    fi

    if [[ -n "$subcommand" ]]; then
        "$tool" "$subcommand" --help 2>&1 | grep -q -- "$flag"
    else
        "$tool" --help 2>&1 | grep -q -- "$flag"
    fi
}

# Run a command, or print it when --print-only was requested.
run_step() {
    local -a cmd=( "$@" )

    if [[ "$print_only" -eq 1 ]]; then
        printf '[profile] would run:'
        printf ' %q' "${cmd[@]}"
        printf '\n'
        return 0
    fi

    printf '[profile] %s\n' "${cmd[0]}"
    "${cmd[@]}"
}

tool=""
bin_path=""
do_build=1
pid=""
duration=""
out_path=""
freq=""
call_graph="fp"
print_only=0
extra=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --bin)
            bin_path="${2:?missing value for --bin}"
            shift 2
            ;;
        --no-build)
            do_build=0
            shift
            ;;
        --pid)
            pid="${2:?missing value for --pid}"
            shift 2
            ;;
        --duration)
            duration="${2:?missing value for --duration}"
            shift 2
            ;;
        --out)
            out_path="${2:?missing value for --out}"
            shift 2
            ;;
        --freq)
            freq="${2:?missing value for --freq}"
            shift 2
            ;;
        --dwarf)
            call_graph="dwarf"
            shift
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
            if [[ -n "$tool" ]]; then
                printf 'unexpected argument: %s\n' "$1" >&2
                usage >&2
                exit 1
            fi
            tool="$1"
            shift
            ;;
    esac
done

tool="${tool:-samply}"

case "$tool" in
    samply|perf|flamegraph|callgrind|heaptrack)
        ;;
    *)
        printf 'unknown tool: %s\n' "$tool" >&2
        usage >&2
        exit 1
        ;;
esac

if [[ -n "$pid" ]] && ! [[ "$pid" =~ ^[0-9]+$ ]]; then
    fail "--pid must be a process id, got '$pid'"
fi

if [[ -n "$duration" ]] && ! [[ "$duration" =~ ^[0-9]+$ ]]; then
    fail "--duration must be a whole number of seconds, got '$duration'"
fi

if [[ -n "$freq" ]] && ! [[ "$freq" =~ ^[0-9]+$ ]]; then
    fail "--freq must be a whole number of Hz, got '$freq'"
fi

if [[ -n "$duration" && -z "$pid" && "$tool" != "samply" ]]; then
    fail "--duration without --pid is only supported by samply; for perf and flamegraph, attach with --pid or stop the capture with Ctrl-C"
fi

if [[ -n "$pid" && "$tool" != "samply" && "$tool" != "perf" && "$tool" != "flamegraph" ]]; then
    fail "--pid is not supported by '$tool'; use samply, perf, or flamegraph"
fi

if [[ "$call_graph" == "dwarf" && "$tool" != "perf" && "$tool" != "flamegraph" ]]; then
    fail "--dwarf only applies to perf and flamegraph"
fi

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

target_dir="${CARGO_TARGET_DIR:-$repo_root/target}"
case "$target_dir" in
    /*)
        ;;
    *)
        target_dir="$repo_root/$target_dir"
        ;;
esac
out_dir="$target_dir/profiling-out"

if [[ -z "$bin_path" ]]; then
    bin_path="$target_dir/profiling/ee"
fi

require_bin() {
    if [[ ! -x "$bin_path" ]]; then
        if [[ "$do_build" -eq 1 ]]; then
            fail "expected an executable at $bin_path after the build"
        fi
        fail "no executable at $bin_path; drop --no-build to build it, or fix --bin"
    fi
}

# Profiling builds must not be silently skipped when the caller asked for one.
build_binary() {
    local rustflags="${RUSTFLAGS-}"
    local frame_pointer_flags="-C force-frame-pointers=yes -C symbol-mangling-version=v0"

    if [[ -n "$rustflags" ]]; then
        rustflags="$rustflags $frame_pointer_flags"
    else
        rustflags="$frame_pointer_flags"
    fi

    printf '[profile] building ee-cli with debug info and frame pointers\n'
    RUSTFLAGS="$rustflags" \
        CFLAGS="${CFLAGS-} -fno-omit-frame-pointer" \
        CXXFLAGS="${CXXFLAGS-} -fno-omit-frame-pointer" \
        cargo build --profile profiling -p ee-cli --bin ee
}

# Writes the binary plus trailing arguments into `profiled_cmd`.
build_profiled_cmd() {
    profiled_cmd=( "$bin_path" )
    if [[ "${extra[0]+set}" == "set" ]]; then
        profiled_cmd+=( "${extra[@]}" )
    fi
}

# samply refuses to start and perf quietly records nothing useful when
# unprivileged sampling is blocked, so warn before launching either one.
check_sampling_permissions() {
    local paranoid_path="/proc/sys/kernel/perf_event_paranoid"

    [[ -r "$paranoid_path" ]] || return 0

    local value
    value="$(cat "$paranoid_path")"
    if [[ "$value" =~ ^[0-9]+$ ]] && (( value > 1 )); then
        warn "kernel.perf_event_paranoid=$value blocks unprivileged sampling; run: sudo sysctl kernel.perf_event_paranoid=1"
    fi
}

run_samply() {
    require_tool samply "Install it with: cargo install samply"
    check_sampling_permissions

    local -a cmd=( samply record )

    if [[ -n "$freq" ]] && tool_supports_flag samply record --rate; then
        cmd+=( --rate "$freq" )
    fi

    if [[ -n "$duration" ]]; then
        if tool_supports_flag samply record --duration; then
            cmd+=( --duration "$duration" )
        else
            fail "the installed samply has no '--duration' support; stop the recording manually instead"
        fi
    fi

    if [[ -n "$out_path" ]]; then
        cmd+=( --output "$out_path" )
    fi

    if [[ -n "$pid" ]]; then
        if ! tool_supports_flag samply record --pid; then
            fail "the installed samply has no '--pid' attach support; run the target under 'samply record -- <command>' instead, or upgrade samply"
        fi
        cmd+=( --pid "$pid" )
        run_step "${cmd[@]}"
        return
    fi

    build_profiled_cmd
    cmd+=( -- )
    cmd+=( "${profiled_cmd[@]}" )
    run_step "${cmd[@]}"
}

run_perf() {
    require_tool perf "perf is Linux-only; install linux-tools for your kernel, or use samply on macOS"
    check_sampling_permissions

    local data_path="${out_path:-$out_dir/perf.data}"
    mkdir -p "$(dirname "$data_path")"

    local -a cmd=(
        perf record
        -g
        --call-graph "$call_graph"
        -F "${freq:-999}"
        -o "$data_path"
    )

    if [[ -n "$pid" ]]; then
        cmd+=( -p "$pid" -- sleep "$duration" )
    else
        build_profiled_cmd
        cmd+=( -- )
        cmd+=( "${profiled_cmd[@]}" )
    fi

    run_step "${cmd[@]}"

    if [[ "$print_only" -eq 0 ]]; then
        printf '[profile] inspect with: perf report -i %q\n' "$data_path"
    fi
}

run_flamegraph() {
    require_tool perf "perf is Linux-only; use 'cargo install flamegraph' on macOS, or samply on any platform"
    require_tool inferno-flamegraph "Install it with: cargo install inferno"
    check_sampling_permissions

    local data_path="${out_path:-$out_dir/flamegraph.data}"
    local svg_path="${data_path%.data}.svg"
    if [[ "$svg_path" == "$data_path" ]]; then
        svg_path="$data_path.svg"
    fi
    mkdir -p "$(dirname "$data_path")"

    local -a record_cmd=(
        perf record
        -g
        --call-graph "$call_graph"
        -F "${freq:-999}"
        -o "$data_path"
    )

    if [[ -n "$pid" ]]; then
        record_cmd+=( -p "$pid" -- sleep "$duration" )
    else
        build_profiled_cmd
        record_cmd+=( -- )
        record_cmd+=( "${profiled_cmd[@]}" )
    fi

    if [[ "$print_only" -eq 1 ]]; then
        run_step "${record_cmd[@]}"
        printf '[profile] would run: perf script -i %q | inferno-flamegraph > %q\n' \
            "$data_path" "$svg_path"
        return
    fi

    run_step "${record_cmd[@]}"
    perf script -i "$data_path" | inferno-flamegraph > "$svg_path"
    printf '[profile] wrote %s\n' "$svg_path"
}

run_callgrind() {
    require_tool valgrind "Install it with your package manager, for example: apt install valgrind"

    warn "callgrind is 10-50x slower; the live TUI is unusable under it"
    warn "prefer headless paths (ee do ...) or the syntax-span probe test binary"

    local out_file="${out_path:-$out_dir/callgrind.out}"
    mkdir -p "$(dirname "$out_file")"

    build_profiled_cmd

    local -a cmd=(
        valgrind
        --tool=callgrind
        --callgrind-out-file="$out_file"
        --cache-sim=yes
        --branch-sim=yes
    )
    cmd+=( "${profiled_cmd[@]}" )

    run_step "${cmd[@]}"

    if [[ "$print_only" -eq 0 ]]; then
        printf '[profile] inspect with: callgrind_annotate --auto=yes %q\n' "$out_file"
        printf '[profile] or open %q in kcachegrind\n' "$out_file"
    fi
}

run_heaptrack() {
    require_tool heaptrack "Install it with your package manager, for example: apt install heaptrack heaptrack-gui"

    local out_file="${out_path:-$out_dir/heaptrack}"
    mkdir -p "$(dirname "$out_file")"

    local -a cmd=( heaptrack )
    if tool_supports_flag heaptrack "" --output; then
        cmd+=( --output "$out_file" )
    fi

    build_profiled_cmd
    cmd+=( "${profiled_cmd[@]}" )

    run_step "${cmd[@]}"

    if [[ "$print_only" -eq 0 ]]; then
        printf '[profile] artifacts next to %q; open with heaptrack_gui\n' "$out_file"
    fi
}

if [[ "$do_build" -eq 1 ]]; then
    build_binary
fi

require_bin

case "$tool" in
    samply)
        run_samply
        ;;
    perf)
        run_perf
        ;;
    flamegraph)
        run_flamegraph
        ;;
    callgrind)
        run_callgrind
        ;;
    heaptrack)
        run_heaptrack
        ;;
esac
