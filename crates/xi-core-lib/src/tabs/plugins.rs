//! `impl CoreState` methods: plugins.
use super::*;

impl CoreState {
    pub(super) fn ensure_manifest_plugins_started(&mut self) {
        let to_start = self
            .plugins
            .iter()
            .filter(|manifest| {
                manifest.activates_on_startup()
                    || self
                        .views
                        .values()
                        .map(|view| {
                            self.config_manager.get_buffer_language(view.borrow().get_buffer_id())
                        })
                        .any(|language| manifest.receives_updates_for(&language))
            })
            .collect::<Vec<_>>();

        for manifest in to_start {
            self.start_plugin(manifest);
        }
    }

    pub(super) fn ensure_plugins_for_language(&mut self, language: &LanguageId) {
        let to_start = self
            .plugins
            .iter()
            .filter(|manifest| manifest.receives_updates_for(language))
            .collect::<Vec<_>>();

        for manifest in to_start {
            self.start_plugin(manifest);
        }
    }

    pub(super) fn start_plugin(&mut self, manifest: Arc<PluginDescription>) {
        if !self.begin_plugin_launch(&manifest.name) {
            return;
        }

        self.scheduled_plugin_restarts.remove(&manifest.name);
        self.plugin_restart_state.entry(manifest.name.clone()).or_default().last_start =
            Some(Instant::now());
        start_plugin_process(
            manifest,
            self.next_plugin_id(),
            self.self_ref.as_ref().unwrap().clone(),
        );
    }

    pub(super) fn begin_plugin_launch(&mut self, plugin_name: &str) -> bool {
        if self.running_plugins.iter().any(|plugin| plugin.name == plugin_name)
            || self.launching_plugins.contains(plugin_name)
        {
            return false;
        }

        self.launching_plugins.insert(plugin_name.to_string());
        true
    }

    pub(super) fn begin_plugin_shutdown(&mut self, plugin_id: PluginId, reason: StopReason) {
        let Some(plugin) = self.running_plugins.iter().find(|plugin| plugin.id == plugin_id) else {
            return;
        };

        if self.stopping_plugins.contains_key(&plugin_id) {
            return;
        }

        let weak_core = self.self_ref.as_ref().unwrap().clone();
        let process = plugin.controller_handle();
        let plugin_name = plugin.name.clone();
        plugin.shutdown();
        self.stopping_plugins.insert(plugin_id, reason);

        std::thread::spawn(move || {
            let deadline = Instant::now() + PLUGIN_SHUTDOWN_TIMEOUT;
            loop {
                let wait_result = process.has_exited().ok();
                if wait_result == Some(true) {
                    break;
                }

                if Instant::now() >= deadline {
                    weak_core.plugin_stderr(
                        plugin_name.clone(),
                        format!(
                            "shutdown timed out after {:?}; terminating plugin",
                            PLUGIN_SHUTDOWN_TIMEOUT
                        ),
                    );
                    let _ = process.terminate();
                    break;
                }

                std::thread::sleep(Duration::from_millis(25));
            }
        });
    }

    pub(super) fn should_keep_running(&self, manifest: &PluginDescription) -> bool {
        manifest.activates_on_startup()
            || self
                .views
                .values()
                .map(|view| self.config_manager.get_buffer_language(view.borrow().get_buffer_id()))
                .any(|language| manifest.receives_updates_for(&language))
            || self
                .pending_plugin_commands
                .iter()
                .any(|command| command.plugin_name == manifest.name)
    }

    pub(super) fn next_restart_delay(&mut self, plugin_name: &str) -> Duration {
        let state = self.plugin_restart_state.entry(plugin_name.to_string()).or_default();
        if state.last_start.is_some_and(|last_start| last_start.elapsed() >= PLUGIN_STABLE_UPTIME) {
            state.consecutive_failures = 0;
        }

        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        let factor = 1_u64 << state.consecutive_failures.saturating_sub(1).min(6);
        Duration::from_millis(
            (PLUGIN_RESTART_BASE_DELAY_MS * factor).min(PLUGIN_RESTART_MAX_DELAY_MS),
        )
    }

    pub(super) fn schedule_plugin_restart(&mut self, plugin_name: &str) {
        if self.scheduled_plugin_restarts.contains(plugin_name) {
            return;
        }

        let Some(manifest) = self.plugins.get_named(plugin_name) else {
            return;
        };
        if matches!(manifest.scope, crate::plugins::manifest::PluginScope::SingleInvocation)
            || !self.should_keep_running(&manifest)
        {
            return;
        }

        let delay = self.next_restart_delay(plugin_name);
        let weak_core = self.self_ref.as_ref().unwrap().clone();
        let restart_name = plugin_name.to_string();
        self.scheduled_plugin_restarts.insert(restart_name.clone());
        warn!("plugin {} exited unexpectedly; restarting in {:?}", restart_name, delay);
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            weak_core.restart_plugin(restart_name);
        });
    }

    pub(crate) fn restart_plugin(&mut self, plugin_name: &str) {
        self.scheduled_plugin_restarts.remove(plugin_name);
        if self.launching_plugins.contains(plugin_name)
            || self.running_plugins.iter().any(|plugin| plugin.name == plugin_name)
        {
            return;
        }

        if let Some(manifest) = self.plugins.get_named(plugin_name)
            && self.should_keep_running(&manifest)
        {
            self.start_plugin(manifest);
        }
    }
}
