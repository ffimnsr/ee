use std::collections::BTreeMap;

use super::AgentServerSettings;

pub(super) fn remove_incomplete_servers(
    enabled: bool,
    servers: &mut BTreeMap<String, AgentServerSettings>,
) -> Vec<String> {
    let mut invalid = Vec::new();
    servers.retain(|id, server| {
        if server.command.trim().is_empty() {
            if enabled {
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

        let invalid = remove_incomplete_servers(false, &mut servers);

        assert!(invalid.is_empty());
        assert!(servers.is_empty());
    }

    #[test]
    fn enabled_agents_report_and_drop_incomplete_servers() {
        let mut servers = BTreeMap::from([(String::from("assistant"), incomplete_server())]);

        let invalid = remove_incomplete_servers(true, &mut servers);

        assert_eq!(invalid, ["assistant"]);
        assert!(servers.is_empty());
    }
}
