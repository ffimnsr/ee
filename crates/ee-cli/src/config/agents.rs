use std::collections::{BTreeMap, BTreeSet};

use super::AgentServerSettings;

/// Drop agent server entries that never received a command from any layer.
///
/// Nothing can spawn such entries, so they are removed. Pure env-only
/// entries (user-global env tables for servers enabled by workspace config)
/// are inert decoration and drop silently, unless they are referenced by
/// `default_agent`/rubber duck or carry server intent (label, args, cwd) —
/// those signal a real misconfiguration worth warning about.
pub(super) fn remove_incomplete_servers(
    enabled: bool,
    referenced: &BTreeSet<String>,
    servers: &mut BTreeMap<String, AgentServerSettings>,
) -> Vec<String> {
    let mut invalid = Vec::new();
    servers.retain(|id, server| {
        if server.command.trim().is_empty() {
            if enabled
                && (referenced.contains(id)
                    || server.label.is_some()
                    || !server.args.is_empty()
                    || server.cwd.is_some())
            {
                invalid.push(id.clone());
            }
            return false;
        }
        true
    });
    invalid
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn incomplete_server() -> AgentServerSettings {
        AgentServerSettings {
            label: None,
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None::<PathBuf>,
        }
    }

    #[test]
    fn disabled_agents_drop_incomplete_servers_without_warnings() {
        let mut servers = BTreeMap::from([(String::from("assistant"), incomplete_server())]);

        let invalid = remove_incomplete_servers(false, &BTreeSet::new(), &mut servers);

        assert!(invalid.is_empty());
        assert!(servers.is_empty());
    }

    #[test]
    fn enabled_agents_drop_unreferenced_env_only_servers_silently() {
        let mut servers = BTreeMap::from([(String::from("assistant"), incomplete_server())]);

        let invalid = remove_incomplete_servers(true, &BTreeSet::new(), &mut servers);

        assert!(invalid.is_empty(), "env-only decoration is inert, not an error");
        assert!(servers.is_empty());
    }

    #[test]
    fn enabled_agents_report_half_defined_servers() {
        let mut servers = BTreeMap::new();
        let mut labeled = incomplete_server();
        labeled.label = Some(String::from("Ghost Helper"));
        servers.insert(String::from("labeled"), labeled);
        let mut with_args = incomplete_server();
        with_args.args = vec![String::from("serve")];
        servers.insert(String::from("with-args"), with_args);
        let mut with_cwd = incomplete_server();
        with_cwd.cwd = Some(PathBuf::from("/tmp"));
        servers.insert(String::from("with-cwd"), with_cwd);

        let invalid = remove_incomplete_servers(true, &BTreeSet::new(), &mut servers);

        assert_eq!(invalid, ["labeled", "with-args", "with-cwd"]);
        assert!(servers.is_empty());
    }

    #[test]
    fn enabled_agents_report_referenced_env_only_servers() {
        let mut servers = BTreeMap::from([
            (String::from("default"), incomplete_server()),
            (String::from("critic"), incomplete_server()),
            (String::from("decor"), incomplete_server()),
        ]);

        let invalid = remove_incomplete_servers(
            true,
            &BTreeSet::from([String::from("default"), String::from("critic")]),
            &mut servers,
        );

        assert_eq!(invalid, ["critic", "default"], "retain visits ids in sorted order");
        assert!(servers.is_empty());
    }
}
