//! `ee do agent trust` grant/revoke commands.
use super::args::{ALL_AGENT_TRUST_PROFILES, AgentTrustProfile};
use super::*;

pub(crate) const AGENT_TRUST_GRANT_DURATION: Duration = Duration::from_secs(30 * 24 * 60 * 60);
pub(crate) const AGENT_TRUST_GRANT_MAX_USES: u64 = 10_000;

/// Persists selected application-owned trust profiles for one workspace.
pub(crate) fn grant_agent_trust_profiles_at(
    store: &policy::TrustStore,
    now: SystemTime,
    profiles: &[AgentTrustProfile],
) -> Result<policy::TrustStoreDocument, policy::TrustStoreError> {
    let mut document = store.load()?;
    let scope = policy::TrustRuleScope {
        workspace: *store.workspace(),
        agent: None,
        expires_at: Some(now + AGENT_TRUST_GRANT_DURATION),
        max_uses: Some(AGENT_TRUST_GRANT_MAX_USES),
    };
    document.workspace_enabled = true;

    if profiles.contains(&AgentTrustProfile::McpSafeRead) {
        for (id, transport_identity) in [
            ("agent_trust_mcp_read_stdio", "stdio:ee --mcp-proxy"),
            ("agent_trust_mcp_read_acp", "acp:ee"),
        ] {
            let exists = document.rules.iter().any(|rule| {
                matches!(
                    rule,
                    policy::TrustRule::McpReadProfile(existing)
                        if existing.server == "ee"
                            && existing.transport_identity == transport_identity
                            && existing.tool_schema_version == policy::EE_MCP_SAFE_READ_TOOL_SCHEMA_VERSION
                            && existing.profile == policy::EE_MCP_SAFE_READ_PROFILE
                )
            });
            if !exists {
                document.rules.push(policy::TrustRule::McpReadProfile(
                    policy::McpReadProfileRule {
                        id: id.to_string(),
                        effect: policy::TrustEffect::Allow,
                        scope: scope.clone(),
                        server: "ee".to_string(),
                        transport_identity: transport_identity.to_string(),
                        tool_schema_version: policy::EE_MCP_SAFE_READ_TOOL_SCHEMA_VERSION,
                        profile: policy::EE_MCP_SAFE_READ_PROFILE.to_string(),
                    },
                ));
            }
        }
    }

    for (selected, id, profile) in [
        (AgentTrustProfile::GitReadonly, "agent_trust_git_readonly", "git_readonly"),
        (
            AgentTrustProfile::TerminalReadonly,
            "agent_trust_terminal_readonly",
            policy::TERMINAL_READONLY_PROFILE,
        ),
    ] {
        if profiles.contains(&selected)
            && !document.rules.iter().any(|rule| {
                matches!(rule, policy::TrustRule::Profile(existing) if existing.profile == profile)
            })
        {
            document.rules.push(policy::TrustRule::Profile(policy::ProfileRule {
                id: id.to_string(),
                effect: policy::TrustEffect::Allow,
                scope: scope.clone(),
                profile: profile.to_string(),
            }));
        }
    }
    store.write(&document)?;
    Ok(document)
}

/// Removes selected application-owned trust profiles from one host-local
/// workspace store. The workspace gate and unrelated host-local rules remain.
pub(crate) fn revoke_agent_trust_profiles_at(
    store: &policy::TrustStore,
    now: SystemTime,
    profiles: &[AgentTrustProfile],
) -> Result<usize, policy::TrustStoreError> {
    let mut document = store.load_at(now)?;
    let before = document.rules.len();
    document.rules.retain(|rule| !match rule {
        policy::TrustRule::McpReadProfile(rule) => {
            profiles.contains(&AgentTrustProfile::McpSafeRead)
                && rule.server == "ee"
                && rule.profile == policy::EE_MCP_SAFE_READ_PROFILE
        }
        policy::TrustRule::Profile(rule) => {
            (profiles.contains(&AgentTrustProfile::GitReadonly) && rule.profile == "git_readonly")
                || (profiles.contains(&AgentTrustProfile::TerminalReadonly)
                    && rule.profile == policy::TERMINAL_READONLY_PROFILE)
        }
        _ => false,
    });
    let removed = before.saturating_sub(document.rules.len());
    if removed > 0 {
        store.write(&document)?;
    }
    Ok(removed)
}

pub(crate) fn selected_agent_trust_profiles(
    profiles: &[AgentTrustProfile],
) -> &[AgentTrustProfile] {
    if profiles.is_empty() { ALL_AGENT_TRUST_PROFILES } else { profiles }
}

pub(crate) fn profile_names(profiles: &[AgentTrustProfile]) -> String {
    profiles.iter().map(|profile| profile.name()).collect::<Vec<_>>().join(", ")
}

pub(crate) fn cmd_agent_trust_grant(profiles: &[AgentTrustProfile]) -> io::Result<()> {
    let profiles = selected_agent_trust_profiles(profiles);
    let workspace = std::env::current_dir()?;
    let store = policy::TrustStore::default_for(&workspace).map_err(io::Error::other)?;
    grant_agent_trust_profiles_at(&store, SystemTime::now(), profiles).map_err(io::Error::other)?;
    println!("agent trust granted for current workspace: {}", profile_names(profiles));
    println!("host-local store: {}", store.path().display());
    Ok(())
}

pub(crate) fn cmd_agent_trust_revoke(profiles: &[AgentTrustProfile]) -> io::Result<()> {
    let workspace = std::env::current_dir()?;
    let store = policy::TrustStore::default_for(&workspace).map_err(io::Error::other)?;
    let removed = revoke_agent_trust_profiles_at(&store, SystemTime::now(), profiles)
        .map_err(io::Error::other)?;
    println!(
        "agent trust revoked for current workspace: {} ({removed} rule(s) removed)",
        profile_names(profiles)
    );
    println!("host-local store: {}", store.path().display());
    Ok(())
}
