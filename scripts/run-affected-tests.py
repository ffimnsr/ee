#!/usr/bin/env python3
"""Run focused tests for changed Rust files, then check direct dependents.

Maps git diff paths to workspace packages and cargo targets, derives the
narrowest test filter for each changed module, and runs only those filters.
Full package suites stay manual. Direct dependents are checked with
`cargo check --tests` so test code that uses changed APIs still compiles.

Usage:
  scripts/run-affected-tests.sh
  scripts/run-affected-tests.sh --base origin/main
  scripts/run-affected-tests.sh crates/ee-mcp/src/proxy.rs
  scripts/run-affected-tests.sh --list
"""

from __future__ import annotations

import argparse
import json
import shlex
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Optional

REPO_ROOT = Path(__file__).resolve().parent.parent
MEMGUARD = REPO_ROOT / "scripts" / "run-tests-memguard.sh"

# ee-cli integration tests are gated behind this feature; keep the default
# aligned with scripts/run-ee-cli-touched-tests.sh.
DEFAULT_FEATURES = {"ee-cli": {"agents"}}

KIND_ORDER = {
    "lib": 0,
    "bin": 1,
    "test": 2,
    "bench": 3,
    "example": 4,
    "custom-build": 5,
}


@dataclass(frozen=True)
class Target:
    name: str
    kinds: tuple[str, ...]
    src_path: Path
    root_dir: Optional[Path]
    runs_tests: bool


@dataclass(frozen=True)
class Package:
    id: str
    name: str
    root: Path
    manifest: Path
    targets: tuple[Target, ...]


def run(
    argv: list[str],
    *,
    check: bool = True,
    capture: bool = True,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        argv,
        cwd=REPO_ROOT,
        check=check,
        capture_output=capture,
        text=True,
    )


def load_metadata() -> dict:
    try:
        completed = run(
            ["cargo", "metadata", "--format-version", "1"],
            check=False,
            capture=True,
        )
    except FileNotFoundError:
        sys.exit("cargo not found on PATH")
    if completed.returncode != 0:
        sys.stderr.write(completed.stderr)
        sys.exit(completed.returncode)
    return json.loads(completed.stdout)


def target_kind(target: Target) -> str:
    kinds = set(target.kinds)
    if kinds & {"lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"}:
        return "lib"
    if "bin" in kinds:
        return "bin"
    if "test" in kinds:
        return "test"
    if "bench" in kinds:
        return "bench"
    if "example" in kinds:
        return "example"
    return "custom-build"


def target_root(src_path: Path, kinds: set[str]) -> Optional[Path]:
    if "custom-build" in kinds:
        return src_path.parent
    if kinds & {"test", "bench", "example"}:
        if src_path.name == "main.rs":
            return src_path.parent
        return src_path.with_suffix("")
    if "bin" in kinds:
        if src_path.name == "main.rs":
            return src_path.parent
        return src_path.with_suffix("")
    # A conventional crate root (lib.rs/main.rs) owns every sibling module.
    if src_path.name in {"lib.rs", "mod.rs"}:
        return src_path.parent
    # A custom crate root path (e.g. src/ffi.rs) owns its own module folder.
    return src_path.with_suffix("")


def load_packages(metadata: dict) -> list[Package]:
    workspace_ids = set(metadata.get("workspace_members", []))
    packages: list[Package] = []
    for entry in metadata.get("packages", []):
        if entry["id"] not in workspace_ids:
            continue
        manifest = Path(entry["manifest_path"]).resolve()
        root = manifest.parent
        targets = []
        for target in entry.get("targets", []):
            src_path = Path(target["src_path"]).resolve()
            kinds = tuple(target.get("kind", []))
            targets.append(
                Target(
                    name=target["name"],
                    kinds=kinds,
                    src_path=src_path,
                    root_dir=target_root(src_path, set(kinds)),
                    runs_tests=bool(target.get("test", True)),
                )
            )
        packages.append(
            Package(
                id=entry["id"],
                name=entry["name"],
                root=root,
                manifest=manifest,
                targets=tuple(targets),
            )
        )
    return packages


def is_under(path: Path, root: Path) -> bool:
    try:
        path.relative_to(root)
    except ValueError:
        return False
    return True


def package_for_path(path: Path, packages: Iterable[Package]) -> Optional[Package]:
    matches = [package for package in packages if is_under(path, package.root)]
    if not matches:
        return None
    return max(matches, key=lambda package: len(str(package.root)))


def select_target(package: Package, path: Path) -> Optional[Target]:
    best: Optional[tuple[tuple[int, int, int], Target]] = None
    for target in package.targets:
        kind = target_kind(target)
        if kind == "custom-build":
            continue
        if path == target.src_path:
            score = (2, len(str(target.root_dir or target.src_path)), -KIND_ORDER[kind])
        elif target.root_dir is not None and is_under(path, target.root_dir):
            score = (1, len(str(target.root_dir)), -KIND_ORDER[kind])
        else:
            continue
        if best is None or score > best[0]:
            best = (score, target)
    return best[1] if best is not None else None


def module_filter(target: Target, path: Path) -> str:
    """Return the narrowest cargo test filter for a changed module file."""
    if path == target.src_path or target.root_dir is None:
        return ""
    try:
        relative = path.relative_to(target.root_dir)
    except ValueError:
        return ""
    parts = list(relative.parts)
    if not parts:
        return ""
    last = parts[-1]
    if last == "mod.rs":
        parts.pop()
    elif last.endswith(".rs"):
        parts[-1] = last[:-3]
    else:
        return ""
    return "::".join(parts)


def prune_filters(filters: Iterable[str]) -> list[str]:
    """Drop filters already covered by a shorter module prefix."""
    unique = sorted(set(filters), key=len)
    if "" in unique:
        return [""]
    kept: list[str] = []
    for candidate in unique:
        if any(candidate == item or candidate.startswith(item + "::") for item in kept):
            continue
        kept.append(candidate)
    return sorted(kept)


def test_command(
    package: Package,
    target: Target,
    filter_text: str,
    feature_overrides: dict[str, set[str]],
) -> Optional[list[str]]:
    kind = target_kind(target)
    if kind == "custom-build" or not target.runs_tests:
        return None
    if kind == "bench" or (kind == "bin" and "bench" in target.name.lower()):
        return None
    command = ["cargo", "test", "--quiet", "-p", package.name]
    features = set(feature_overrides.get(package.name, set()))
    features.update(DEFAULT_FEATURES.get(package.name, set()))
    if features:
        command.extend(["--features", ",".join(sorted(features))])
    if kind == "lib":
        command.append("--lib")
    elif kind == "bin":
        command.extend(["--bin", target.name])
    elif kind == "test":
        command.extend(["--test", target.name])
    elif kind == "example":
        command.extend(["--example", target.name])
    else:
        return None
    if filter_text:
        command.append(filter_text)
    return command


def check_command(package: Package, feature_overrides: dict[str, set[str]]) -> list[str]:
    command = ["cargo", "check", "--quiet", "--tests", "-p", package.name]
    features = set(feature_overrides.get(package.name, set()))
    features.update(DEFAULT_FEATURES.get(package.name, set()))
    if features:
        command.extend(["--features", ",".join(sorted(features))])
    return command


def git_changed_files(base: str, include_untracked: bool) -> list[Path]:
    completed = run(
        [
            "git",
            "diff",
            "--name-only",
            "-z",
            "--diff-filter=ACMRTUXB",
            base,
            "--",
        ],
        check=False,
    )
    if completed.returncode != 0:
        sys.stderr.write(completed.stderr)
        sys.exit(completed.returncode)

    names = [name for name in completed.stdout.split("\0") if name]
    if include_untracked:
        untracked = run(
            ["git", "ls-files", "--others", "--exclude-standard", "-z"],
            check=False,
        )
        if untracked.returncode != 0:
            sys.stderr.write(untracked.stderr)
            sys.exit(untracked.returncode)
        names.extend(name for name in untracked.stdout.split("\0") if name)

    paths: list[Path] = []
    seen: set[Path] = set()
    for name in names:
        path = (REPO_ROOT / name).resolve()
        if path in seen:
            continue
        seen.add(path)
        paths.append(path)
    return paths


def explicit_paths(arguments: Iterable[str]) -> list[Path]:
    paths: list[Path] = []
    seen: set[Path] = set()
    for argument in arguments:
        path = Path(argument)
        if path.is_absolute():
            path = path.resolve()
        else:
            path = (Path.cwd() / path).resolve()
            if not path.exists() and (REPO_ROOT / argument).exists():
                path = (REPO_ROOT / argument).resolve()
        candidates = (
            (
                candidate
                for candidate in path.rglob("*")
                if candidate.suffix == ".rs" or candidate.name == "Cargo.toml"
            )
            if path.is_dir()
            else (path,)
        )
        for candidate in candidates:
            if not candidate.is_file() or candidate in seen:
                continue
            seen.add(candidate)
            paths.append(candidate)
    return paths


def reverse_dependents(metadata: dict) -> dict[str, set[str]]:
    workspace_ids = set(metadata.get("workspace_members", []))
    reverse: dict[str, set[str]] = {}
    resolve = metadata.get("resolve") or {}
    for node in resolve.get("nodes", []):
        if node.get("id") not in workspace_ids:
            continue
        for dependency in node.get("deps", []):
            dependency_id = dependency.get("pkg")
            if dependency_id is None:
                continue
            reverse.setdefault(dependency_id, set()).add(node["id"])
    return reverse


def parse_feature_overrides(values: Iterable[str]) -> dict[str, set[str]]:
    overrides: dict[str, set[str]] = {}
    for value in values:
        if "=" not in value:
            sys.exit(f"--features expects PKG=FEAT1,FEAT2, got {value!r}")
        package, features = value.split("=", 1)
        overrides.setdefault(package.strip(), set()).update(
            feature.strip() for feature in features.split(",") if feature.strip()
        )
    return overrides


def format_command(argv: list[str], memguard: bool) -> str:
    if memguard:
        argv = [str(MEMGUARD), *argv]
    return shlex.join(argv)


def main(argv: Optional[list[str]] = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "paths",
        nargs="*",
        help="changed files or directories; default is git diff against --base",
    )
    parser.add_argument(
        "--base",
        default="HEAD",
        help="git revision to diff against (default: HEAD)",
    )
    parser.add_argument(
        "--no-untracked",
        action="store_true",
        help="ignore untracked files when deriving changes",
    )
    parser.add_argument(
        "--no-neighbors",
        action="store_true",
        help="skip cargo check --tests for direct workspace dependents",
    )
    parser.add_argument(
        "--no-memguard",
        action="store_true",
        help="run cargo directly instead of scripts/run-tests-memguard.sh",
    )
    parser.add_argument(
        "--features",
        action="append",
        default=[],
        metavar="PKG=FEAT1,FEAT2",
        help="extra cargo features per package (repeatable)",
    )
    parser.add_argument(
        "--list",
        "--dry-run",
        dest="dry_run",
        action="store_true",
        help="print selected commands without running them",
    )
    args = parser.parse_args(argv)

    feature_overrides = parse_feature_overrides(args.features)
    metadata = load_metadata()
    packages = load_packages(metadata)
    package_by_id = {package.id: package for package in packages}

    if args.paths:
        changed = explicit_paths(args.paths)
    else:
        changed = git_changed_files(args.base, not args.no_untracked)

    tests: dict[tuple[str, str, str, str], tuple[Package, Target, str]] = {}
    changed_packages: set[str] = set()
    manifest_packages: set[str] = set()
    build_script_packages: set[str] = set()
    skipped_targets: set[tuple[str, str, str]] = set()
    unmapped: list[Path] = []

    for path in changed:
        if not path.exists():
            continue
        if not path.is_file():
            continue
        if path.suffix != ".rs" and path.name != "Cargo.toml":
            continue

        package = package_for_path(path, packages)
        if package is None:
            unmapped.append(path)
            continue

        if path == package.manifest:
            manifest_packages.add(package.id)
            changed_packages.add(package.id)
            continue

        if any(
            target_kind(target) == "custom-build" and path == target.src_path
            for target in package.targets
        ):
            build_script_packages.add(package.id)
            changed_packages.add(package.id)
            continue

        if path.suffix != ".rs":
            continue

        changed_packages.add(package.id)
        target = select_target(package, path)
        if target is None:
            # Custom crate layouts can hide module ownership. Fall back to the
            # package lib, or its first bin, instead of silently skipping.
            fallback = next(
                (item for item in package.targets if target_kind(item) == "lib"),
                None,
            )
            if fallback is None:
                fallback = next(
                    (item for item in package.targets if target_kind(item) == "bin"),
                    None,
                )
            if fallback is None:
                unmapped.append(path)
                continue
            target = fallback
            filter_text = ""
        else:
            filter_text = module_filter(target, path)

        kind = target_kind(target)
        if (
            kind == "bench"
            or not target.runs_tests
            or (kind == "bin" and "bench" in target.name.lower())
        ):
            skipped_targets.add((package.name, target.name, kind))
            continue

        tests[(package.id, kind, target.name, filter_text)] = (
            package,
            target,
            filter_text,
        )

    test_commands: list[list[str]] = []
    grouped: dict[tuple[str, str, str], list[tuple[Package, Target, str]]] = {}
    for item in tests.values():
        package, target, _ = item
        grouped.setdefault(
            (package.id, target_kind(target), target.name),
            [],
        ).append(item)
    for items in grouped.values():
        filters = prune_filters(item[2] for item in items)
        package, target, _ = items[0]
        for filter_text in filters:
            command = test_command(package, target, filter_text, feature_overrides)
            if command is not None:
                test_commands.append(command)

    neighbor_ids: set[str] = set()
    if not args.no_neighbors and changed_packages:
        reverse = reverse_dependents(metadata)
        for package_id in changed_packages:
            neighbor_ids.update(reverse.get(package_id, set()))
        neighbor_ids.difference_update(changed_packages)
    neighbor_commands = [
        check_command(package_by_id[package_id], feature_overrides)
        for package_id in sorted(neighbor_ids, key=lambda item: package_by_id[item].name)
        if package_id in package_by_id
    ]

    test_commands.sort()
    neighbor_commands.sort()

    if manifest_packages:
        for package_id in sorted(manifest_packages, key=lambda item: package_by_id[item].name):
            print(
                f"warning: {package_by_id[package_id].name} Cargo.toml changed; "
                "full package test is a manual gate",
                file=sys.stderr,
            )
    if build_script_packages:
        for package_id in sorted(build_script_packages, key=lambda item: package_by_id[item].name):
            print(
                f"warning: {package_by_id[package_id].name} build.rs changed; "
                f"run cargo check -p {package_by_id[package_id].name} --all-targets manually",
                file=sys.stderr,
            )
    for package_name, target_name, kind in sorted(skipped_targets):
        print(
            f"warning: skipped {kind} target {package_name}:{target_name}; "
            "run its benchmark or manual check explicitly",
            file=sys.stderr,
        )
    for path in unmapped:
        print(f"warning: no workspace test target for {path}", file=sys.stderr)

    if not test_commands and not neighbor_commands:
        print("no affected Rust tests or dependents found")
        return 0

    memguard = not args.no_memguard
    if not args.dry_run and memguard and not MEMGUARD.exists():
        sys.exit(f"missing memory guard: {MEMGUARD}")

    if test_commands:
        print("affected tests:", flush=True)
        for command in test_commands:
            print(f"  {format_command(command, memguard)}", flush=True)
    if neighbor_commands:
        print("direct dependent checks:", flush=True)
        for command in neighbor_commands:
            print(f"  {format_command(command, memguard)}", flush=True)

    if args.dry_run:
        return 0

    for command in [*test_commands, *neighbor_commands]:
        full = [str(MEMGUARD), *command] if memguard else command
        result = subprocess.run(full, cwd=REPO_ROOT)
        if result.returncode != 0:
            print(
                f"FAILED: {shlex.join(command)} ({result.returncode})",
                file=sys.stderr,
            )
            return result.returncode

    print(
        f"affected tests passed: {len(test_commands)} test command(s), "
        f"{len(neighbor_commands)} dependent check(s)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
