//! Secret-like configured and resolved values used for redaction.

use super::*;

impl App {
    /// Secret-like configured values (agent + MCP env/header values whose
    /// keys look secret-like) plus every value resolved from a `secret://`
    /// reference at launch/forward time. References themselves are never
    /// collected, and malformed reference templates are left to fail closed
    /// at resolution rather than leaking as literals.
    pub(crate) fn agents_secret_values(&self) -> Vec<String> {
        let mut secrets = Vec::new();
        for server in self.config.agents.servers.values() {
            for (name, value) in &server.env {
                if ee_agent_host::redact::is_secret_key(name)
                    && !crate::secrets::is_secret_reference_text(&value.raw)
                {
                    secrets.push(value.raw.clone());
                }
            }
        }
        for server in self.config.mcp.servers.values() {
            match server {
                crate::config::McpServerSettings::Stdio { env, .. } => {
                    for (name, value) in env {
                        if ee_agent_host::redact::is_secret_key(name)
                            && !crate::secrets::resolve::value_has_reference(value)
                        {
                            secrets.push(value.raw.clone());
                        }
                    }
                }
                crate::config::McpServerSettings::StreamableHttp { headers, .. } => {
                    for (name, value) in headers {
                        if ee_agent_host::redact::is_secret_key(name)
                            && !crate::secrets::resolve::header_value_has_reference(value)
                        {
                            secrets.push(value.raw.clone());
                        }
                    }
                }
            }
        }
        secrets.extend(self.agents.resolved_secret_values.iter().cloned());
        secrets.sort();
        secrets.dedup();
        secrets
    }
}
