//! Native v2 runtime loop.
//!
//! The runtime owns the transport, routes host messages to the plugin, answers
//! service calls from any thread, and runs periodic ticks so asynchronous
//! plugin work (language-server callbacks) can publish effects without a host
//! event.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use ee_plugin_api::context::{Plugin, PluginContext, ServiceCaller};
use ee_plugin_api::effect::{CommandRequest, Effect};
use ee_plugin_api::error::PluginError;
use ee_plugin_api::wire::{
    EffectEnvelope, HealthReport, HostToPlugin, InitializeParams, PluginResponse, PluginToHost,
    ServiceRequestParams, WireError,
};

use crate::config::PluginConfig;

/// Message transport used by the runtime.
///
/// Implementations must be usable from the runtime reader thread and from
/// plugin worker threads at the same time.
pub trait RuntimeTransport: Send + Sync + 'static {
    fn recv(&self) -> Result<Option<HostToPlugin>, PluginError>;
    fn send(&self, message: PluginToHost) -> Result<(), PluginError>;
}

/// Shared connection with response correlation.
pub struct Connection {
    transport: Arc<dyn RuntimeTransport>,
    incoming: Mutex<Receiver<HostToPlugin>>,
    pending: Arc<Mutex<HashMap<u64, mpsc::Sender<PluginResponse>>>>,
    next_request: AtomicU64,
    timeout: Duration,
}

impl Connection {
    /// Start the reader thread and correlate responses.
    pub fn new(transport: Arc<dyn RuntimeTransport>, timeout: Duration) -> Arc<Self> {
        let (sender, receiver) = mpsc::channel();
        let pending: Arc<Mutex<HashMap<u64, mpsc::Sender<PluginResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let reader_transport = Arc::clone(&transport);
        let reader_pending = Arc::clone(&pending);
        thread::Builder::new()
            .name("ee-plugin-host-reader".to_owned())
            .spawn(move || {
                loop {
                    match reader_transport.recv() {
                        Ok(Some(HostToPlugin::Response(response))) => {
                            let waiter = reader_pending
                                .lock()
                                .expect("response lock poisoned")
                                .remove(&response.request_id);
                            if let Some(waiter) = waiter {
                                let _ = waiter.send(response);
                            }
                        }
                        Ok(Some(message)) => {
                            if sender.send(message).is_err() {
                                break;
                            }
                        }
                        Ok(None) | Err(_) => break,
                    }
                }
            })
            .expect("plugin reader thread must start");
        Arc::new(Self {
            transport,
            incoming: Mutex::new(receiver),
            pending,
            next_request: AtomicU64::new(1),
            timeout,
        })
    }

    pub fn caller(self: &Arc<Self>) -> Arc<dyn ServiceCaller> {
        Arc::new(HostCaller { connection: Arc::clone(self) })
    }

    /// Receive the next host message, or `None` after timeout.
    pub fn recv(&self, timeout: Duration) -> Result<Option<HostToPlugin>, PluginError> {
        match self.incoming.lock().expect("incoming lock poisoned").recv_timeout(timeout) {
            Ok(message) => Ok(Some(message)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => {
                Err(PluginError::Transport { message: "host closed the plugin channel".to_owned() })
            }
        }
    }

    pub fn send(&self, message: PluginToHost) -> Result<(), PluginError> {
        self.transport.send(message)
    }

    pub fn respond(
        &self,
        request_id: u64,
        result: Option<serde_json::Value>,
    ) -> Result<(), PluginError> {
        self.send(PluginToHost::Response(PluginResponse { request_id, result, error: None }))
    }

    pub fn respond_error(&self, request_id: u64, error: PluginError) -> Result<(), PluginError> {
        self.send(PluginToHost::Response(PluginResponse {
            request_id,
            result: None,
            error: Some(WireError {
                class: error.class().as_str().to_owned(),
                code: error.code().to_owned(),
                message: error.to_string(),
                retryable: false,
            }),
        }))
    }

    pub fn call(
        &self,
        service: ee_plugin_api::ids::ServiceId,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, PluginError> {
        let request_id = self.next_request.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = mpsc::channel();
        self.pending.lock().expect("response lock poisoned").insert(request_id, sender);
        self.send(PluginToHost::Request(ServiceRequestParams {
            service: service.clone(),
            method: method.to_owned(),
            request_id,
            params,
        }))?;
        match receiver.recv_timeout(self.timeout) {
            Ok(response) => match (response.result, response.error) {
                (Some(result), None) => Ok(result),
                (None, Some(error)) => Err(PluginError::Transport {
                    message: format!("{}: {}", error.code, error.message),
                }),
                _ => Ok(serde_json::Value::Null),
            },
            Err(RecvTimeoutError::Timeout) => {
                self.pending.lock().expect("response lock poisoned").remove(&request_id);
                Err(PluginError::Transport {
                    message: format!("service `{service}.{method}` timed out"),
                })
            }
            Err(RecvTimeoutError::Disconnected) => {
                Err(PluginError::Transport { message: "host response channel closed".to_owned() })
            }
        }
    }
}

struct HostCaller {
    connection: Arc<Connection>,
}

impl ServiceCaller for HostCaller {
    fn call_service(
        &self,
        service: ee_plugin_api::ids::ServiceId,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, PluginError> {
        self.connection.call(service, method, params)
    }

    fn submit_effects(&self, effects: Vec<Effect>) -> Result<(), PluginError> {
        if effects.is_empty() {
            return Ok(());
        }
        self.connection.send(PluginToHost::Effect(EffectEnvelope { request_id: None, effects }))
    }
}

/// Runtime handle exposed to plugin entry points and tests.
pub struct SdkHost {
    pub connection: Arc<Connection>,
    pub context: PluginContext,
}

/// Run a plugin over stdio using newline-delimited JSON framing.
pub fn run_stdio<P: Plugin>(plugin: P) -> Result<(), PluginError> {
    let config = PluginConfig::from_env()?;
    let transport: Arc<dyn RuntimeTransport> = Arc::new(StdioTransport::new());
    let host = connect(plugin, config, transport)?;
    let _ = host;
    Ok(())
}

/// Connect a runtime to an arbitrary transport.
pub fn connect<P: Plugin>(
    mut plugin: P,
    config: PluginConfig,
    transport: Arc<dyn RuntimeTransport>,
) -> Result<SdkHost, PluginError> {
    let connection =
        Connection::new(transport, Duration::from_millis(config.limits.rpc_timeout_ms.max(1)));
    let initialize = wait_for_initialize(&connection)?;
    let context = PluginContext::new(config.plugin_id.as_str())
        .configured_with(initialize.config.clone())
        .with_service_caller(connection.caller());
    plugin.apply(&context)?;
    connection.respond(0, Some(serde_json::json!({"ok": true})))?;
    connection
        .send(PluginToHost::Health(HealthReport { state: "loaded".to_owned(), reason: None }))?;

    let tick = plugin.tick_interval().unwrap_or(Duration::from_millis(20));
    loop {
        match connection.recv(tick)? {
            Some(HostToPlugin::Event(event)) => {
                let effects = plugin.on_event(&context, &event)?;
                connection.caller().submit_effects(effects)?;
            }
            Some(HostToPlugin::Command(command)) => {
                let request = CommandRequest {
                    id: command.id.clone(),
                    view_id: command.view_id,
                    request_id: command.request_id,
                    args: command.args.clone(),
                };
                match plugin.on_command(&context, &request) {
                    Ok(effects) => {
                        connection.caller().submit_effects(effects)?;
                        connection.respond(command.request_id, Some(serde_json::Value::Null))?;
                    }
                    Err(error) => {
                        connection.respond_error(command.request_id, error)?;
                    }
                }
            }
            Some(HostToPlugin::ContributionRequest(request)) => {
                plugin.on_contribution_request(&context, request.revision)?;
            }
            Some(HostToPlugin::Shutdown(_)) => {
                plugin.on_shutdown(&context);
                return Ok(SdkHost { connection, context });
            }
            Some(HostToPlugin::Initialize(_)) | Some(HostToPlugin::Response(_)) => {}
            None => {
                let effects = plugin.on_tick(&context)?;
                connection.caller().submit_effects(effects)?;
            }
        }
    }
}

fn wait_for_initialize(connection: &Arc<Connection>) -> Result<InitializeParams, PluginError> {
    loop {
        match connection.recv(Duration::from_secs(10))? {
            Some(HostToPlugin::Initialize(params)) => return Ok(params),
            Some(_) => continue,
            None => {
                return Err(PluginError::Transport {
                    message: "initialize handshake timed out".to_owned(),
                });
            }
        }
    }
}

/// Stdio transport with newline-delimited JSON.
pub struct StdioTransport {
    reader: Mutex<BufReader<std::io::Stdin>>,
    writer: Mutex<std::io::Stdout>,
}

impl StdioTransport {
    pub fn new() -> Self {
        Self {
            reader: Mutex::new(BufReader::new(std::io::stdin())),
            writer: Mutex::new(std::io::stdout()),
        }
    }
}

impl Default for StdioTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeTransport for StdioTransport {
    fn recv(&self) -> Result<Option<HostToPlugin>, PluginError> {
        let mut reader = self.reader.lock().expect("stdin lock poisoned");
        let mut buffer = Vec::new();
        loop {
            buffer.clear();
            let read = reader.read_until(b'\n', &mut buffer)?;
            if read == 0 {
                return Ok(None);
            }
            while matches!(buffer.last(), Some(b'\n' | b'\r')) {
                buffer.pop();
            }
            if !buffer.is_empty() {
                break;
            }
        }
        serde_json::from_slice(&buffer)
            .map(Some)
            .map_err(|error| PluginError::Transport { message: error.to_string() })
    }

    fn send(&self, message: PluginToHost) -> Result<(), PluginError> {
        let payload = serde_json::to_vec(&message)
            .map_err(|error| PluginError::Transport { message: error.to_string() })?;
        let mut writer = self.writer.lock().expect("stdout lock poisoned");
        writer.write_all(&payload)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        Ok(())
    }
}
