//! Wasm transport tests built from WAT fixtures.
//!
//! Fixtures are inline so tests never need a prebuilt plugin, network, or user
//! home. `wat` support comes from the workspace `wasmtime` feature set.

use std::sync::{Arc, Mutex};

use ee_plugin_api::effect::Effect;
use ee_plugin_api::error::{ErrorClass, PluginError};
use ee_plugin_api::host::{HostAdapter, HostKind};
use ee_plugin_api::ids::PluginId;
use ee_plugin_api::manifest::Permissions;
use wasmtime::{Config, Engine, Module};

use super::linker::ImportGates;
use super::{WasmClient, WasmSpec};
use crate::MemoryContributionSink;
use crate::health::TerminationReason;

#[derive(Default)]
struct RecordingAdapter {
    effects: Mutex<Vec<Effect>>,
}

impl HostAdapter for RecordingAdapter {
    fn host_kind(&self) -> HostKind {
        HostKind::Editor
    }

    fn submit_effects(
        &self,
        _plugin: Option<&PluginId>,
        effects: Vec<Effect>,
    ) -> Result<(), PluginError> {
        self.effects.lock().unwrap().extend(effects);
        Ok(())
    }
}

fn exports(extra: &str) -> String {
    format!(
        r#"(module
  (memory (export "memory") 1)
  (global $next (mut i32) (i32.const 4096))
  (func (export "plugin_alloc") (param $len i32) (result i32)
    (local $ptr i32)
    global.get $next
    local.set $ptr
    local.get $ptr
    local.get $len
    i32.add
    global.set $next
    local.get $ptr)
  (func (export "plugin_dealloc") (param i32) (param i32))
  {extra}
)"#
    )
}

const EMPTY_I64: &str = r#"(func (export "plugin_apply") (param i32) (param i32) (result i64) (i64.const 0))
  (func (export "plugin_event") (param i32) (param i32) (result i64) (i64.const 0))
  (func (export "plugin_command") (param i32) (param i32) (result i64) (i64.const 0))
  (func (export "plugin_contribution") (param i32) (param i32) (result i64) (i64.const 0))"#;

struct Fixture {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
}

fn fixture(wat: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plugin.wasm");
    std::fs::write(&path, wat).unwrap();
    Fixture { _dir: dir, path }
}

fn spawn(wat: &str) -> (Fixture, WasmClient) {
    let fixture = fixture(wat);
    let mut spec = WasmSpec::new(PluginId::new("guest").unwrap(), &fixture.path);
    spec.limits.rpc_timeout_ms = 100;
    let adapter = Arc::new(RecordingAdapter::default());
    let sink = Arc::new(MemoryContributionSink::new());
    let client = WasmClient::spawn(spec, adapter, sink).unwrap();
    (fixture, client)
}

#[test]
fn valid_module_loads_and_stops_cleanly() {
    let (_fixture, mut client) = spawn(&exports(EMPTY_I64));
    assert!(client.is_running());
    assert_eq!(client.termination_reason(), None);
    client.shutdown();
    assert_eq!(client.termination_reason(), Some(TerminationReason::HostShutdown));
}

#[test]
fn missing_export_is_rejected() {
    let wat = exports(
        r#"(func (export "plugin_event") (param i32) (param i32) (result i64) (i64.const 0))"#,
    );
    let fixture = fixture(&wat);
    let mut spec = WasmSpec::new(PluginId::new("guest").unwrap(), &fixture.path);
    spec.limits.rpc_timeout_ms = 100;
    let error = WasmClient::spawn(
        spec,
        Arc::new(RecordingAdapter::default()),
        Arc::new(MemoryContributionSink::new()),
    )
    .unwrap_err();
    assert_eq!(error.class(), ErrorClass::Manifest);
}

#[test]
fn undeclared_import_is_denied_before_instantiation() {
    let wat = exports(&format!(
        r#"(import "wasi_snapshot_preview1" "fd_write"
      (func (param i32) (param i32) (param i32) (param i32) (result i32)))
    {EMPTY_I64}"#
    ));
    let fixture = fixture(&wat);
    let mut spec = WasmSpec::new(PluginId::new("guest").unwrap(), &fixture.path);
    spec.limits.rpc_timeout_ms = 100;
    let error = WasmClient::spawn(
        spec,
        Arc::new(RecordingAdapter::default()),
        Arc::new(MemoryContributionSink::new()),
    )
    .unwrap_err();
    assert_eq!(error.class(), ErrorClass::Permission);
}

#[test]
fn command_import_requires_its_permission() {
    let wat = exports(&format!(
        r#"(import "env" "host_command" (func (param i32) (param i32) (result i64)))
    {EMPTY_I64}"#
    ));
    let engine = Engine::new(&Config::new()).unwrap();
    let module = Module::new(&engine, &wat).unwrap();

    let denied = ImportGates::new(&Permissions::default());
    assert!(denied.check_module(&module).is_err());

    let mut granted = Permissions::default();
    granted.0.insert("editor.commands.invoke".to_owned(), true);
    assert!(ImportGates::new(&granted).check_module(&module).is_ok());
}

#[test]
fn undeclared_socket_import_is_denied() {
    let wat = exports(&format!(
        r#"(import "env" "host_socket" (func (param i32) (param i32) (result i32)))
    {EMPTY_I64}"#
    ));
    let engine = Engine::new(&Config::new()).unwrap();
    let module = Module::new(&engine, &wat).unwrap();
    let gates = ImportGates::new(&Permissions::default());
    let error = gates.check_module(&module).unwrap_err();
    assert_eq!(error.class(), ErrorClass::Permission);
}

#[test]
fn wasm_filesystem_service_call_uses_broker_scope() {
    let wat = exports(
        r#"(import "env" "host_service_call" (func $call (param i32) (param i32) (result i64)))
  (data (i32.const 64) "{\"service\":\"editor.fs@1\",\"method\":\"read\",\"request_id\":1,\"params\":{\"path\":\"secret\"}}")
  (func (export "plugin_apply") (param i32) (param i32) (result i64)
    (drop (call $call (i32.const 64) (i32.const 83)))
    (i64.const 0))
  (func (export "plugin_event") (param i32) (param i32) (result i64) (i64.const 0))
  (func (export "plugin_command") (param i32) (param i32) (result i64) (i64.const 0))
  (func (export "plugin_contribution") (param i32) (param i32) (result i64) (i64.const 0))"#,
    );
    let fixture = fixture(&wat);
    let mut spec = WasmSpec::new(PluginId::new("guest").unwrap(), &fixture.path);
    spec.limits.rpc_timeout_ms = 100;
    // `host_service_call` needs at least one declared permission; the broker
    // still denies the workspace path because no root was granted.
    spec.permissions.0.insert("editor.fs.read".to_owned(), true);
    let client = WasmClient::spawn(
        spec,
        Arc::new(RecordingAdapter::default()),
        Arc::new(MemoryContributionSink::new()),
    )
    .unwrap();
    let error = client.last_error().unwrap_or_default();
    assert!(error.contains("editor.fs.read"), "{error}");
}

#[test]
fn granted_health_import_reaches_host_state() {
    let wat = exports(
        r#"(import "env" "host_health" (func $health (param i32) (param i32) (result i32)))
  (data (i32.const 64) "{\"state\":\"loaded\"}")
  (func (export "plugin_apply") (param i32) (param i32) (result i64)
    (drop (call $health (i32.const 64) (i32.const 18)))
    (i64.const 0))
  (func (export "plugin_event") (param i32) (param i32) (result i64) (i64.const 0))
  (func (export "plugin_command") (param i32) (param i32) (result i64) (i64.const 0))
  (func (export "plugin_contribution") (param i32) (param i32) (result i64) (i64.const 0))"#,
    );
    let (_fixture, client) = spawn(&wat);
    assert_eq!(client.health().unwrap().state, "loaded");
}

#[test]
fn infinite_loop_is_terminated_by_fuel_or_epoch() {
    let wat = exports(
        r#"(func (export "plugin_apply") (param i32) (param i32) (result i64)
    (loop $spin (br $spin))
    (i64.const 0))
  (func (export "plugin_event") (param i32) (param i32) (result i64) (i64.const 0))
  (func (export "plugin_command") (param i32) (param i32) (result i64) (i64.const 0))
  (func (export "plugin_contribution") (param i32) (param i32) (result i64) (i64.const 0))"#,
    );
    let fixture = fixture(&wat);
    let mut spec = WasmSpec::new(PluginId::new("spin").unwrap(), &fixture.path);
    spec.limits.rpc_timeout_ms = 50;
    let error = WasmClient::spawn(
        spec,
        Arc::new(RecordingAdapter::default()),
        Arc::new(MemoryContributionSink::new()),
    )
    .unwrap_err();
    assert!(matches!(error, PluginError::Resource { .. }));
    assert!(error.to_string().contains("fuel_exhausted"));
}

#[test]
fn memory_cap_denies_growth_and_terminates() {
    let wat = exports(
        r#"(func (export "plugin_apply") (param i32) (param i32) (result i64)
    (if (i32.eq (memory.grow (i32.const 64)) (i32.const -1))
      (then unreachable))
    (i64.const 0))
  (func (export "plugin_event") (param i32) (param i32) (result i64) (i64.const 0))
  (func (export "plugin_command") (param i32) (param i32) (result i64) (i64.const 0))
  (func (export "plugin_contribution") (param i32) (param i32) (result i64) (i64.const 0))"#,
    );
    let fixture = fixture(&wat);
    let mut spec = WasmSpec::new(PluginId::new("grow").unwrap(), &fixture.path);
    spec.limits.rpc_timeout_ms = 100;
    // One 64 KiB page is allowed; any growth is denied.
    spec.limits.max_memory_bytes = 64 * 1024;
    let error = WasmClient::spawn(
        spec,
        Arc::new(RecordingAdapter::default()),
        Arc::new(MemoryContributionSink::new()),
    )
    .unwrap_err();
    assert!(error.to_string().contains("memory_limit"), "{error}");
}

#[test]
fn trap_is_contained() {
    let wat = exports(
        r#"(func (export "plugin_apply") (param i32) (param i32) (result i64) (unreachable)
  (func (export "plugin_event") (param i32) (param i32) (result i64) (i64.const 0))
  (func (export "plugin_command") (param i32) (param i32) (result i64) (i64.const 0))
  (func (export "plugin_contribution") (param i32) (param i32) (result i64) (i64.const 0))"#,
    );
    let fixture = fixture(&wat);
    let mut spec = WasmSpec::new(PluginId::new("trap").unwrap(), &fixture.path);
    spec.limits.rpc_timeout_ms = 100;
    let error = WasmClient::spawn(
        spec,
        Arc::new(RecordingAdapter::default()),
        Arc::new(MemoryContributionSink::new()),
    )
    .unwrap_err();
    assert!(error.to_string().contains("trap"), "{error}");
}

#[test]
fn oversized_output_is_rejected() {
    // The guest returns a packed pointer/length above the output cap without
    // writing any bytes; the host must check the length before reading.
    let wat = exports(
        r#"(func (export "plugin_apply") (param i32) (param i32) (result i64)
    (i64.const 0))
  (func (export "plugin_event") (param i32) (param i32) (result i64) (i64.const 0))
  (func (export "plugin_command") (param i32) (param i32) (result i64)
    (i64.const 1099511627776))
  (func (export "plugin_contribution") (param i32) (param i32) (result i64) (i64.const 0))"#,
    );
    let fixture = fixture(&wat);
    let mut spec = WasmSpec::new(PluginId::new("big").unwrap(), &fixture.path);
    spec.limits.rpc_timeout_ms = 100;
    let mut client = WasmClient::spawn(
        spec,
        Arc::new(RecordingAdapter::default()),
        Arc::new(MemoryContributionSink::new()),
    )
    .unwrap();
    let error = client
        .invoke(&ee_plugin_api::ids::CommandId::new("big.run").unwrap(), serde_json::json!({}))
        .unwrap_err();
    assert!(error.to_string().contains("output exceeds"), "{error}");
    assert_eq!(client.termination_reason(), Some(TerminationReason::MemoryLimit));
}
