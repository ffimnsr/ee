use std::fs;
use std::hint::black_box;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput};

use ee_cli::perf::{
    measure_open_to_first_render, measure_store_render_positions, measure_vlf_page_down,
    measure_vlf_render_allocations, measure_vlf_render_positions,
};
use ee_cli::vlf_bench_support::{
    FixtureMeta, FixtureSpec, ONE_MIB, PAGE_DOWN_TIMEOUT, RENDER_GATE_ALLOC_BYTES,
    RENDER_GATE_BUDGET, RENDER_GATE_VIEWPORT_LINES, TWO_GIB, build_fixture, default_fixture_dir,
    measure_search,
};

const SETTLE_TIMEOUT: Duration = Duration::from_secs(60);
const GATE_REPETITIONS: usize = 7;

fn warm_fixture_100mb() -> &'static FixtureMeta {
    static FIXTURE: OnceLock<FixtureMeta> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let root = default_fixture_dir("ee-vlf-criterion");
        fs::create_dir_all(&root).expect("create criterion fixture dir");
        build_fixture(FixtureSpec::new("100mb-warm", 100 * ONE_MIB, false), &root)
            .expect("build criterion fixture")
    })
}

/// §4 gate fixture: a 2 GB fully-dense synthetic file (real line text across
/// the whole file — sparse zero holes would read as page-sized lines and
/// distort the line-window measures).
fn gate_fixture_2gib() -> &'static FixtureMeta {
    static FIXTURE: OnceLock<FixtureMeta> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let root = default_fixture_dir("ee-vlf-render-gate");
        fs::create_dir_all(&root).expect("create gate fixture dir");
        build_fixture(FixtureSpec::new("2gib-dense", TWO_GIB, true), &root)
            .expect("build 2 GiB gate fixture")
    })
}

fn bench_warm_open_to_first_render(c: &mut Criterion) {
    let fixture = warm_fixture_100mb();
    let mut group = c.benchmark_group("vlf_warm_open");
    group.throughput(Throughput::Bytes(fixture.spec.size_bytes));
    group.bench_with_input(
        BenchmarkId::new("open_to_first_render", fixture.spec.label),
        &fixture.path,
        |b, path: &PathBuf| {
            b.iter(|| black_box(measure_open_to_first_render(path).expect("measure warm open")));
        },
    );
    group.finish();
}

fn bench_warm_page_down(c: &mut Criterion) {
    let fixture = warm_fixture_100mb();
    let mut group = c.benchmark_group("vlf_warm_navigation");
    group.throughput(Throughput::Bytes(fixture.spec.size_bytes));
    group.bench_with_input(
        BenchmarkId::new("page_down_warm", fixture.spec.label),
        &fixture.path,
        |b, path: &PathBuf| {
            b.iter(|| {
                let metrics =
                    measure_vlf_page_down(path, PAGE_DOWN_TIMEOUT).expect("measure warm page-down");
                black_box(metrics.warm)
            });
        },
    );
    group.finish();
}

fn bench_streaming_search_throughput(c: &mut Criterion) {
    let fixture = warm_fixture_100mb();
    let mut group = c.benchmark_group("vlf_warm_search");
    group.throughput(Throughput::Bytes(fixture.spec.size_bytes));
    group.bench_with_input(
        BenchmarkId::new("streaming_search_full_scan", fixture.spec.label),
        &fixture.path,
        |b, path: &PathBuf| {
            b.iter(|| {
                let metrics = measure_search(path, false).expect("measure streaming search");
                black_box((metrics.scanned_bytes, metrics.elapsed))
            });
        },
    );
    group.finish();
}

/// §4 acceptance gate (plan §6 item 4): per-render < 16 ms at head / middle /
/// tail of the 2 GB file, same bound for the index-behind (`Pending`) reply,
/// and per-render allocations window-bounded and flat across positions.
/// Panics (non-zero exit) on violation; prints the report otherwise.
///
/// The budget is asserted on the store-level per-render work (§4: decode +
/// index + syntax). The app-level end-to-end numbers are reported too — they
/// include the transport polls (core/reader channel quanta) that are a
/// property of the bench harness, not the render cost.
fn run_gate() -> Result<(), String> {
    let fixture = gate_fixture_2gib();
    eprintln!("gate: 2 GiB dense fixture at {}", fixture.path.display());

    let core_metrics =
        measure_store_render_positions(&fixture.path, RENDER_GATE_VIEWPORT_LINES, SETTLE_TIMEOUT)
            .map_err(|err| format!("store render-position measure failed: {err}"))?;

    let mut failures = Vec::new();
    for (label, sample) in [
        ("pending (index behind)", core_metrics.pending),
        ("head", core_metrics.head),
        ("middle", core_metrics.middle),
        ("tail", core_metrics.tail),
    ] {
        if sample > RENDER_GATE_BUDGET {
            failures.push(format!("{label}: {sample:?} > budget {RENDER_GATE_BUDGET:?}"));
        }
    }

    // Informational: end-to-end latency through the app (includes transport
    // polls). Same positions, same window.
    let e2e = measure_vlf_render_positions(
        &fixture.path,
        SETTLE_TIMEOUT,
        RENDER_GATE_VIEWPORT_LINES,
        GATE_REPETITIONS,
    )
    .ok();

    let allocs =
        measure_vlf_render_allocations(&fixture.path, SETTLE_TIMEOUT, RENDER_GATE_VIEWPORT_LINES)
            .map_err(|err| format!("render-alloc measure failed: {err}"))?;
    let [head_alloc, middle_alloc, tail_alloc] = allocs.samples().map(|(_, bytes)| bytes);
    for (label, bytes) in allocs.samples() {
        if bytes > RENDER_GATE_ALLOC_BYTES {
            failures.push(format!("{label} alloc: {bytes} B > cap {RENDER_GATE_ALLOC_BYTES} B"));
        }
    }
    // Flat across positions: file-size-proportional work would grow with the
    // distance from head. Compare the two real renders (middle vs tail); the
    // head sample is a cached no-op window (the settling renders before it
    // already built the payloads) and its alloc baseline is degenerate.
    if middle_alloc > tail_alloc.saturating_mul(3) || tail_alloc > middle_alloc.saturating_mul(3) {
        failures.push(format!(
            "alloc not flat across positions: head {head_alloc} B, middle {middle_alloc} B, \
             tail {tail_alloc} B"
        ));
    }

    println!(
        "\nVLF §4 render gate ({}-line window, {:.1} GiB, {} lines, index scan {:.1}s):\n\
         \x20 pending (index behind): {:?}   core {:?}\n\
         \x20 head:                  {:?}   core {:?}\n\
         \x20 middle:                {:?}   core {:?}\n\
         \x20 tail:                  {:?}   core {:?}\n\
         \x20 alloc peak  head:      {} B\n\
         \x20 alloc peak  middle:    {} B\n\
         \x20 alloc peak  tail:      {} B\n",
        core_metrics.viewport_lines,
        fixture.spec.size_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
        core_metrics.total_lines,
        core_metrics.index_wait.as_secs_f64(),
        e2e.as_ref().map(|m| m.pending).unwrap_or_default(),
        core_metrics.pending,
        e2e.as_ref().map(|m| m.head).unwrap_or_default(),
        core_metrics.head,
        e2e.as_ref().map(|m| m.middle).unwrap_or_default(),
        core_metrics.middle,
        e2e.as_ref().map(|m| m.tail).unwrap_or_default(),
        core_metrics.tail,
        head_alloc,
        middle_alloc,
        tail_alloc,
    );

    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!("VLF §4 render gate VIOLATED:\n- {}", failures.join("\n- ")))
    }
}

fn main() {
    if let Err(violation) = run_gate() {
        eprintln!("{violation}");
        std::process::exit(1);
    }
    let mut criterion = Criterion::default().configure_from_args();
    bench_warm_open_to_first_render(&mut criterion);
    bench_warm_page_down(&mut criterion);
    bench_streaming_search_throughput(&mut criterion);
    criterion.final_summary();
}
