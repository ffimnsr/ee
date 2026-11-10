//! Wasm host transport.
//!
//! Wasm is the required tier for untrusted plugins. Every import is defined
//! only when the manifest grants the matching permission, every call runs under
//! a fuel budget plus an epoch deadline, and memory growth is capped before the
//! guest can allocate it.

mod linker;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use ee_plugin_api::context::ContributionSink;
use ee_plugin_api::error::PluginError;
use ee_plugin_api::host::{HostAdapter, HostKind};
use ee_plugin_api::ids::{CommandId, PluginId, ServiceId};
use ee_plugin_api::manifest::{Limits, ManifestV2, Permissions, PluginKind};
use ee_plugin_api::wire::{HealthReport, InitializeParams, PluginResponse};
use serde_json::Value;
use wasmtime::{Config, Engine, Instance, Memory, Module, ResourceLimiter, Store, TypedFunc};

use crate::contributions::ContributionSession;
use crate::health::TerminationReason;
use crate::native::BrokerPolicy;

use linker::ImportGates;

/// Maximum bytes a Wasm export may return in one call.
pub const MAX_WASM_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
/// Epoch tick used by the wall-clock deadline thread.
pub const EPOCH_TICK: Duration = Duration::from_millis(50);
/// Fuel granted per top-level call. Wasmtime fuel counts instructions, not time;
/// the epoch deadline is the wall-clock bound and this is the compute bound.
pub const FUEL_PER_CALL: u64 = 50_000_000;
/// Required guest exports.
pub const REQUIRED_EXPORTS: &[&str] = &[
    "plugin_alloc",
    "plugin_dealloc",
    "plugin_apply",
    "plugin_event",
    "plugin_command",
    "plugin_contribution",
];

/// Everything needed to instantiate one Wasm plugin.
#[derive(Debug, Clone)]
pub struct WasmSpec {
    pub plugin_id: PluginId,
    pub entry: PathBuf,
    pub limits: Limits,
    pub permissions: Permissions,
    /// Scoped filesystem/network broker applied to service calls.
    pub broker: BrokerPolicy,
    pub host: HostKind,
    pub services: Vec<ServiceId>,
    pub config: Value,
}

impl WasmSpec {
    pub fn new(plugin_id: PluginId, entry: impl Into<PathBuf>) -> Self {
        Self {
            plugin_id,
            entry: entry.into(),
            limits: Limits::default(),
            permissions: Permissions::default(),
            broker: BrokerPolicy::deny_all(),
            host: HostKind::Editor,
            services: Vec::new(),
            config: Value::Null,
        }
    }

    pub fn from_manifest(
        manifest: &ManifestV2,
        manifest_dir: &Path,
        workspace_root: &Path,
    ) -> Result<Self, PluginError> {
        if manifest.kind != PluginKind::Wasm {
            return Err(PluginError::Manifest(
                ee_plugin_api::manifest::ManifestError::InvalidEntry {
                    entry: manifest.entry.clone(),
                    kind: manifest.kind.as_str(),
                },
            ));
        }
        let plugin_id = PluginId::new(manifest.name.clone()).map_err(PluginError::from)?;
        Ok(Self {
            plugin_id,
            entry: manifest_dir.join(&manifest.entry),
            limits: manifest.limits,
            permissions: manifest.permissions.clone(),
            broker: BrokerPolicy::from_manifest(manifest, workspace_root),
            host: HostKind::Editor,
            services: Vec::new(),
            config: Value::Null,
        })
    }
}

/// Store data shared by every host import.
pub(crate) struct WasmHostState {
    pub plugin_id: PluginId,
    pub adapter: Arc<dyn HostAdapter>,
    pub sink: Arc<dyn ContributionSink>,
    pub contributions: ContributionSession,
    pub limits: Limits,
    pub max_memory_bytes: usize,
    pub broker: BrokerPolicy,
    pub memory: Option<Memory>,
    pub health: Option<HealthReport>,
    pub last_error: Option<String>,
    pub memory_denied: bool,
}

impl ResourceLimiter for WasmHostState {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if desired > self.max_memory_bytes {
            self.memory_denied = true;
            return Ok(false);
        }
        Ok(true)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        // Tables are not part of the manifest contract; keep them bounded so a
        // guest cannot grow function tables without limit.
        Ok(desired <= 10_000)
    }
}

/// One instantiated Wasm plugin.
pub struct WasmClient {
    store: Store<WasmHostState>,
    instance: Instance,
    memory: Memory,
    plugin_id: PluginId,
    rpc_timeout: Duration,
    epoch_ticks: u64,
    terminated: Option<TerminationReason>,
    stop_ticker: Arc<AtomicBool>,
    ticker: Option<JoinHandle<()>>,
    stderr: Arc<Mutex<String>>,
}

impl WasmClient {
    /// Load, gate imports, instantiate, and run `plugin_apply`.
    pub fn spawn(
        spec: WasmSpec,
        adapter: Arc<dyn HostAdapter>,
        sink: Arc<dyn ContributionSink>,
    ) -> Result<Self, PluginError> {
        if !spec.entry.is_file() {
            return Err(PluginError::Io {
                message: format!("wasm entry `{}` does not exist", spec.entry.display()),
            });
        }

        let mut config = Config::new();
        config.consume_fuel(true);
        config.epoch_interruption(true);
        let engine = Engine::new(&config).map_err(|error| PluginError::Runtime {
            message: format!("wasm engine init failed: {error}"),
        })?;
        let module = Module::from_file(&engine, &spec.entry).map_err(|error| {
            PluginError::Schema { message: format!("invalid wasm module: {error}") }
        })?;

        ImportGates::new(&spec.permissions).check_module(&module)?;
        for export in REQUIRED_EXPORTS {
            if module.get_export(export).is_none() {
                return Err(PluginError::Manifest(
                    ee_plugin_api::manifest::ManifestError::InvalidEntry {
                        entry: spec.entry.display().to_string(),
                        kind: "wasm (missing required export)",
                    },
                ));
            }
        }

        let max_memory_bytes = usize::try_from(spec.limits.max_memory_bytes).unwrap_or(usize::MAX);
        let state = WasmHostState {
            plugin_id: spec.plugin_id.clone(),
            adapter,
            sink,
            contributions: ContributionSession::new(),
            limits: spec.limits,
            max_memory_bytes,
            broker: spec.broker,
            memory: None,
            health: None,
            last_error: None,
            memory_denied: false,
        };
        let mut store = Store::new(&engine, state);
        store.limiter(|state| state);
        store.set_fuel(FUEL_PER_CALL).map_err(|error| PluginError::Runtime {
            message: format!("cannot set wasm fuel: {error}"),
        })?;

        let mut linker = wasmtime::Linker::new(&engine);
        ImportGates::new(&spec.permissions).define(&mut linker)?;
        let instance = linker.instantiate(&mut store, &module).map_err(|error| {
            PluginError::Runtime { message: format!("wasm instantiation failed: {error}") }
        })?;
        let memory = instance.get_memory(&mut store, "memory").ok_or_else(|| {
            PluginError::Manifest(ee_plugin_api::manifest::ManifestError::InvalidEntry {
                entry: spec.entry.display().to_string(),
                kind: "wasm (missing memory export)",
            })
        })?;
        store.data_mut().memory = Some(memory);

        let epoch_ticks = (spec.limits.rpc_timeout_ms.max(EPOCH_TICK.as_millis() as u64)
            / EPOCH_TICK.as_millis() as u64)
            .max(1);
        let stop_ticker = Arc::new(AtomicBool::new(false));
        let ticker_stop = Arc::clone(&stop_ticker);
        let ticker_engine = engine.clone();
        let ticker = thread::Builder::new()
            .name(format!("ee-wasm-epoch-{}", spec.plugin_id))
            .spawn(move || {
                while !ticker_stop.load(Ordering::Relaxed) {
                    thread::sleep(EPOCH_TICK);
                    ticker_engine.increment_epoch();
                }
            })
            .map_err(|error| PluginError::Runtime {
                message: format!("cannot start wasm epoch thread: {error}"),
            })?;

        let mut client = Self {
            store,
            instance,
            memory,
            plugin_id: spec.plugin_id,
            rpc_timeout: Duration::from_millis(spec.limits.rpc_timeout_ms.max(1)),
            epoch_ticks,
            terminated: None,
            stop_ticker,
            ticker: Some(ticker),
            stderr: Arc::new(Mutex::new(String::new())),
        };

        let initialize = InitializeParams {
            plugin_id: client.plugin_id.clone(),
            host: spec.host,
            api: ee_plugin_api::manifest::API_VERSION.to_owned(),
            services: spec.services,
            config: spec.config,
            limits: spec.limits,
        };
        let payload = serde_json::to_vec(&initialize)
            .map_err(|error| PluginError::Runtime { message: error.to_string() })?;
        client.call("plugin_apply", &payload)?;
        Ok(client)
    }

    /// Invoke a command and return its result.
    pub fn invoke(&mut self, id: &CommandId, args: Value) -> Result<Value, PluginError> {
        self.invoke_view(id, ee_plugin_api::model::ViewId(0), args)
    }

    pub fn invoke_view(
        &mut self,
        id: &CommandId,
        view_id: ee_plugin_api::model::ViewId,
        args: Value,
    ) -> Result<Value, PluginError> {
        let params = ee_plugin_api::wire::CommandParams {
            id: id.clone(),
            view_id,
            request_id: 1,
            args,
            deadline_ms: self.rpc_timeout.as_millis() as u64,
        };
        let payload = serde_json::to_vec(&params)
            .map_err(|error| PluginError::Runtime { message: error.to_string() })?;
        let Some(output) = self.call("plugin_command", &payload)? else {
            return Ok(Value::Null);
        };
        let response: PluginResponse = serde_json::from_slice(&output).map_err(|error| {
            PluginError::Transport { message: format!("malformed wasm response: {error}") }
        })?;
        if let Some(error) = response.error {
            return Err(PluginError::Transport { message: error.message });
        }
        Ok(response.result.unwrap_or(Value::Null))
    }

    /// Deliver an event to the guest.
    pub fn send_event(
        &mut self,
        name: impl Into<String>,
        revision: u64,
        payload: Value,
    ) -> Result<(), PluginError> {
        let params = ee_plugin_api::wire::EventParams { name: name.into(), revision, payload };
        let bytes = serde_json::to_vec(&params)
            .map_err(|error| PluginError::Runtime { message: error.to_string() })?;
        self.call("plugin_event", &bytes)?;
        Ok(())
    }

    /// Ask the guest for a fresh contribution snapshot.
    pub fn request_contributions(&mut self, revision: u64) -> Result<(), PluginError> {
        let params = ee_plugin_api::wire::ContributionRequestParams { revision };
        let bytes = serde_json::to_vec(&params)
            .map_err(|error| PluginError::Runtime { message: error.to_string() })?;
        if let Some(output) = self.call("plugin_contribution", &bytes)? {
            self.apply_contributions(&output)?;
        }
        Ok(())
    }

    pub fn plugin_id(&self) -> &PluginId {
        &self.plugin_id
    }

    pub fn health(&self) -> Option<&HealthReport> {
        self.store.data().health.as_ref()
    }

    pub fn contribution_ids(&self) -> Vec<ee_plugin_api::ids::ContributionId> {
        self.store.data().contributions.ids()
    }

    pub fn stderr(&self) -> String {
        self.stderr.lock().map(|text| text.clone()).unwrap_or_default()
    }

    /// Last host-import error reported by the guest, if any.
    pub fn last_error(&self) -> Option<&str> {
        self.store.data().last_error.as_deref()
    }

    pub fn termination_reason(&self) -> Option<TerminationReason> {
        self.terminated
    }

    pub fn is_running(&self) -> bool {
        self.terminated.is_none()
    }

    /// Wasm calls are synchronous; there is no queued transport work.
    pub fn pump(&mut self) -> Result<usize, PluginError> {
        Ok(0)
    }

    /// See [`WasmClient::pump`].
    pub fn pump_for(&mut self, _budget: Duration) -> Result<usize, PluginError> {
        Ok(0)
    }

    /// Stop the guest, drop its epoch ticker, and dispose contributions.
    pub fn shutdown(&mut self) {
        if self.terminated.is_none() {
            self.terminated = Some(TerminationReason::HostShutdown);
        }
        self.stop_ticker.store(true, Ordering::Relaxed);
        if let Some(ticker) = self.ticker.take() {
            let _ = ticker.join();
        }
        self.store.data_mut().contributions.dispose_all();
    }

    fn apply_contributions(&mut self, output: &[u8]) -> Result<(), PluginError> {
        let envelope: ee_plugin_api::wire::ContributionEnvelope = serde_json::from_slice(output)
            .map_err(|error| PluginError::Transport {
                message: format!("malformed wasm contribution: {error}"),
            })?;
        let max = self.store.data().limits.max_contributions;
        for id in envelope.remove {
            self.store.data_mut().contributions.remove(&id);
        }
        for contribution in envelope.upsert {
            if self.store.data().contributions.len() >= max {
                self.terminated = Some(TerminationReason::MemoryLimit);
                return Err(PluginError::Resource {
                    message: format!("plugin `{}` exceeded {max} contributions", self.plugin_id),
                });
            }
            let state = self.store.data_mut();
            state.contributions.upsert(state.sink.as_ref(), contribution)?;
        }
        Ok(())
    }

    /// Copy input into guest memory, run one export, and copy the response out.
    ///
    /// Returns `Ok(None)` when the guest returned a zero-length response.
    fn call(&mut self, name: &str, input: &[u8]) -> Result<Option<Vec<u8>>, PluginError> {
        if let Some(reason) = self.terminated {
            return Err(PluginError::Runtime {
                message: format!("plugin `{}` terminated: {}", self.plugin_id, reason.as_str()),
            });
        }
        if input.len() > MAX_WASM_OUTPUT_BYTES {
            return Err(PluginError::Resource {
                message: format!("wasm input exceeds {MAX_WASM_OUTPUT_BYTES} bytes"),
            });
        }

        // Reset the compute and wall-clock budgets before the first guest call:
        // instantiation consumes fuel, and the default epoch deadline traps as
        // soon as the ticker advances, so `plugin_alloc` needs a fresh window.
        self.store.set_fuel(FUEL_PER_CALL).map_err(|error| PluginError::Runtime {
            message: format!("cannot reset wasm fuel: {error}"),
        })?;
        self.store.set_epoch_deadline(self.epoch_ticks);

        let alloc: TypedFunc<i32, i32> =
            self.instance.get_typed_func(&mut self.store, "plugin_alloc").map_err(|error| {
                PluginError::Runtime { message: format!("wasm missing plugin_alloc: {error}") }
            })?;
        let ptr = alloc
            .call(&mut self.store, input.len() as i32)
            .map_err(|error| self.map_trap("plugin_alloc", error))?;
        if ptr < 0 {
            return Err(PluginError::Resource {
                message: "wasm plugin_alloc returned a negative pointer".to_owned(),
            });
        }
        if !input.is_empty() {
            self.memory.write(&mut self.store, ptr as usize, input).map_err(|error| {
                PluginError::Runtime { message: format!("wasm input write failed: {error}") }
            })?;
        }

        let func: TypedFunc<(i32, i32), i64> =
            self.instance.get_typed_func(&mut self.store, name).map_err(|error| {
                PluginError::Runtime { message: format!("wasm missing export {name}: {error}") }
            })?;
        let packed = func
            .call(&mut self.store, (ptr, input.len() as i32))
            .map_err(|error| self.map_trap(name, error))?;
        if packed == 0 {
            return Ok(None);
        }

        let out_ptr = ((packed as u64) >> 32) as u32;
        let out_len = usize::try_from((packed as u64) as u32).unwrap_or(usize::MAX);
        if out_len > MAX_WASM_OUTPUT_BYTES {
            self.terminated = Some(TerminationReason::MemoryLimit);
            return Err(PluginError::Resource {
                message: format!("wasm output exceeds {MAX_WASM_OUTPUT_BYTES} bytes"),
            });
        }
        let mut bytes = vec![0_u8; out_len];
        self.memory.read(&self.store, out_ptr as usize, &mut bytes).map_err(|error| {
            PluginError::Runtime { message: format!("wasm output read failed: {error}") }
        })?;

        if let Ok(dealloc) =
            self.instance.get_typed_func::<(i32, i32), ()>(&mut self.store, "plugin_dealloc")
        {
            let _ = dealloc.call(&mut self.store, (out_ptr as i32, out_len as i32));
        }
        Ok(Some(bytes))
    }

    fn map_trap(&mut self, call: &str, error: wasmtime::Error) -> PluginError {
        if let Some(trap) = error.downcast_ref::<wasmtime::Trap>() {
            let reason = if self.store.data().memory_denied {
                TerminationReason::MemoryLimit
            } else {
                match trap {
                    wasmtime::Trap::OutOfFuel | wasmtime::Trap::Interrupt => {
                        TerminationReason::FuelExhausted
                    }
                    _ => TerminationReason::Trap,
                }
            };
            self.terminated = Some(reason);
            return PluginError::Resource {
                message: format!("wasm `{call}` terminated: {} ({trap})", reason.as_str()),
            };
        }
        self.terminated = Some(TerminationReason::Trap);
        PluginError::Runtime { message: format!("wasm `{call}` failed: {error}") }
    }
}

impl Drop for WasmClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl std::fmt::Debug for WasmClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WasmClient")
            .field("plugin_id", &self.plugin_id)
            .field("termination", &self.terminated)
            .finish_non_exhaustive()
    }
}
