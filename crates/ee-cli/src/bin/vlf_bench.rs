use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;

use ee_cli::perf::{measure_open_to_first_render, measure_vlf_page_down};
use ee_cli::vlf_bench_support::{
    FixtureMeta, FixtureSpec, ONE_MIB, PAGE_DOWN_TIMEOUT, build_fixture, default_fixture_dir,
    measure_goto, measure_search,
};
use serde::Serialize;

#[derive(Debug, Serialize)]
struct SuiteResult {
    fixture: String,
    size_bytes: u64,
    total_lines: u64,
    open_to_first_render_ms: u128,
    open_ms: u128,
    first_draw_ms: u128,
    page_down_cold_ms: u128,
    goto_before_index_ms: u128,
    goto_before_index_kind: String,
    goto_before_index_byte: Option<u64>,
    goto_after_index_ms: u128,
    goto_after_index_byte: Option<u64>,
    index_completion_ms: u128,
    search_cancel_latency_ms: u128,
    search_cancel_batches: usize,
}

#[derive(Debug)]
struct Config {
    json: bool,
    keep_fixtures: bool,
    fixture_dir: Option<PathBuf>,
}

fn main() -> io::Result<()> {
    let config = parse_args()?;
    let fixture_root =
        config.fixture_dir.clone().unwrap_or_else(|| default_fixture_dir("ee-vlf-bench"));
    fs::create_dir_all(&fixture_root)?;

    let specs = [
        FixtureSpec { label: "100mb", size_bytes: 100 * ONE_MIB },
        FixtureSpec { label: "1gb", size_bytes: 1024 * ONE_MIB },
        FixtureSpec { label: "10gb", size_bytes: 10 * 1024 * ONE_MIB },
    ];

    let mut fixtures = Vec::with_capacity(specs.len());
    for spec in specs {
        fixtures.push(build_fixture(spec, &fixture_root)?);
    }

    let mut results = Vec::with_capacity(fixtures.len());
    for fixture in &fixtures {
        results.push(run_suite(fixture)?);
    }

    if config.json {
        println!("{}", serde_json::to_string_pretty(&results).map_err(io::Error::other)?);
    } else {
        print_table(&results);
    }

    if !config.keep_fixtures {
        for fixture in fixtures {
            let _ = fs::remove_file(fixture.path);
        }
    }

    Ok(())
}

fn parse_args() -> io::Result<Config> {
    let mut json = false;
    let mut keep_fixtures = false;
    let mut fixture_dir = None;

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--keep-fixtures" => keep_fixtures = true,
            "--fixture-dir" => {
                let Some(value) = args.next() else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--fixture-dir requires a path",
                    ));
                };
                fixture_dir = Some(PathBuf::from(value));
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown argument: {other}"),
                ));
            }
        }
    }

    Ok(Config { json, keep_fixtures, fixture_dir })
}

fn print_help() {
    println!(
        "vlf_bench\n\nUSAGE:\n  cargo run -p ee-cli --bin vlf_bench --release [-- --json] [--keep-fixtures] [--fixture-dir DIR]\n\nRuns one-shot VLF integration probes for 100 MB, 1 GB, and 10 GB fixtures. Focus: cold open-to-first-render, cold page-down, approximate/exact goto-line, and search cancellation latency. Warm steady-state benches live under `cargo bench -p ee-cli --bench vlf`."
    );
}

fn run_suite(fixture: &FixtureMeta) -> io::Result<SuiteResult> {
    let open = measure_open_to_first_render(&fixture.path)?;
    let page_down = measure_vlf_page_down(&fixture.path, PAGE_DOWN_TIMEOUT)?;

    let before = measure_goto(&fixture.path, fixture.total_lines, false)?;
    let after = measure_goto(&fixture.path, fixture.total_lines, true)?;

    let cancel = measure_search(&fixture.path, true)?;

    Ok(SuiteResult {
        fixture: fixture.spec.label.to_owned(),
        size_bytes: fixture.spec.size_bytes,
        total_lines: fixture.total_lines,
        open_to_first_render_ms: open.total.as_millis(),
        open_ms: open.open.as_millis(),
        first_draw_ms: open.draw.as_millis(),
        page_down_cold_ms: page_down.cold.as_millis(),
        goto_before_index_ms: before.lookup_elapsed.as_millis(),
        goto_before_index_kind: before.kind,
        goto_before_index_byte: before.byte_offset,
        goto_after_index_ms: after.lookup_elapsed.as_millis(),
        goto_after_index_byte: after.byte_offset,
        index_completion_ms: after.index_elapsed.as_millis(),
        search_cancel_latency_ms: cancel.cancel_elapsed.unwrap_or_default().as_millis(),
        search_cancel_batches: cancel.batches,
    })
}

fn print_table(results: &[SuiteResult]) {
    println!(
        "fixture,size,open_ms,draw_ms,total_ms,page_down_cold_ms,goto_before_ms,goto_before_kind,goto_after_ms,index_ms,cancel_ms,cancel_batches"
    );
    for result in results {
        println!(
            "{},{},{},{},{},{},{},{},{},{},{},{}",
            result.fixture,
            result.size_bytes,
            result.open_ms,
            result.first_draw_ms,
            result.open_to_first_render_ms,
            result.page_down_cold_ms,
            result.goto_before_index_ms,
            result.goto_before_index_kind,
            result.goto_after_index_ms,
            result.index_completion_ms,
            result.search_cancel_latency_ms,
            result.search_cancel_batches,
        );
    }
}
