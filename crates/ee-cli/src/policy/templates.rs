//! Application-owned mandatory-confirm template registry.
//!
//! Template ids are versioned host constants. Persisted rules still carry all
//! resolved matcher fields; template ids only identify why host UI offered the
//! rule and are validated against those explicit fields when loaded.

use super::rules::{HostMatchMode, MatchMode, TrustRule, WriteOperationKind};
use super::{FilesystemOperationKind, TrustCategory, TrustEffect};

pub(crate) const VCS_PUSH: &str = "vcs-push-v1";
pub(crate) const VCS_MUTATION: &str = "vcs-mutation-v1";
pub(crate) const PACKAGE_PUBLISH: &str = "package-publish-v1";
pub(crate) const PACKAGE_INSTALL_SCRIPTS: &str = "package-install-scripts-v1";
pub(crate) const PRIVILEGE_ESCALATION: &str = "privilege-escalation-v1";
pub(crate) const WORKFLOW_CONFIG_WRITE: &str = "workflow-config-write-v1";
pub(crate) const NEW_NETWORK_HOST: &str = "new-network-host-v1";
pub(crate) const DESTRUCTIVE_FILESYSTEM: &str = "destructive-filesystem-v1";

const VCS_MUTATIONS: [&str; 7] =
    ["branch", "cherry-pick", "commit", "merge", "rebase", "reset", "tag"];
const PACKAGE_MANAGERS: [&str; 5] = ["cargo", "npm", "pnpm", "yarn", "bun"];
const PRIVILEGE_TOOLS: [&str; 3] = ["sudo", "doas", "pkexec"];

pub(crate) fn validate_template(template_id: &str, rule: &TrustRule) -> Result<(), String> {
    if rule.effect() != TrustEffect::Confirm {
        return Err("template_id is valid only for confirm rules".into());
    }
    let valid = match (template_id, rule) {
        (VCS_PUSH, TrustRule::Command(rule)) => {
            rule.executable == "git" && command_starts_with(rule.match_mode, &rule.argv, "push")
        }
        (VCS_MUTATION, TrustRule::Command(rule)) => {
            rule.executable == "git"
                && rule.argv.first().is_some_and(|arg| VCS_MUTATIONS.contains(&arg.as_str()))
        }
        (PACKAGE_PUBLISH, TrustRule::Command(rule)) => {
            PACKAGE_MANAGERS.contains(&rule.executable.as_str())
                && rule.argv.first().is_some_and(|arg| arg == "publish")
        }
        (PACKAGE_INSTALL_SCRIPTS, TrustRule::Command(rule)) => {
            PACKAGE_MANAGERS.contains(&rule.executable.as_str())
                && rule
                    .argv
                    .first()
                    .is_some_and(|arg| matches!(arg.as_str(), "install" | "add" | "ci"))
        }
        (PRIVILEGE_ESCALATION, TrustRule::Command(rule)) => {
            PRIVILEGE_TOOLS.contains(&rule.executable.as_str())
        }
        (WORKFLOW_CONFIG_WRITE, TrustRule::Write(rule)) => {
            matches!(rule.operation, WriteOperationKind::Create | WriteOperationKind::Modify)
                && rule.path_prefix.segments().iter().any(|segment| {
                    matches!(segment.as_str(), "workflows" | "config" | "configuration")
                })
        }
        (NEW_NETWORK_HOST, TrustRule::Network(rule)) => {
            rule.host_match() == HostMatchMode::Exact && rule.category() == TrustCategory::Network
        }
        (DESTRUCTIVE_FILESYSTEM, TrustRule::Filesystem(rule)) => {
            rule.operations.iter().any(|operation| {
                matches!(
                    operation,
                    FilesystemOperationKind::Delete
                        | FilesystemOperationKind::Rename
                        | FilesystemOperationKind::Chmod
                        | FilesystemOperationKind::Symlink
                )
            })
        }
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(format!("template {template_id:?} does not match resolved rule fields"))
    }
}

fn command_starts_with(mode: MatchMode, argv: &[String], expected: &str) -> bool {
    !argv.is_empty()
        && argv[0] == expected
        && matches!(mode, MatchMode::ArgvExact | MatchMode::ArgvPrefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_ids_are_versioned_and_unique() {
        let ids = [
            VCS_PUSH,
            VCS_MUTATION,
            PACKAGE_PUBLISH,
            PACKAGE_INSTALL_SCRIPTS,
            PRIVILEGE_ESCALATION,
            WORKFLOW_CONFIG_WRITE,
            NEW_NETWORK_HOST,
            DESTRUCTIVE_FILESYSTEM,
        ];
        let unique = ids.into_iter().collect::<std::collections::BTreeSet<_>>();
        assert_eq!(unique.len(), ids.len());
        assert!(ids.into_iter().all(|id| id.ends_with("-v1")));
    }
}
