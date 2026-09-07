//! `impl CoreState` methods: construction.
use super::*;

impl CoreState {
    pub(crate) fn new(
        peer: &RpcPeer,
        config_dir: Option<PathBuf>,
        extras_dir: Option<PathBuf>,
    ) -> Self {
        #[cfg(feature = "notify")]
        let mut watcher = FileWatcher::new(peer.clone());

        let config_manager = ConfigManager::new(config_dir, extras_dir);

        let plugins_dir = config_manager.get_plugins_dir();
        if let Some(p) = plugins_dir.as_ref() {
            #[cfg(feature = "notify")]
            watcher.watch_filtered(p, true, PLUGIN_EVENT_TOKEN, |p| p.is_dir() || !p.exists());
        }

        CoreState {
            views: BTreeMap::new(),
            editors: BTreeMap::new(),
            #[cfg(feature = "notify")]
            file_manager: FileManager::new(watcher),
            #[cfg(not(feature = "notify"))]
            file_manager: FileManager::new(),
            kill_ring: RefCell::new(Rope::from("")),
            width_cache: RefCell::new(WidthCache::new()),
            config_manager,
            self_ref: None,
            pending_views: Vec::new(),
            pending_line_ending_verifications: Vec::new(),
            peer: Client::new(peer.clone()),
            id_counter: Counter::default(),
            plugins: PluginCatalog::default(),
            launching_plugins: HashSet::new(),
            scheduled_plugin_restarts: HashSet::new(),
            stopping_plugins: HashMap::new(),
            plugin_restart_state: HashMap::new(),
            pending_plugin_commands: Vec::new(),
            running_plugins: Vec::new(),
        }
    }

    pub(super) fn next_view_id(&self) -> ViewId {
        ViewId(self.id_counter.next())
    }

    pub(super) fn next_buffer_id(&self) -> BufferId {
        BufferId(self.id_counter.next())
    }

    pub(super) fn next_plugin_id(&self) -> PluginId {
        PluginPid(self.id_counter.next())
    }

    pub(crate) fn finish_setup(&mut self, self_ref: WeakXiCore) {
        self.self_ref = Some(self_ref);

        // instead of having to do this here, config should just own
        // the plugin catalog and reload automatically
        let plugin_paths = self.config_manager.get_plugin_paths();
        self.plugins.reload_from_paths(&plugin_paths).into_iter().for_each(|err| {
            let message = format!("error loading plugin {err:?}");
            warn!("{message}");
            self.peer.alert(message);
        });
        let plugin_languages = self.plugins.make_languages_map();
        let languages = merged_runtime_languages(&plugin_languages);
        let languages_ids = languages.iter().map(|l| l.name.clone()).collect::<Vec<_>>();
        self.peer.available_languages(languages_ids);
        let lang_config_changes = self.config_manager.set_languages(languages);
        if let Err(error) = reload_default_runtime_loader_languages(self.config_manager.languages())
        {
            warn!("failed reloading runtime loader language config: {error}");
        }
        self.handle_config_changes(lang_config_changes);

        self.ensure_manifest_plugins_started();
    }

    /// Sets (overwriting) the config for a given domain.
    pub(super) fn set_config(&mut self, domain: ConfigDomain, table: Table) {
        let plugin_name = match &domain {
            ConfigDomain::PluginConfig(name) => Some(name.clone()),
            _ => None,
        };
        match self.config_manager.set_user_config(domain, table.clone()) {
            Err(e) => self.peer.alert(format!("{}", e)),
            Ok(changes) => {
                if let Some(plugin_name) = plugin_name {
                    self.running_plugins
                        .iter()
                        .filter(|plugin| plugin.name == plugin_name)
                        .for_each(|plugin| plugin.plugin_config_changed(&table));
                }
                self.handle_config_changes(changes)
            }
        }
    }

    pub(crate) fn set_buffer_user_config(&mut self, buffer_id: BufferId, table: Table) {
        self.set_config(ConfigDomain::UserOverride(buffer_id), table);
    }

    /// Notify editors/views/plugins of config changes.
    pub(super) fn handle_config_changes(&self, changes: Vec<(BufferId, Table)>) {
        for (id, table) in changes {
            let view_id = self
                .views
                .values()
                .find(|v| v.borrow().get_buffer_id() == id)
                .map(|v| v.borrow().get_view_id())
                .unwrap();

            self.make_context(view_id).unwrap().config_changed(&table)
        }
    }
}
