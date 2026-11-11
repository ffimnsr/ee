#!/usr/bin/env python3
"""Unit tests for scripts/run-affected-tests.py helpers.

Run with: python3 scripts/tests/test-run-affected-tests.py
"""

import importlib.util
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT = REPO_ROOT / "scripts" / "run-affected-tests.py"

spec = importlib.util.spec_from_file_location("run_affected_tests", SCRIPT)
assert spec is not None and spec.loader is not None
affected = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = affected
spec.loader.exec_module(affected)

Target = affected.Target
Package = affected.Package


def target(
    name: str,
    kinds: tuple[str, ...],
    src_path: str,
    root_dir: str,
    *,
    runs_tests: bool = True,
) -> Target:
    return Target(
        name=name,
        kinds=kinds,
        src_path=Path(src_path),
        root_dir=Path(root_dir),
        runs_tests=runs_tests,
    )


def package(name: str, targets: tuple[Target, ...]) -> Package:
    root = Path("/tmp") / name
    return Package(
        id=name,
        name=name,
        root=root,
        manifest=root / "Cargo.toml",
        targets=targets,
    )


def test_target_roots() -> None:
    assert affected.target_root(Path("src/lib.rs"), {"lib"}) == Path("src")
    assert affected.target_root(Path("src/main.rs"), {"bin"}) == Path("src")
    assert affected.target_root(Path("src/bin/foo.rs"), {"bin"}) == Path("src/bin/foo")
    assert affected.target_root(Path("tests/foo/main.rs"), {"test"}) == Path("tests/foo")
    assert affected.target_root(Path("tests/foo.rs"), {"test"}) == Path("tests/foo")


def test_module_filter() -> None:
    lib = target("demo", ("lib",), "src/lib.rs", "src")
    assert affected.module_filter(lib, Path("src/lib.rs")) == ""
    assert affected.module_filter(lib, Path("src/foo.rs")) == "foo"
    assert affected.module_filter(lib, Path("src/foo/bar.rs")) == "foo::bar"
    assert affected.module_filter(lib, Path("src/foo/mod.rs")) == "foo"

    bin_target = target("demo", ("bin",), "src/bin/foo.rs", "src/bin/foo")
    assert affected.module_filter(bin_target, Path("src/bin/foo.rs")) == ""
    assert affected.module_filter(bin_target, Path("src/bin/foo/bar.rs")) == "bar"

    test_target = target("foo", ("test",), "tests/foo.rs", "tests/foo")
    assert affected.module_filter(test_target, Path("tests/foo.rs")) == ""
    assert affected.module_filter(test_target, Path("tests/foo/bar.rs")) == "bar"


def test_prune_filters() -> None:
    assert affected.prune_filters(["foo::bar", "foo", "baz"]) == ["baz", "foo"]
    assert affected.prune_filters(["foo", "foobar"]) == ["foo", "foobar"]
    assert affected.prune_filters(["foo::bar", ""]) == [""]


def test_select_target_prefers_exact_and_lib() -> None:
    lib_target = target("demo", ("lib",), "src/lib.rs", "src")
    main_target = target("demo", ("bin",), "src/main.rs", "src")
    pkg = package("demo", (lib_target, main_target))

    assert affected.select_target(pkg, Path("src/foo.rs")) == lib_target
    assert affected.select_target(pkg, Path("src/main.rs")) == main_target

    bin_target = target("bar", ("bin",), "src/bin/bar.rs", "src/bin/bar")
    pkg = package("demo", (lib_target, bin_target))
    assert affected.select_target(pkg, Path("src/bin/bar.rs")) == bin_target
    assert affected.select_target(pkg, Path("src/bin/bar/module.rs")) == bin_target

    test_target = target("flow", ("test",), "tests/flow.rs", "tests/flow")
    pkg = package("demo", (lib_target, test_target))
    assert affected.select_target(pkg, Path("tests/flow/case.rs")) == test_target


def test_test_command_features_and_skips() -> None:
    lib_target = target("ee", ("lib",), "src/lib.rs", "src")
    pkg = package("ee-cli", (lib_target,))
    command = affected.test_command(pkg, lib_target, "app::agent_pane", {})
    assert command == [
        "cargo",
        "test",
        "--quiet",
        "-p",
        "ee-cli",
        "--features",
        "agents",
        "--lib",
        "app::agent_pane",
    ]

    bench_target = target("vlf", ("bench",), "benches/vlf.rs", "benches/vlf")
    assert affected.test_command(pkg, bench_target, "", {}) is None

    no_tests = target("helper", ("lib",), "src/lib.rs", "src", runs_tests=False)
    assert affected.test_command(pkg, no_tests, "", {}) is None

    bench_bin = target("vlf_bench", ("bin",), "src/bin/vlf_bench.rs", "src/bin/vlf_bench")
    assert affected.test_command(pkg, bench_bin, "", {}) is None


def main() -> None:
    tests = sorted(
        (name, value)
        for name, value in globals().items()
        if name.startswith("test_") and callable(value)
    )
    for name, test in tests:
        test()
        print(f"ok {name}")
    print(f"{len(tests)} helper tests passed")


if __name__ == "__main__":
    main()
