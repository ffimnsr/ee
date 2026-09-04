//! Editor configuration loading for ee.
//!
//! Settings are resolved by merging layers in priority order (lowest first):
//!   1. built-in defaults
//!   2. `/etc/ee/config.toml`
//!   3. `$XDG_CONFIG_HOME/ee/config.toml` or `~/.config/ee/config.toml`
//!   4. fallback `~/.ee.toml` when XDG user config is missing
//!   5. every ancestor `.ee.toml` from outermost to innermost
//!   6. `.editorconfig` (walked up from the open file, per spec)
//!
//! Later layers override earlier ones for any key that is explicitly set.

mod agents;
mod agents_settings;
mod constants;
mod discovery;
mod editor_settings;
mod editorconfig;
mod init;
mod lsp;
mod mcp;
mod raw;
mod rubber_duck;
mod runtime_languages;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
mod value;
mod web_context;
mod web_context_merge;
mod workspace_memory;

#[allow(unused_imports)]
pub(super) use {
    agents::*, agents_settings::*, constants::*, discovery::*, editor_settings::*, editorconfig::*,
    init::*, lsp::*, mcp::*, raw::*, rubber_duck::*, runtime_languages::*, value::*,
    web_context::*, web_context_merge::*, workspace_memory::*,
};

#[cfg(test)]
#[allow(unused_imports)]
pub(super) use test_support::*;
