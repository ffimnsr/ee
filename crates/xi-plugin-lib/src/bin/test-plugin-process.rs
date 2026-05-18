use std::env;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use serde_json::Value;
use xi_core_lib::ConfigTable;
use xi_rope::RopeDelta;

use xi_plugin_lib::{ChunkCache, CoreProxy, Plugin, View, mainloop};

struct LoggingPlugin {
    log_path: String,
    crash_on_config: bool,
}

impl LoggingPlugin {
    fn log(&self, event: &str) {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
            .expect("log file should open");
        writeln!(file, "{event}").expect("log file should write");
    }
}

impl Plugin for LoggingPlugin {
    type Cache = ChunkCache;

    fn initialize(&mut self, _core: CoreProxy) {
        self.log("initialize");
    }

    fn update(
        &mut self,
        _view: &mut View<Self::Cache>,
        _delta: Option<&RopeDelta>,
        _edit_type: String,
        _author: String,
    ) {
    }

    fn did_save(&mut self, _view: &mut View<Self::Cache>, _old_path: Option<&Path>) {}

    fn did_close(&mut self, view: &View<Self::Cache>) {
        self.log(&format!("did_close:{}:{:?}", view.get_id(), view.get_view_ids()));
    }

    fn new_view(&mut self, view: &mut View<Self::Cache>) {
        self.log(&format!("new_view:{}:{:?}", view.get_id(), view.get_view_ids()));
    }

    fn config_changed(&mut self, view: &mut View<Self::Cache>, changes: &ConfigTable) {
        self.log(&format!(
            "config_changed:{}:{:?}",
            view.get_id(),
            changes.keys().collect::<Vec<_>>()
        ));
        assert!(!self.crash_on_config, "config crash requested");
    }

    fn custom_command(&mut self, view: &mut View<Self::Cache>, method: &str, _params: Value) {
        self.log(&format!("custom_command:{}:{}", view.get_id(), method));
    }

    fn shutdown(&mut self) {
        self.log("shutdown");
    }
}

fn main() {
    let log_path = env::var("XI_PLUGIN_EVENT_LOG").expect("XI_PLUGIN_EVENT_LOG is required");
    let crash_on_config = env::var("XI_PLUGIN_CRASH_ON_CONFIG").ok().as_deref() == Some("1");
    let mut plugin = LoggingPlugin { log_path, crash_on_config };

    if let Err(err) = mainloop(&mut plugin) {
        eprintln!("plugin mainloop failed: {err}");
        std::process::exit(1);
    }
}
