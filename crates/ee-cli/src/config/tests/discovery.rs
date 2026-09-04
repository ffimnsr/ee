use std::path::{Path, PathBuf};

use super::super::*;
use super::common::*;
// The process cwd is process-global; lock it while mutating.
#[test]
fn xdg_user_config_preferred_over_legacy() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(env.cwd.as_path()).unwrap();
    std::fs::create_dir_all(env.home_dir.as_ref().unwrap()).unwrap();
    std::fs::create_dir_all(env.config_dir.as_ref().unwrap().join("ee")).unwrap();
    std::fs::write(env.home_dir.as_ref().unwrap().join(".ee.toml"), "cursor_line = true\n")
        .unwrap();
    std::fs::write(
        env.config_dir.as_ref().unwrap().join("ee").join("config.toml"),
        "wrap_lines = true\n",
    )
    .unwrap();

    let layers = discover_config_layers_with_env(&env, None).layers;

    assert_eq!(
        layer_paths(&layers),
        vec![env.config_dir.as_ref().unwrap().join("ee").join("config.toml")]
    );

    let settings = load_config_with_env(None, &env);
    assert!(settings.wrap_lines);
    assert!(!settings.cursor_line);
}
#[test]
fn legacy_user_config_is_not_misclassified_as_workspace_config() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let mut env = test_config_environment(temp.path());
    env.home_dir = Some(home.clone());
    env.cwd = home.join("project");
    std::fs::create_dir_all(&env.cwd).unwrap();
    std::fs::create_dir_all(env.config_dir.as_ref().unwrap().join("ee")).unwrap();
    std::fs::write(home.join(".ee.toml"), "[languages.yaml]\nfile_types = [\"yaml\", \"yml\"]\n")
        .unwrap();
    std::fs::write(
        env.config_dir.as_ref().unwrap().join("ee").join("config.toml"),
        "wrap_lines = true\n",
    )
    .unwrap();

    let runtime = runtime_languages_with_env(None, &env);
    let layers = discover_config_layers_with_env(&env, None).layers;

    assert!(!runtime.workspace_overrides.contains_key("yaml"));
    assert_eq!(
        layer_paths(&layers),
        vec![env.config_dir.as_ref().unwrap().join("ee").join("config.toml")]
    );
}
#[test]
fn relative_file_path_discovers_workspace_config_from_cwd() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let file = env.cwd.join("src").join("main.rs");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "fn main() {}\n").unwrap();
    std::fs::write(env.cwd.join(".ee.toml"), "cursor_line = true\n").unwrap();

    let layers = discover_config_layers_with_env(&env, Some(Path::new("src/main.rs"))).layers;

    assert!(layers.iter().any(|layer| {
        layer.kind == ConfigLayerKind::Ancestor && layer.path == env.cwd.join(".ee.toml")
    }));
}
#[test]
fn legacy_user_config_used_when_xdg_missing() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(env.cwd.as_path()).unwrap();
    std::fs::create_dir_all(env.home_dir.as_ref().unwrap()).unwrap();
    std::fs::write(env.home_dir.as_ref().unwrap().join(".ee.toml"), "cursor_line = true\n")
        .unwrap();

    let layers = discover_config_layers_with_env(&env, None).layers;

    assert_eq!(layer_paths(&layers), vec![env.home_dir.as_ref().unwrap().join(".ee.toml")]);

    let settings = load_config_with_env(None, &env);
    assert!(settings.cursor_line);
}
#[test]
fn xi_core_config_dir_prefers_xdg_config_home() {
    let temp = tempfile::tempdir().unwrap();
    let xdg_config_home = temp.path().join("xdg-home");
    let _guard = super::super::TestEnvVarGuard::set("XDG_CONFIG_HOME", &xdg_config_home);

    assert_eq!(xi_core_config_dir(), Some(xdg_config_home.join("ee")));
}
#[test]
fn bundled_runtime_root_prefers_env_then_release_layouts() {
    let fallback = Path::new("/tmp/runtime-fallback");
    let windows_exe = Path::new("C:/Program Files/ee/ee.exe");

    assert_eq!(
        resolve_bundled_runtime_root(
            Some(Path::new("/custom/runtime")),
            Some(Path::new("/opt/ee/bin/ee")),
            fallback
        ),
        PathBuf::from("/custom/runtime")
    );
    assert_eq!(
        resolve_bundled_runtime_root(None, Some(Path::new("/opt/ee/bin/ee")), fallback),
        PathBuf::from("/opt/ee/share/ee")
    );
    let expected_windows = if cfg!(windows) {
        PathBuf::from("C:/Program Files/ee/runtime")
    } else {
        fallback.join("runtime")
    };
    assert_eq!(resolve_bundled_runtime_root(None, Some(windows_exe), fallback), expected_windows);
}
#[test]
fn xi_core_client_extras_dir_uses_bundled_plugin_tree() {
    let temp = tempfile::tempdir().unwrap();
    let runtime_root = temp.path().join("runtime");
    let plugins_dir = runtime_root.join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    let _guard = super::super::TestEnvVarGuard::set("EE_RUNTIME_DIR", &runtime_root);

    assert_eq!(xi_core_client_extras_dir(), Some(plugins_dir));
}
#[test]
fn ancestor_chain_merges_outer_to_inner() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let project = env.cwd.join("project");
    let folder = project.join("folder");
    let file = folder.join("main.rs");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(project.join(".ee.toml"), "cursor_line = true\nindent_size = 2\n").unwrap();
    std::fs::write(folder.join(".ee.toml"), "indent_size = 8\nwrap_lines = true\n").unwrap();

    let settings = load_config_with_env(Some(&file), &env);

    assert!(settings.cursor_line);
    assert!(settings.wrap_lines);
    assert_eq!(settings.indent_size, 8);
}
#[test]
fn root_true_in_folder_stops_user_and_system_layers() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let project = env.cwd.join("project");
    let folder = project.join("folder");
    let file = folder.join("main.rs");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::create_dir_all(env.home_dir.as_ref().unwrap()).unwrap();
    std::fs::create_dir_all(env.config_dir.as_ref().unwrap().join("ee")).unwrap();
    std::fs::create_dir_all(env.system_config_path.parent().unwrap()).unwrap();
    std::fs::write(env.system_config_path.as_path(), "trim_trailing_whitespace = true\n").unwrap();
    std::fs::write(
        env.config_dir.as_ref().unwrap().join("ee").join("config.toml"),
        "insert_final_newline = true\n",
    )
    .unwrap();
    std::fs::write(project.join(".ee.toml"), "cursor_line = true\n").unwrap();
    std::fs::write(folder.join(".ee.toml"), "root = true\nwrap_lines = true\n").unwrap();

    let settings = load_config_with_env(Some(&file), &env);

    assert!(settings.wrap_lines);
    assert!(!settings.cursor_line);
    assert!(!settings.insert_final_newline);
    assert!(!settings.trim_trailing_whitespace);
}
#[test]
fn root_true_in_project_stops_user_and_system_but_keeps_inner_folder() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let project = env.cwd.join("project");
    let folder = project.join("folder");
    let file = folder.join("main.rs");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::create_dir_all(env.config_dir.as_ref().unwrap().join("ee")).unwrap();
    std::fs::create_dir_all(env.system_config_path.parent().unwrap()).unwrap();
    std::fs::write(env.system_config_path.as_path(), "trim_trailing_whitespace = true\n").unwrap();
    std::fs::write(
        env.config_dir.as_ref().unwrap().join("ee").join("config.toml"),
        "insert_final_newline = true\n",
    )
    .unwrap();
    std::fs::write(project.join(".ee.toml"), "root = true\ncursor_line = true\n").unwrap();
    std::fs::write(folder.join(".ee.toml"), "wrap_lines = true\n").unwrap();

    let settings = load_config_with_env(Some(&file), &env);

    assert!(settings.cursor_line);
    assert!(settings.wrap_lines);
    assert!(!settings.insert_final_newline);
    assert!(!settings.trim_trailing_whitespace);
}
#[test]
fn root_true_in_user_stops_system_but_keeps_workspace_layers() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let project = env.cwd.join("project");
    let folder = project.join("folder");
    let file = folder.join("main.rs");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::create_dir_all(env.config_dir.as_ref().unwrap().join("ee")).unwrap();
    std::fs::create_dir_all(env.system_config_path.parent().unwrap()).unwrap();
    std::fs::write(env.system_config_path.as_path(), "trim_trailing_whitespace = true\n").unwrap();
    std::fs::write(
        env.config_dir.as_ref().unwrap().join("ee").join("config.toml"),
        "root = true\ninsert_final_newline = true\n",
    )
    .unwrap();
    std::fs::write(project.join(".ee.toml"), "cursor_line = true\n").unwrap();
    std::fs::write(folder.join(".ee.toml"), "wrap_lines = true\n").unwrap();

    let settings = load_config_with_env(Some(&file), &env);

    assert!(settings.insert_final_newline);
    assert!(settings.cursor_line);
    assert!(settings.wrap_lines);
    assert!(!settings.trim_trailing_whitespace);
}
#[test]
fn lsp_config_root_true_stops_project_discovery() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    let project = env.cwd.join("project");
    let folder = project.join("folder");
    let file = folder.join("main.rs");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::create_dir_all(env.config_dir.as_ref().unwrap().join("ee")).unwrap();
    std::fs::write(
        env.config_dir.as_ref().unwrap().join("ee").join("config.toml"),
        "[lsp.servers.rust]\ncommand = \"rust-analyzer\"\n",
    )
    .unwrap();
    std::fs::write(
        project.join(".ee.toml"),
        "root = true\n[lsp.servers.rust]\ncommand = \"project-rust\"\n",
    )
    .unwrap();
    std::fs::write(folder.join(".ee.toml"), "[lsp.servers.rust]\ncommand = \"inner-rust\"\n")
        .unwrap();

    let settings = load_config_with_env(Some(&file), &env);
    let rust = settings.lsp.servers.get("rust").unwrap();

    assert_eq!(rust.command, "inner-rust");
}
#[test]
fn system_config_is_lowest_priority_external_layer() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(env.cwd.as_path()).unwrap();
    std::fs::create_dir_all(env.system_config_path.parent().unwrap()).unwrap();
    std::fs::write(env.system_config_path.as_path(), "trim_trailing_whitespace = true\n").unwrap();

    let layers = discover_config_layers_with_env(&env, None).layers;
    let settings = load_config_with_env(None, &env);

    assert_eq!(layer_paths(&layers), vec![env.system_config_path.clone()]);
    assert!(settings.trim_trailing_whitespace);
}
#[test]
fn search_report_marks_legacy_as_fallback_when_xdg_missing() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());
    std::fs::create_dir_all(env.cwd.as_path()).unwrap();
    std::fs::create_dir_all(env.home_dir.as_ref().unwrap()).unwrap();
    std::fs::write(env.home_dir.as_ref().unwrap().join(".ee.toml"), "cursor_line = true\n")
        .unwrap();

    let report = config_search_report_with_env(&env, None);
    let legacy =
        report.layers.into_iter().find(|layer| layer.kind == ConfigLayerKind::UserLegacy).unwrap();

    assert!(legacy.loaded);
    assert_eq!(legacy.note.as_deref(), Some("loaded because XDG user config is missing"));
}
#[test]
fn config_scope_paths_use_xdg_for_global_and_cwd_for_local() {
    let temp = tempfile::tempdir().unwrap();
    let env = test_config_environment(temp.path());

    assert_eq!(
        config_path_for_scope_with_env(ConfigScope::Global, &env).unwrap(),
        env.config_dir.as_ref().unwrap().join("ee").join("config.toml")
    );
    assert_eq!(
        config_path_for_scope_with_env(ConfigScope::Local, &env).unwrap(),
        env.cwd.join(".ee.toml")
    );
}
