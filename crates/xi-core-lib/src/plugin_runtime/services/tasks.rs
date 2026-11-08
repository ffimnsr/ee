//! `editor.tasks` service.
//!
//! Plugin work that can block runs on a worker thread with a deadline and a
//! cancellation flag. Output is bounded so a task cannot grow host memory
//! without limit.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ee_plugin_api::effect::TaskRequest;
use ee_plugin_api::error::PluginError;
use ee_plugin_api::service::Service;
use serde_json::Value;

/// Maximum serialized task output.
pub const MAX_TASK_BYTES: usize = 1024 * 1024;

/// Background work invoked by `Effect::RunTask`.
pub trait TaskHandler: Send + Sync + 'static {
    fn run(&self, payload: &Value, cancel: &AtomicBool) -> Result<Value, PluginError>;
}

/// Registry of task kinds.
#[derive(Clone, Default)]
pub struct TaskService {
    handlers: Arc<Mutex<BTreeMap<String, Arc<dyn TaskHandler>>>>,
}

impl TaskService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, kind: impl Into<String>, handler: Arc<dyn TaskHandler>) {
        self.handlers.lock().expect("task lock poisoned").insert(kind.into(), handler);
    }

    pub fn has_kind(&self, kind: &str) -> bool {
        self.handlers.lock().expect("task lock poisoned").contains_key(kind)
    }

    /// Run an effect request, discarding its value.
    pub fn run_request(&self, request: &TaskRequest) -> Result<(), PluginError> {
        self.run_kind(
            &request.task_kind,
            &request.payload,
            Duration::from_millis(request.timeout_ms.max(1)),
        )
        .map(|_| ())
    }

    /// Run a task kind and wait for a bounded result.
    pub fn run_kind(
        &self,
        kind: &str,
        payload: &Value,
        timeout: Duration,
    ) -> Result<Value, PluginError> {
        self.run_cancellable(kind, payload, timeout, Arc::new(AtomicBool::new(false)))
    }

    /// Run a task kind with an external cancellation flag.
    pub fn run_cancellable(
        &self,
        kind: &str,
        payload: &Value,
        timeout: Duration,
        cancel: Arc<AtomicBool>,
    ) -> Result<Value, PluginError> {
        let handler = self
            .handlers
            .lock()
            .expect("task lock poisoned")
            .get(kind)
            .cloned()
            .ok_or(PluginError::Unsupported { feature: "task kind" })?;
        let payload = payload.clone();
        let cancel_for_handler = Arc::clone(&cancel);
        let (sender, receiver) = std::sync::mpsc::channel();

        std::thread::Builder::new()
            .name(format!("ee-plugin-task-{kind}"))
            .spawn(move || {
                let _ = sender.send(handler.run(&payload, &cancel_for_handler));
            })
            .map_err(|error| PluginError::Runtime {
                message: format!("cannot start task worker: {error}"),
            })?;

        let result = match receiver.recv_timeout(timeout) {
            Ok(result) => result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                cancel.store(true, Ordering::SeqCst);
                Err(PluginError::Runtime { message: format!("task `{kind}` timed out") })
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                Err(PluginError::Runtime { message: format!("task `{kind}` worker exited") })
            }
        };

        let value = result?;
        let bytes = serde_json::to_vec(&value).map(|bytes| bytes.len()).unwrap_or(0);
        if bytes > MAX_TASK_BYTES {
            return Err(PluginError::Resource {
                message: format!("task output exceeds {MAX_TASK_BYTES} bytes"),
            });
        }
        Ok(value)
    }
}

impl Service for TaskService {
    const ID: &'static str = "editor.tasks@1";
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Echo;

    impl TaskHandler for Echo {
        fn run(&self, payload: &Value, _cancel: &AtomicBool) -> Result<Value, PluginError> {
            Ok(payload.clone())
        }
    }

    struct Slow;

    impl TaskHandler for Slow {
        fn run(&self, _payload: &Value, _cancel: &AtomicBool) -> Result<Value, PluginError> {
            std::thread::sleep(Duration::from_millis(100));
            Ok(Value::Null)
        }
    }

    struct Cancellable;

    impl TaskHandler for Cancellable {
        fn run(&self, _payload: &Value, cancel: &AtomicBool) -> Result<Value, PluginError> {
            while !cancel.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(PluginError::Cancelled)
        }
    }

    #[test]
    fn success_returns_bounded_value() {
        let tasks = TaskService::new();
        tasks.register("echo", Arc::new(Echo));
        let value = tasks.run_kind("echo", &json!({"a": 1}), Duration::from_millis(100)).unwrap();
        assert_eq!(value, json!({"a": 1}));
    }

    #[test]
    fn timeout_is_reported() {
        let tasks = TaskService::new();
        tasks.register("slow", Arc::new(Slow));
        let error = tasks.run_kind("slow", &Value::Null, Duration::from_millis(5)).unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }

    #[test]
    fn cancellation_flag_is_observed() {
        let tasks = TaskService::new();
        tasks.register("cancel", Arc::new(Cancellable));
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancel);
        let handle = std::thread::spawn(move || {
            tasks.run_cancellable("cancel", &Value::Null, Duration::from_secs(2), flag)
        });
        std::thread::sleep(Duration::from_millis(20));
        cancel.store(true, Ordering::SeqCst);
        let error = handle.join().unwrap().unwrap_err();
        assert_eq!(error.class(), ee_plugin_api::error::ErrorClass::Runtime);
    }

    #[test]
    fn unknown_kind_is_unsupported() {
        let tasks = TaskService::new();
        let error = tasks.run_kind("missing", &Value::Null, Duration::from_millis(5)).unwrap_err();
        assert_eq!(error.class(), ee_plugin_api::error::ErrorClass::Runtime);
    }
}
