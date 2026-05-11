use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};

use ee_tui::perf::{measure_open_to_first_render, measure_vlf_page_down};
use ee_tui::vlf_bench_support::{
    FixtureMeta, FixtureSpec, ONE_MIB, PAGE_DOWN_TIMEOUT, build_fixture, default_fixture_dir,
    measure_search,
};

fn warm_fixture_100mb() -> &'static FixtureMeta {
    static FIXTURE: OnceLock<FixtureMeta> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let root = default_fixture_dir("ee-vlf-criterion");
        fs::create_dir_all(&root).expect("create criterion fixture dir");
        build_fixture(FixtureSpec { label: "100mb-warm", size_bytes: 100 * ONE_MIB }, &root)
            .expect("build criterion fixture")
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

criterion_group!(
    benches,
    bench_warm_open_to_first_render,
    bench_warm_page_down,
    bench_streaming_search_throughput
);
criterion_main!(benches);
