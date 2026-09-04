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

use super::agents_settings::agents_settings_to_toml;
use super::constants::CONFIG_TEMPLATE;
use super::discovery::{
    ConfigEnvironment, ConfigScope, config_path_for_scope_with_env, load_config_with_env,
};
use super::editor_settings::{
    EditorSettings, EndOfLine, IndentStyle, NumberStyle, StatuslineFormat,
};
use super::lsp::lsp_settings_to_toml;
use super::mcp::{mcp_settings_to_toml, validate_agents_mcp_config};
use super::raw::{EeToml, keymap_settings_to_toml};
use super::runtime_languages::{runtime_languages_to_toml, runtime_languages_with_env};
#[cfg(test)]
use super::test_support::test_cwd_lock;
use std::fs;
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};

use schemars::schema_for;
use xi_core_lib::runtime_loader::{
    RuntimeLanguageOverrides, configure_default_runtime_loader_overrides_if_changed,
    validate_runtime_language_overrides,
};

pub(crate) fn resolved_config_with_env(
    file_path: Option<&Path>,
    env: &ConfigEnvironment,
) -> EeToml {
    let settings = load_config_with_env(file_path, env);
    let runtime_languages = runtime_languages_to_toml(runtime_languages_with_env(file_path, env));
    EeToml {
        root: None,
        indent_style: Some(match settings.indent_style {
            IndentStyle::Spaces => String::from("spaces"),
            IndentStyle::Tabs => String::from("tabs"),
        }),
        indent_size: Some(settings.indent_size),
        tab_width: Some(settings.tab_width),
        end_of_line: Some(match settings.end_of_line {
            EndOfLine::Lf => String::from("lf"),
            EndOfLine::CrLf => String::from("crlf"),
            EndOfLine::Cr => String::from("cr"),
        }),
        charset: Some(settings.charset),
        trim_trailing_whitespace: Some(settings.trim_trailing_whitespace),
        insert_final_newline: Some(settings.insert_final_newline),
        auto_indent: Some(settings.auto_indent),
        smart_indent: Some(settings.smart_indent),
        number_style: Some(match settings.number_style {
            NumberStyle::Absolute => String::from("absolute"),
            NumberStyle::Relative => String::from("relative"),
            NumberStyle::RelativeAbsolute => String::from("relative_absolute"),
        }),
        color_column: settings.color_column,
        show_visible_whitespace: Some(settings.show_visible_whitespace),
        scroll_offset: Some(settings.scroll_offset),
        wrap_lines: Some(settings.wrap_lines),
        sign_column: Some(settings.sign_column),
        cursor_line: Some(settings.cursor_line),
        statusline_format: Some(match settings.statusline_format {
            StatuslineFormat::Default => String::from("default"),
            StatuslineFormat::Minimal => String::from("minimal"),
        }),
        lsp: lsp_settings_to_toml(&settings.lsp),
        languages: runtime_languages,
        keymap: keymap_settings_to_toml(&settings.keymap),
        agents: agents_settings_to_toml(&settings.agents),
        mcp: mcp_settings_to_toml(&settings.mcp),
    }
}

pub(crate) fn merged_config_document(file_path: Option<&Path>) -> Result<String, String> {
    let document = resolved_config_with_env(file_path, &ConfigEnvironment::from_process());
    toml::to_string_pretty(&document)
        .map(|mut text| {
            text.push('\n');
            text
        })
        .map_err(|err| format!("cannot render merged config: {err}"))
}

pub(super) fn validate_config_contents(path: &Path, contents: &str) -> Result<(), String> {
    let parsed = toml::from_str::<EeToml>(contents)
        .map_err(|err| format!("Config parse error in {}: {err}", path.display()))?;

    validate_agents_mcp_config(&parsed)
        .map_err(|err| format!("Config validation error in {}: {err}", path.display()))?;

    if parsed.languages.is_empty() {
        return Ok(());
    }

    let is_workspace_layer = path.file_name().is_some_and(|name| name == ".ee.toml");
    let mut user_overrides = RuntimeLanguageOverrides::new();
    let mut workspace_overrides = RuntimeLanguageOverrides::new();
    if is_workspace_layer {
        workspace_overrides = parsed.languages;
    } else {
        user_overrides = parsed.languages;
    }

    validate_runtime_language_overrides(&user_overrides, &workspace_overrides, is_workspace_layer)
        .map_err(|err| format!("Config validation error in {}: {err}", path.display()))
}

pub(super) fn init_config_with_env(
    scope: ConfigScope,
    env: &ConfigEnvironment,
) -> Result<PathBuf, String> {
    let path = config_path_for_scope_with_env(scope, env)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("Cannot create {}: {err}", parent.display()))?;
    }

    let mut file =
        fs::OpenOptions::new().write(true).create_new(true).open(&path).map_err(|err| {
            if err.kind() == ErrorKind::AlreadyExists {
                format!("Config already exists: {}", path.display())
            } else {
                format!("Cannot create {}: {err}", path.display())
            }
        })?;
    file.write_all(CONFIG_TEMPLATE.as_bytes())
        .map_err(|err| format!("Cannot write {}: {err}", path.display()))?;
    Ok(path)
}

pub(crate) fn init_config(scope: ConfigScope) -> Result<PathBuf, String> {
    init_config_with_env(scope, &ConfigEnvironment::from_process())
}

pub(crate) fn configure_runtime_loader_for_file(
    file_path: Option<&Path>,
    workspace_trusted: bool,
) -> Result<(), String> {
    let runtime_languages =
        runtime_languages_with_env(file_path, &ConfigEnvironment::from_process());
    configure_default_runtime_loader_overrides_if_changed(
        runtime_languages.user_overrides,
        runtime_languages.workspace_overrides,
        workspace_trusted,
    )
    .map_err(|error| error.to_string())
}

pub(crate) fn validate_config_file(path: &Path) -> Result<(), String> {
    let contents = std::fs::read_to_string(path)
        .map_err(|err| format!("Cannot read {}: {err}", path.display()))?;
    validate_config_contents(path, &contents)
}

pub(crate) fn config_schema_json() -> Result<String, String> {
    let schema = serde_json::to_value(schema_for!(EeToml))
        .map_err(|err| format!("Cannot serialize config schema: {err}"))?;
    serde_json::to_string_pretty(&schema)
        .map(|mut text| {
            text.push('\n');
            text
        })
        .map_err(|err| format!("Cannot format config schema: {err}"))
}

pub(crate) fn write_config_schema(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Cannot create {}: {err}", parent.display()))?;
    }
    std::fs::write(path, config_schema_json()?)
        .map_err(|err| format!("Cannot write {}: {err}", path.display()))
}

pub(crate) fn check_config_schema(path: &Path) -> Result<(), String> {
    let expected = config_schema_json()?;
    let actual = std::fs::read_to_string(path)
        .map_err(|err| format!("Cannot read {}: {err}", path.display()))?;
    if actual == expected {
        return Ok(());
    }
    Err(format!(
        "Config schema drift detected at {}. Run `cargo run -p ee-cli -- do schema generate`.",
        path.display()
    ))
}

/// Load and merge all config layers for the given open file (if any).
pub(crate) fn load_config(file_path: Option<&Path>) -> EditorSettings {
    #[cfg(test)]
    let _cwd_lock = test_cwd_lock().lock().unwrap();

    load_config_with_env(file_path, &ConfigEnvironment::from_process())
}
