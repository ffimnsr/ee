//! Ignored wall-clock probe for the file-open pipeline.
//!
//! This drives the real end-to-end open path — `CoreState::do_new_view` plus
//! the deferred `finalize_new_views` pass — through a `XiCore` with a
//! recording peer, and reports per-stage wall times from `crate::open_probe`.
//!
//! Run with:
//!
//! ```text
//! cargo test --quiet -p ee-xi-core-lib open_path_perf_probe -- --ignored --nocapture
//! ```
//!
//! Or through the orchestration script, which also supports external profilers:
//!
//! ```text
//! scripts/profile-open.sh
//! scripts/profile-open.sh --tool samply
//! ```
//!
//! Environment knobs:
//!
//! - `EE_OPEN_PROBE_SIZES`: comma-separated MiB sizes (default `1,4,8`;
//!   larger sweeps such as `32` are opt-in via the environment).
//! - `EE_OPEN_PROBE_ITERS`: warm iterations per fixture (default `5`).
//! - `EE_OPEN_PROBE_SHAPES`: comma-separated shapes from `code`, `longline`,
//!   `mixedcrlf` (default all).
//! - `EE_OPEN_PROBE_EXT`: fixture extension (default `txt`; `rs` exercises
//!   syntax-enabled render when the runtime grammars are installed).
//! - `EE_OPEN_PROBE_JSON`: write a machine-readable JSON artifact to this path.
//!
//! Each fixture is opened once cold (right after the fixture is written) and
//! then `EE_OPEN_PROBE_ITERS` times warm. `Render` overlaps `Finalize` because
//! `finish_init` renders inline (see `crate::open_probe` module docs), so the
//! printed stage sum double-counts the initial render by design.

use super::*;
use crate::open_probe::{OpenStage, OpenStageTimings};
use std::env;
use std::path::{Path, PathBuf};
use xi_rope::LinesMetric;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    Code,
    LongLine,
    MixedCrlf,
}

impl Shape {
    fn parse(name: &str) -> Option<Self> {
        match name.trim() {
            "code" => Some(Shape::Code),
            "longline" => Some(Shape::LongLine),
            "mixedcrlf" => Some(Shape::MixedCrlf),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Shape::Code => "code",
            Shape::LongLine => "longline",
            Shape::MixedCrlf => "mixedcrlf",
        }
    }

    fn seed(self) -> &'static str {
        match self {
            Shape::Code => "fn render_row(idx: usize) -> usize { idx * 42 + 7 }\n",
            Shape::LongLine => "x",
            Shape::MixedCrlf => "    αβγ🙂delta\r\n",
        }
    }
}

/// Repeats `seed` until `target` bytes are reached, clamping the final partial
/// copy to a char boundary. `LongLine` also appends the trailing newline.
fn fixture(shape: Shape, target: usize) -> Vec<u8> {
    let seed = shape.seed();
    let body_target = match shape {
        Shape::LongLine => target.saturating_sub(1),
        _ => target,
    };
    let mut out = String::with_capacity(target + seed.len());
    while out.len() + seed.len() <= body_target {
        out.push_str(seed);
    }
    let mut remaining = body_target - out.len();
    while remaining > 0 && !seed.is_char_boundary(remaining) {
        remaining -= 1;
    }
    out.push_str(&seed[..remaining]);
    if shape == Shape::LongLine {
        out.push('\n');
    }
    out.into_bytes()
}

#[derive(Debug, Clone, Copy, Default)]
struct FixtureStats {
    /// Backing mode chosen by the open policy for this fixture.
    mode: &'static str,
    /// Bytes visible through the buffer (rope length or VLF store length).
    bytes: usize,
    /// Logical line count (rope only; VLF reports 1 until indexed).
    lines: usize,
    /// Rope leaf count (0 for VLF).
    leaves: usize,
    /// Bytes of `update` payload produced by the first render.
    update_bytes: usize,
}

fn open_once(path: &Path) -> (OpenStageTimings, FixtureStats) {
    let peer = RecordingPeer::default();
    let rpc_peer: Box<dyn Peer> = Box::new(peer.clone());
    let ctx = RpcCtx::new(rpc_peer.box_clone());
    let mut core = crate::XiCore::new();
    core.handle_notification(
        &ctx,
        crate::rpc::CoreNotification::ClientStarted { config_dir: None, client_extras_dir: None },
    );
    // Startup traffic (plugin/config notifications) is not part of the open.
    peer.take_notifications();

    crate::open_probe::arm();
    let view_id_value =
        core.inner().do_new_view(Some(path.to_path_buf())).expect("open should succeed");
    let view_id: ViewId = serde_json::from_value(view_id_value).expect("view id");
    core.inner().handle_idle(NEW_VIEW_IDLE_TOKEN);
    let timings = crate::open_probe::take().expect("open probe is armed");

    let update_bytes = peer
        .take_notifications()
        .iter()
        .filter(|(method, _)| method == "update")
        .map(|(_, params)| serde_json::to_vec(params).map(|bytes| bytes.len()).unwrap_or(0))
        .sum();

    let stats = {
        let inner = core.inner();
        let buffer_id = inner.views.get(&view_id).expect("view exists").borrow().get_buffer_id();
        let editor = inner.editors.get(&buffer_id).expect("editor exists").borrow();
        if let Some(store) = editor.vlf_store.as_ref() {
            FixtureStats {
                mode: "vlf",
                bytes: store.len_bytes() as usize,
                lines: 1,
                leaves: 0,
                update_bytes,
            }
        } else {
            let rope = editor.get_buffer();
            FixtureStats {
                mode: "rope",
                bytes: rope.len(),
                lines: rope.measure::<LinesMetric>() + 1,
                leaves: rope.iter_chunks(..).count(),
                update_bytes,
            }
        }
    };

    (timings, stats)
}

fn stage_micros(timings: &OpenStageTimings) -> Vec<u128> {
    OpenStage::ALL.iter().map(|stage| timings.micros(*stage)).collect()
}

fn median_plan(warm: &[OpenStageTimings]) -> Vec<u128> {
    (0..OpenStage::ALL.len())
        .map(|index| {
            let mut values: Vec<u128> =
                warm.iter().map(|timings| stage_micros(timings)[index]).collect();
            values.sort_unstable();
            values[values.len() / 2]
        })
        .collect()
}

fn min_plan(warm: &[OpenStageTimings]) -> Vec<u128> {
    (0..OpenStage::ALL.len())
        .map(|index| warm.iter().map(|timings| stage_micros(timings)[index]).min().unwrap_or(0))
        .collect()
}

fn stage_us(plan: &[u128], stage: OpenStage) -> u128 {
    let index = OpenStage::ALL.iter().position(|s| *s == stage).expect("stage in ALL");
    plan[index]
}

fn format_plan(plan: &[u128]) -> String {
    OpenStage::ALL
        .iter()
        .zip(plan)
        .map(|(stage, micros)| format!("{}={micros}", stage.name()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn plan_json(plan: &[u128]) -> Value {
    let mut map = serde_json::Map::new();
    for (stage, micros) in OpenStage::ALL.iter().zip(plan) {
        map.insert(stage.name().to_owned(), json!(micros));
    }
    Value::Object(map)
}

fn env_list(name: &str, default: &str) -> Vec<String> {
    env::var(name)
        .unwrap_or_else(|_| default.to_owned())
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

#[test]
#[ignore = "manual performance probe; run with --ignored --nocapture"]
fn open_path_perf_probe() {
    let sizes: Vec<usize> = env_list("EE_OPEN_PROBE_SIZES", "1,4,8")
        .iter()
        .map(|value| value.parse().expect("EE_OPEN_PROBE_SIZES entries must be integers"))
        .collect();
    let shapes: Vec<Shape> = env_list("EE_OPEN_PROBE_SHAPES", "code,longline,mixedcrlf")
        .iter()
        .map(|value| Shape::parse(value).unwrap_or_else(|| panic!("unknown shape: {value}")))
        .collect();
    let iters: usize = env::var("EE_OPEN_PROBE_ITERS")
        .unwrap_or_else(|_| "5".to_owned())
        .parse()
        .expect("EE_OPEN_PROBE_ITERS must be an integer");
    let ext = env::var("EE_OPEN_PROBE_EXT").unwrap_or_else(|_| "txt".to_owned());

    let dir = tempfile::TempDir::new().expect("temp dir");
    let mut rows: Vec<Value> = Vec::new();

    println!(
        "[open-probe] open path probe: shapes={:?} sizes_mib={:?} iters={} ext={ext}",
        shapes.iter().map(|shape| shape.name()).collect::<Vec<_>>(),
        sizes,
        iters
    );

    for shape in shapes {
        for mib in &sizes {
            let target = mib * 1024 * 1024;
            let path = dir.path().join(format!("{}-{mib}mib.{ext}", shape.name()));
            std::fs::write(&path, fixture(shape, target)).expect("write fixture");

            let (cold, stats) = open_once(&path);
            let mut warm: Vec<OpenStageTimings> = Vec::with_capacity(iters);
            for _ in 0..iters {
                let (timings, _) = open_once(&path);
                warm.push(timings);
            }
            let warm_median = median_plan(&warm);
            let warm_min = min_plan(&warm);

            let bytes = stats.bytes as f64;
            let throughput = |plan: &[u128]| -> f64 {
                if stats.leaves == 0 {
                    // VLF fixtures never build a rope; the read+rope number
                    // is not comparable, so report 0.
                    return 0.0;
                }
                let read = stage_us(plan, OpenStage::ReadDecode) as f64;
                let rope = stage_us(plan, OpenStage::RopeBuild) as f64;
                let seconds = (read + rope) / 1_000_000.0;
                if seconds <= 0.0 { 0.0 } else { bytes / (1024.0 * 1024.0) / seconds }
            };

            println!(
                "[open-probe] fixture={} size_mib={mib} mode={} bytes={} lines={} leaves={} update_bytes={}",
                shape.name(),
                stats.mode,
                stats.bytes,
                stats.lines,
                stats.leaves,
                stats.update_bytes
            );
            println!("[open-probe]   cold {} total_us={}", cold.summary(), cold.total_micros());
            println!(
                "[open-probe]   warm_median {} total_us={} read_rope_mib_per_s={:.1}",
                format_plan(&warm_median),
                warm_median.iter().sum::<u128>(),
                throughput(&warm_median)
            );
            println!(
                "[open-probe]   warm_min {} read_rope_mib_per_s={:.1}",
                format_plan(&warm_min),
                throughput(&warm_min)
            );

            rows.push(json!({
                "fixture": shape.name(),
                "size_mib": mib,
                "bytes": stats.bytes,
                "lines": stats.lines,
                "leaves": stats.leaves,
                "update_bytes": stats.update_bytes,
                "cold": plan_json(&stage_micros(&cold)),
                "warm_median": plan_json(&warm_median),
                "warm_min": plan_json(&warm_min),
            }));
        }
    }

    if let Some(json_path) = env::var_os("EE_OPEN_PROBE_JSON") {
        let artifact = json!({
            "iters": iters,
            "ext": ext,
            "fixtures": rows,
        });
        std::fs::write(
            PathBuf::from(&json_path),
            serde_json::to_vec_pretty(&artifact).expect("serialize probe artifact"),
        )
        .expect("write probe artifact");
        println!("[open-probe] wrote {}", PathBuf::from(json_path).display());
    }
}
