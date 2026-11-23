---
id: a0f08a98c1b1f9cbde513d0b3f2fdab2
label: hook:mcp:stop
source_type: hook_event
session_id: 96ad84220b620129f2575182a253e5490d6991f0d249559734d4bca6913ec159
agent_id: null
content_type: application/json
identity_kind: artifact_label
identity_value: /home/pastel/Projects/ee:hook:mcp:stop
repo_roots:
  - /home/pastel/Projects/ee
repo_ids: []
created_at: 2026-11-23T16:11:09.426454292Z
---
{"summary":"Fixed relative workspace-root trust warning; added manifest choices/sections/advanced setup UX, masked secret input, agent/agent-critic defaults, and hardcoded agent pane display name. Validated with cargo check, targeted tests, clippy, fmt.","changed_files":["crates/ee-cli/src/agent_setup.rs","crates/ee-cli/src/agent_setup_prompt.rs","crates/ee-cli/src/setup_prompt.rs","crates/ee-cli/src/config/editorconfig.rs","crates/ee-cli/src/config/agents_settings.rs","crates/ee-cli/src/secrets/cli.rs","crates/ee-cli/src/app/agent_pane/app_events.rs","crates/ee-cli/src/app/agent_pane/format.rs","crates/ee-cli/src/app/agent_pane/constants.rs","crates/ee-cli/src/ui/render_agents.rs","crates/ee-agent-protocol/src/setup.rs","crates/ee-agent-opencode/src/config/manifest.rs","crates/ee-agent-openrouter/src/config.rs"]}