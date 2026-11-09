//! Capability-gated linker for Wasm host imports.
//!
//! Imports only exist when the manifest grants the mapped permission. A module
//! that imports anything else fails before instantiation, so an undeclared
//! plugin cannot reach ambient host capabilities.

use ee_plugin_api::error::PluginError;
use ee_plugin_api::manifest::Permissions;
use ee_plugin_api::wire::{EffectEnvelope, HealthReport, ServiceRequestParams};
use serde::Deserialize;
use serde_json::Value;
use wasmtime::{Caller, Linker, Module};

use super::{MAX_WASM_OUTPUT_BYTES, WasmHostState};

/// Service used for editor command dispatch from Wasm guests.
const COMMANDS_SERVICE: &str = "editor.commands@1";
/// Maximum return code magnitude used by host imports.
const DENIED: i32 = -1;
const LIMIT: i32 = -2;

/// What a host import requires before it is linked.
#[derive(Debug, Clone, Copy)]
enum Requirement {
    /// Always available. Used for a plugin's own output channels.
    Always,
    /// Requires at least one declared permission.
    AnyDeclared,
    /// Requires every listed permission.
    All(&'static [&'static str]),
}

const EVENT: &str = "host_event";
const COMMAND: &str = "host_command";
const SERVICE_CALL: &str = "host_service_call";
const CONTRIBUTE: &str = "host_contribute";
const HEALTH: &str = "host_health";

fn requirement(name: &str) -> Option<Requirement> {
    match name {
        EVENT | HEALTH => Some(Requirement::Always),
        CONTRIBUTE | SERVICE_CALL => Some(Requirement::AnyDeclared),
        COMMAND => Some(Requirement::All(&["editor.commands.invoke"])),
        _ => None,
    }
}

/// Import gate set derived from manifest permissions.
pub(super) struct ImportGates {
    permissions: Permissions,
}

impl ImportGates {
    pub fn new(permissions: &Permissions) -> Self {
        Self { permissions: permissions.clone() }
    }

    fn granted(&self, name: &str) -> bool {
        match requirement(name) {
            Some(Requirement::Always) => true,
            Some(Requirement::AnyDeclared) => !self.permissions.0.is_empty(),
            Some(Requirement::All(required)) => {
                required.iter().all(|permission| self.permissions.grants(permission))
            }
            None => false,
        }
    }

    /// Reject any import that is unknown or not granted.
    pub fn check_module(&self, module: &Module) -> Result<(), PluginError> {
        for import in module.imports() {
            if import.module() != "env" || !self.granted(import.name()) {
                return Err(PluginError::Permission {
                    permission: format!("wasm import {}::{}", import.module(), import.name()),
                });
            }
        }
        Ok(())
    }

    /// Define every granted import.
    pub fn define(&self, linker: &mut Linker<WasmHostState>) -> Result<(), PluginError> {
        if self.granted(EVENT) {
            define(linker.func_wrap("env", EVENT, host_event), EVENT)?;
        }
        if self.granted(HEALTH) {
            define(linker.func_wrap("env", HEALTH, host_health), HEALTH)?;
        }
        if self.granted(CONTRIBUTE) {
            define(linker.func_wrap("env", CONTRIBUTE, host_contribute), CONTRIBUTE)?;
        }
        if self.granted(COMMAND) {
            define(linker.func_wrap("env", COMMAND, host_command), COMMAND)?;
        }
        if self.granted(SERVICE_CALL) {
            define(linker.func_wrap("env", SERVICE_CALL, host_service_call), SERVICE_CALL)?;
        }
        Ok(())
    }
}

fn define(
    result: Result<&mut Linker<WasmHostState>, wasmtime::Error>,
    name: &str,
) -> Result<(), PluginError> {
    result.map(|_| ()).map_err(|error| PluginError::Runtime {
        message: format!("cannot define wasm import {name}: {error}"),
    })
}

fn host_event(mut caller: Caller<'_, WasmHostState>, ptr: i32, len: i32) -> wasmtime::Result<i32> {
    let Some(payload) = read_payload(&mut caller, ptr, len) else {
        return Ok(DENIED);
    };
    let envelope: EffectEnvelope = match serde_json::from_slice(&payload) {
        Ok(envelope) => envelope,
        Err(error) => {
            caller.data_mut().last_error = Some(error.to_string());
            return Ok(DENIED);
        }
    };
    let plugin_id = caller.data().plugin_id.clone();
    let adapter = caller.data().adapter.clone();
    match adapter.submit_effects(Some(&plugin_id), envelope.effects) {
        Ok(()) => Ok(0),
        Err(error) => {
            caller.data_mut().last_error = Some(error.to_string());
            Ok(DENIED)
        }
    }
}

fn host_health(mut caller: Caller<'_, WasmHostState>, ptr: i32, len: i32) -> wasmtime::Result<i32> {
    let Some(payload) = read_payload(&mut caller, ptr, len) else {
        return Ok(DENIED);
    };
    let report: HealthReport = match serde_json::from_slice(&payload) {
        Ok(report) => report,
        Err(error) => {
            caller.data_mut().last_error = Some(error.to_string());
            return Ok(DENIED);
        }
    };
    caller.data_mut().health = Some(report);
    Ok(0)
}

fn host_contribute(
    mut caller: Caller<'_, WasmHostState>,
    ptr: i32,
    len: i32,
) -> wasmtime::Result<i32> {
    let Some(payload) = read_payload(&mut caller, ptr, len) else {
        return Ok(DENIED);
    };
    let envelope: ee_plugin_api::wire::ContributionEnvelope = match serde_json::from_slice(&payload)
    {
        Ok(envelope) => envelope,
        Err(error) => {
            caller.data_mut().last_error = Some(error.to_string());
            return Ok(DENIED);
        }
    };
    let max = caller.data().limits.max_contributions;
    if caller.data().contributions.len().saturating_add(envelope.upsert.len()) > max {
        caller.data_mut().last_error = Some(format!("contribution cap of {max} exceeded"));
        return Ok(LIMIT);
    }
    for id in envelope.remove {
        caller.data_mut().contributions.remove(&id);
    }
    for contribution in envelope.upsert {
        let state = caller.data_mut();
        if let Err(error) = state.contributions.upsert(state.sink.as_ref(), contribution) {
            state.last_error = Some(error.to_string());
            return Ok(DENIED);
        }
    }
    Ok(0)
}

#[derive(Debug, Deserialize)]
struct CommandInvocation {
    command: String,
    #[serde(default)]
    args: Value,
}

fn host_command(
    mut caller: Caller<'_, WasmHostState>,
    ptr: i32,
    len: i32,
) -> wasmtime::Result<i64> {
    let Some(payload) = read_payload(&mut caller, ptr, len) else {
        return Ok(0);
    };
    let invocation: CommandInvocation = match serde_json::from_slice(&payload) {
        Ok(invocation) => invocation,
        Err(error) => {
            caller.data_mut().last_error = Some(error.to_string());
            return Ok(0);
        }
    };
    let request = ServiceRequestParams {
        service: COMMANDS_SERVICE.parse().expect("static service id"),
        method: "invoke".to_owned(),
        request_id: 0,
        params: serde_json::json!({
            "command": invocation.command,
            "args": invocation.args,
        }),
    };
    Ok(dispatch(&mut caller, &request))
}

fn host_service_call(
    mut caller: Caller<'_, WasmHostState>,
    ptr: i32,
    len: i32,
) -> wasmtime::Result<i64> {
    let Some(payload) = read_payload(&mut caller, ptr, len) else {
        return Ok(0);
    };
    let request: ServiceRequestParams = match serde_json::from_slice(&payload) {
        Ok(request) => request,
        Err(error) => {
            caller.data_mut().last_error = Some(error.to_string());
            return Ok(0);
        }
    };
    if let Err(error) = caller.data().broker.check_service_request(&request) {
        caller.data_mut().last_error = Some(error.to_string());
        return Ok(0);
    }
    Ok(dispatch(&mut caller, &request))
}

fn dispatch(caller: &mut Caller<'_, WasmHostState>, request: &ServiceRequestParams) -> i64 {
    let plugin_id = caller.data().plugin_id.clone();
    let adapter = caller.data().adapter.clone();
    match adapter.handle_service_request(&plugin_id, request) {
        Ok(value) => match serde_json::to_vec(&value) {
            Ok(bytes) => write_response(caller, &bytes),
            Err(error) => {
                caller.data_mut().last_error = Some(error.to_string());
                0
            }
        },
        Err(error) => {
            caller.data_mut().last_error = Some(error.to_string());
            0
        }
    }
}

fn read_payload(caller: &mut Caller<'_, WasmHostState>, ptr: i32, len: i32) -> Option<Vec<u8>> {
    if ptr < 0 || len < 0 {
        caller.data_mut().last_error = Some("negative wasm payload bounds".to_owned());
        return None;
    }
    let len = usize::try_from(len).ok()?;
    if len > MAX_WASM_OUTPUT_BYTES {
        caller.data_mut().last_error = Some("wasm payload exceeds cap".to_owned());
        return None;
    }
    let memory = caller.data().memory?;
    let mut buffer = vec![0_u8; len];
    memory.read(&*caller, usize::try_from(ptr).ok()?, &mut buffer).ok()?;
    Some(buffer)
}

/// Allocate guest memory via `plugin_alloc` and copy `bytes` into it.
///
/// Reentrant export calls are supported by wasmtime: the guest is paused inside
/// the import while `plugin_alloc` runs.
fn write_response(caller: &mut Caller<'_, WasmHostState>, bytes: &[u8]) -> i64 {
    if bytes.len() > MAX_WASM_OUTPUT_BYTES {
        caller.data_mut().last_error = Some("wasm response exceeds cap".to_owned());
        return 0;
    }
    let Some(alloc) = caller.get_export("plugin_alloc").and_then(|export| export.into_func())
    else {
        caller.data_mut().last_error = Some("guest is missing plugin_alloc".to_owned());
        return 0;
    };
    let Ok(alloc) = alloc.typed::<i32, i32>(&*caller) else {
        caller.data_mut().last_error = Some("plugin_alloc has the wrong signature".to_owned());
        return 0;
    };
    let Ok(ptr) = alloc.call(&mut *caller, bytes.len() as i32) else {
        caller.data_mut().last_error = Some("plugin_alloc failed".to_owned());
        return 0;
    };
    if ptr < 0 {
        caller.data_mut().last_error = Some("plugin_alloc returned a negative pointer".to_owned());
        return 0;
    }
    let Some(memory) = caller.data().memory else {
        return 0;
    };
    if let Err(error) = memory.write(&mut *caller, ptr as usize, bytes) {
        caller.data_mut().last_error = Some(error.to_string());
        return 0;
    }
    ((ptr as u64) << 32 | bytes.len() as u64) as i64
}
