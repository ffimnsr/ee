// Copyright 2017 The xi-editor Authors.
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
use criterion::{Criterion, criterion_group, criterion_main};
use xi_rope::rope::Rope;

fn build_triangle(n: usize) -> String {
    let mut s = String::new();
    let mut line = String::new();
    for _ in 0..n {
        s += &line;
        s += "\n";
        line += "a";
    }
    s
}

fn build_short_lines(n: usize) -> String {
    let line = "match s.as_bytes()[minsplit - 1..splitpoint].iter().rposition(|&c| c == b'\n') {";
    let mut s = String::new();
    for _ in 0..n {
        s += line;
    }
    s
}

fn build_few_big_lines(size: usize) -> String {
    let mut s = String::with_capacity(size * 10 + 20);
    for _ in 0..10 {
        for _ in 0..size {
            s += "a";
        }
        s += "\n";
    }
    s
}

fn benchmark_file_load_short_lines(c: &mut Criterion) {
    let text = build_short_lines(50_000);
    c.bench_function("benchmark_file_load_short_lines", |b| {
        b.iter(|| {
            let _ = Rope::from(&text);
        })
    });
}

fn benchmark_file_load_few_big_lines(c: &mut Criterion) {
    let text = build_few_big_lines(1_000_000);
    c.bench_function("benchmark_file_load_few_big_lines", |b| {
        b.iter(|| {
            let _ = Rope::from(&text);
        })
    });
}

fn benchmark_char_insertion_one_line_edit(c: &mut Criterion) {
    let mut text = Rope::from("b".repeat(100));
    let mut offset = 100;
    c.bench_function("benchmark_char_insertion_one_line_edit", |b| {
        b.iter(|| {
            text.edit(offset..=offset, "a");
            offset += 1;
        })
    });
}

fn benchmark_paste_into_line(c: &mut Criterion) {
    let mut text = Rope::from(build_short_lines(50_000));
    let insertion = "a".repeat(50);
    let mut offset = 100;
    c.bench_function("benchmark_paste_into_line", |b| {
        b.iter(|| {
            text.edit(offset..=offset, &insertion);
            offset += 150;
        })
    });
}

fn benchmark_insert_newline(c: &mut Criterion) {
    let mut text = Rope::from(build_few_big_lines(1_000_000));
    let mut offset = 1000;
    c.bench_function("benchmark_insert_newline", |b| {
        b.iter(|| {
            text.edit(offset..=offset, "\n");
            offset += 1001;
        })
    });
}

fn benchmark_overwrite_into_line(c: &mut Criterion) {
    let mut text = Rope::from(build_short_lines(50_000));
    let mut offset = 100;
    let insertion = "a".repeat(50);
    c.bench_function("benchmark_overwrite_into_line", |b| {
        b.iter(|| {
            text.edit(offset..=offset + 20, &insertion);
            offset += 30;
        })
    });
}

fn benchmark_triangle_concat_inplace(c: &mut Criterion) {
    let mut text = Rope::from("");
    let insertion = build_triangle(3000);
    let insertion_len = insertion.len();
    let mut offset = 0;
    c.bench_function("benchmark_triangle_concat_inplace", |b| {
        b.iter(|| {
            text.edit(offset..=offset, &insertion);
            offset += insertion_len;
        })
    });
}

fn real_world_editing_scenario(c: &mut Criterion) {
    c.bench_function("real_world_editing_scenario", |b| {
        b.iter(|| {
            let mut text = Rope::default();
            let mut cursor = 0;
            for i in 1..10_000 {
                let s = if i % 80 == 0 { "\n" } else { "a" };
                text.edit(cursor..cursor, s);
                if i % 123 == 0 {
                    text.edit(cursor - 5..cursor, "");
                }

                cursor = match i {
                    1000 => 200,
                    2000 => 1800,
                    3000 => 1000,
                    4000 => text.len() - 1,
                    5000 => 404,
                    6000 => 4444,
                    7000 => 6990,
                    8000 => 6990,
                    9000 => 100,
                    n if n % 123 == 0 => cursor - 5,
                    _ => cursor + 1,
                };
            }
        })
    });
}

criterion_group!(
    benches,
    benchmark_file_load_short_lines,
    benchmark_file_load_few_big_lines,
    benchmark_char_insertion_one_line_edit,
    benchmark_paste_into_line,
    benchmark_insert_newline,
    benchmark_overwrite_into_line,
    benchmark_triangle_concat_inplace,
    real_world_editing_scenario
);
criterion_main!(benches);
