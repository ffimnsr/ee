// Benchmark APIs exercise selected editor paths without linking the CLI entrypoint.
// Runtime-only paths therefore remain intentionally unreachable in this library target.
#[allow(dead_code)]
mod app;
#[allow(dead_code)]
mod backend;
#[allow(dead_code)]
mod buffer;
#[allow(dead_code)]
mod config;
#[allow(dead_code)]
mod folds;
#[allow(dead_code)]
mod git;
#[allow(dead_code)]
mod highlight;
#[allow(dead_code)]
mod keymap;
#[allow(dead_code)]
mod logs;
pub mod perf;
#[allow(dead_code)]
mod picker;
#[allow(dead_code)]
mod policy;
#[allow(dead_code)]
mod quickfix;
#[allow(dead_code)]
mod registers;
#[allow(dead_code)]
mod render_metrics;
#[allow(dead_code)]
mod secrets;
#[allow(dead_code)]
mod session;
#[allow(dead_code)]
mod terminal;
#[allow(dead_code)]
mod text;
#[allow(dead_code)]
mod theme;
#[allow(dead_code)]
mod ui;
pub mod vlf_bench_support;
#[allow(dead_code)]
mod vlf_viewport;
#[allow(dead_code)]
mod window;
