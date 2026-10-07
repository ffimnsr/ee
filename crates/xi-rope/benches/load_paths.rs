// Copyright 2018 The xi-editor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Load-path benches for the rope: whole-text construction (the exact shape
//! the editor open path uses) and per-leaf metric computation.
//!
//! These complement `rope_builder.rs` by measuring `Rope::from(&str)` end to
//! end (builder + metrics + tree build) at sizes the open-path probe reports
//! as dominant, plus the three metric passes (`RopeInfo::compute_info`) that
//! run once per leaf.

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use xi_rope::tree::NodeInfo;
use xi_rope::{Rope, RopeInfo};

const TARGET_BYTES_8_MIB: usize = 8 * 1024 * 1024;

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

fn fixture_many_line() -> String {
    repeat_to_target(
        "fn render_row(idx: usize) -> &'static str { \"abcdefghijklmnopqrstuvwxyz0123456789\" }\n",
        TARGET_BYTES_8_MIB,
    )
}

fn fixture_long_line() -> String {
    let mut text = String::with_capacity(TARGET_BYTES_8_MIB + 1);
    text.push_str(&"x".repeat(TARGET_BYTES_8_MIB - 1));
    text.push('\n');
    text
}

fn fixture_mixed_utf8_crlf() -> String {
    repeat_to_target("αβγ🙂delta\r\nplain-ascii-line\n終わりと🙂emoji\r\n", TARGET_BYTES_8_MIB)
}

fn bench_rope_from_str(c: &mut Criterion, name: &str, text: &str) {
    let mut group = c.benchmark_group("rope_from_str");
    group.throughput(Throughput::Bytes(text.len() as u64));
    // Large fixtures: 20 samples keeps the whole bench well under a minute.
    group.sample_size(20);
    group.bench_function(name, |b| {
        b.iter(|| black_box(Rope::from(black_box(text))));
    });
    group.finish();
}

/// One leaf's worth of bytes, exactly the granularity `compute_info` runs at.
fn leaf_bytes(seed: &str, target: usize) -> String {
    repeat_to_target(seed, target)
}

#[allow(clippy::ptr_arg)] // `compute_info` takes `&String` (the tree leaf type)
fn bench_compute_info(c: &mut Criterion, name: &str, leaf: &String) {
    let mut group = c.benchmark_group("rope_info_compute");
    group.throughput(Throughput::Bytes(leaf.len() as u64));
    group.sample_size(30);
    group.bench_function(name, |b| {
        b.iter(|| black_box(<RopeInfo as NodeInfo>::compute_info(black_box(leaf))));
    });
    group.finish();
}

/// The full metric pass over an 8 MiB buffer split into 1 KiB leaves, summing
/// into one `RopeInfo` — what the real `RopeBuilder` does per flushed leaf.
fn bench_metrics_whole_buffer(c: &mut Criterion, name: &str, text: &str) {
    // Split like the real builder does: ~1 KiB leaves, clamped to char
    // boundaries so multi-byte sequences are never cut.
    let mut leaves: Vec<String> = Vec::new();
    let mut offset = 0;
    while offset < text.len() {
        let mut end = (offset + 1024).min(text.len());
        while end > offset && !text.is_char_boundary(end) {
            end -= 1;
        }
        leaves.push(text[offset..end].to_owned());
        offset = end;
    }
    let mut group = c.benchmark_group("rope_metrics_whole");
    group.throughput(Throughput::Bytes(leaves.len() as u64 * 1024));
    group.sample_size(20);
    group.bench_function(name, |b| {
        b.iter(|| {
            let mut acc = RopeInfo::identity();
            for leaf in black_box(&leaves) {
                acc.accumulate(&<RopeInfo as NodeInfo>::compute_info(leaf));
            }
            black_box(acc)
        });
    });
    group.finish();
}

fn bench_rope_from_owned(c: &mut Criterion, name: &str, text: &str) {
    let mut group = c.benchmark_group("rope_from_owned");
    group.throughput(Throughput::Bytes(text.len() as u64));
    group.sample_size(20);
    group.bench_function(name, |b| {
        // The open path owns the decoded string, so the clone here stands in
        // for the read buffer; every other cost is the owned construction.
        b.iter(|| {
            let owned = black_box(text.to_owned());
            black_box(Rope::from_owned(owned))
        });
    });
    group.finish();
}

fn benchmark_rope_from_str(c: &mut Criterion) {
    let many_line = fixture_many_line();
    bench_rope_from_str(c, "many_line_8mib", &many_line);
    let long_line = fixture_long_line();
    bench_rope_from_str(c, "long_line_8mib", &long_line);
    let mixed = fixture_mixed_utf8_crlf();
    bench_rope_from_str(c, "mixed_utf8_crlf_8mib", &mixed);
}

fn benchmark_rope_from_owned(c: &mut Criterion) {
    let many_line = fixture_many_line();
    bench_rope_from_owned(c, "many_line_8mib", &many_line);
    let long_line = fixture_long_line();
    bench_rope_from_owned(c, "long_line_8mib", &long_line);
    let mixed = fixture_mixed_utf8_crlf();
    bench_rope_from_owned(c, "mixed_utf8_crlf_8mib", &mixed);
}

fn benchmark_rope_info_compute(c: &mut Criterion) {
    // A leaf's metric cost depends on content: ASCII code-like text vs a
    // mixed UTF-8 line with CRLF endings.
    let ascii_leaf = leaf_bytes("fn render_row(idx: usize) -> usize { idx }\n", 1024);
    bench_compute_info(c, "ascii_1kib", &ascii_leaf);
    let mixed_leaf = leaf_bytes("αβγ🙂delta\r\nplain-line\n終わり\r\n", 1024);
    bench_compute_info(c, "mixed_utf8_1kib", &mixed_leaf);
}

fn benchmark_metrics_whole_buffer(c: &mut Criterion) {
    let many_line = fixture_many_line();
    bench_metrics_whole_buffer(c, "ascii_8mib", &many_line);
    let mixed = fixture_mixed_utf8_crlf();
    bench_metrics_whole_buffer(c, "mixed_utf8_8mib", &mixed);
}

criterion_group!(
    benches,
    benchmark_rope_from_str,
    benchmark_rope_from_owned,
    benchmark_rope_info_compute,
    benchmark_metrics_whole_buffer
);
criterion_main!(benches);
