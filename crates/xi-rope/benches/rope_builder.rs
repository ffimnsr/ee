use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use xi_rope::RopeBuilder;

const TARGET_BYTES_20_MIB: usize = 20 * 1024 * 1024;

fn clamp_to_char_boundary(text: &str, splitpoint: usize) -> usize {
    let mut splitpoint = splitpoint.min(text.len());
    while splitpoint > 0 && !text.is_char_boundary(splitpoint) {
        splitpoint -= 1;
    }
    splitpoint
}

fn repeat_to_target(seed: &str, target_bytes: usize) -> String {
    let mut text = String::with_capacity(target_bytes + seed.len());
    while text.len() + seed.len() <= target_bytes {
        text.push_str(seed);
    }
    if text.len() < target_bytes {
        let end = clamp_to_char_boundary(seed, target_bytes - text.len());
        text.push_str(&seed[..end]);
    }
    text
}

fn fixture_many_line_20_mib() -> String {
    repeat_to_target(
        "fn render_row(idx: usize) -> &'static str { \"abcdefghijklmnopqrstuvwxyz0123456789\" }\n",
        TARGET_BYTES_20_MIB,
    )
}

fn fixture_long_line_20_mib() -> String {
    let mut text = String::with_capacity(TARGET_BYTES_20_MIB + 1);
    text.push_str(&"x".repeat(TARGET_BYTES_20_MIB - 1));
    text.push('\n');
    text
}

fn fixture_mixed_utf8_crlf_20_mib() -> String {
    repeat_to_target("αβγ🙂delta\r\nplain-ascii-line\n終わりと🙂emoji\r\n", TARGET_BYTES_20_MIB)
}

fn bench_builder_load(c: &mut Criterion, name: &str, text: &str) {
    let mut group = c.benchmark_group("rope_builder");
    group.throughput(Throughput::Bytes(text.len() as u64));
    group.bench_function(name, |b| {
        b.iter(|| {
            let mut builder = RopeBuilder::new();
            builder.push_str(black_box(text));
            black_box(builder.finish());
        });
    });
    group.finish();
}

fn benchmark_rope_builder_20mib_many_line(c: &mut Criterion) {
    let text = fixture_many_line_20_mib();
    bench_builder_load(c, "benchmark_rope_builder_20mib_many_line", &text);
}

fn benchmark_rope_builder_20mib_long_line(c: &mut Criterion) {
    let text = fixture_long_line_20_mib();
    bench_builder_load(c, "benchmark_rope_builder_20mib_long_line", &text);
}

fn benchmark_rope_builder_20mib_mixed_utf8_crlf(c: &mut Criterion) {
    let text = fixture_mixed_utf8_crlf_20_mib();
    bench_builder_load(c, "benchmark_rope_builder_20mib_mixed_utf8_crlf", &text);
}

criterion_group!(
    benches,
    benchmark_rope_builder_20mib_many_line,
    benchmark_rope_builder_20mib_long_line,
    benchmark_rope_builder_20mib_mixed_utf8_crlf
);
criterion_main!(benches);
