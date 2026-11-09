#!/usr/bin/env python3
"""Run every `ee-cli --bin ee` test in small fresh-process chunks.

A single process running the whole bin suite accumulates harness allocations
(large file fixtures, backends, grammar data). On memory-constrained hosts that
pushes MemAvailable under the 10% floor. Splitting by test module releases peak
allocations between chunks and keeps peak RSS bounded.

Each chunk runs single-threaded and under scripts/run-tests-memguard.sh, which
kills the chunk if MemAvailable drops below the configured floor.
"""

import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
MEMGUARD = REPO_ROOT / "scripts" / "run-tests-memguard.sh"
CARGO = [
    "cargo",
    "test",
    "--quiet",
    "-p",
    "ee-cli",
    "--features",
    "agents",
    "--bin",
    "ee",
]


def list_tests() -> list[str]:
    listing = subprocess.run(
        CARGO + ["--", "--list"], check=True, capture_output=True, text=True
    ).stdout
    names = []
    for line in listing.splitlines():
        # Test names contain `::`, so split off only the trailing `: test` marker.
        name = line.rsplit(":", 1)[0].strip()
        if name:
            names.append(name)
    return names


def group_names(names: list[str]) -> list[str]:
    """Group by the first two path segments, e.g. `tests::misc`."""
    groups = set()
    for name in names:
        parts = name.split("::")
        groups.add("::".join(parts[:2]) if len(parts) >= 2 else parts[0])
    return sorted(groups)


def main() -> int:
    names = list_tests()
    groups = group_names(names)

    for index, group in enumerate(groups, start=1):
        print(f"[{index}/{len(groups)}] {group}", flush=True)
        command = [str(MEMGUARD)] + CARGO + [group, "--", "--test-threads=1"]
        result = subprocess.run(command)
        if result.returncode != 0:
            print(f"FAILED: {group} ({result.returncode})", file=sys.stderr, flush=True)
            return result.returncode

    print(f"all {len(names)} bin tests passed across {len(groups)} chunks")
    return 0


if __name__ == "__main__":
    sys.exit(main())
