//! Runtime-loader tests: fetch.
use super::*;

#[test]
fn runtime_loader_fetches_grammar_source_from_cargo_registry() {
    let _guard = env_lock();
    let loader = default_runtime_loader();
    let temp_dir = TempDir::new().unwrap();

    let fetched = loader
        .fetch_grammar_sources(&[String::from("rust")], false, temp_dir.path(), true)
        .unwrap();

    assert_eq!(fetched.len(), 1);
    assert!(fetched[0].source_pin.starts_with("crate:"));
    assert_eq!(fetched[0].resolved_rev, None);
    assert!(fetched[0].source_dir.join("tree-sitter.json").exists());
    assert!(fetched[0].source_dir.join("src").join("parser.c").exists());
}

#[test]
fn runtime_loader_fetches_versioned_grammar_without_workspace_dependency_edit() {
    let _guard = env_lock();
    let temp_dir = TempDir::new().unwrap();
    let cargo_home = temp_dir.path().join("cargo-home");
    let registry_source =
        cargo_home.join("registry").join("src").join("test-index").join("tree-sitter-demo-1.2.3");
    fs::create_dir_all(registry_source.join("src")).unwrap();

    let cargo_script = temp_dir.path().join("fake-cargo.sh");
    fs::write(
        &cargo_script,
        format!(
            "#!/bin/sh\nset -eu\nmanifest=\"\"\nwhile [ \"$#\" -gt 0 ]; do\n  case \"$1\" in\n    --manifest-path) manifest=\"$2\"; shift 2 ;;\n    *) shift ;;\n  esac\ndone\n[ -n \"$manifest\" ]\ngrep -q 'tree-sitter-demo = \"=1.2.3\"' \"$manifest\"\nmkdir -p \"{}\"\nprintf '{{\"grammars\":[{{\"name\":\"Demo\",\"scope\":\"source.demo\",\"file-types\":[\"demo\"],\"path\":\".\"}}],\"metadata\":{{\"version\":\"1.2.3\"}}}}' > \"{}/tree-sitter.json\"\nprintf 'int tree_sitter_demo(void) {{ return 0; }}\n' > \"{}/src/parser.c\"\n",
            registry_source.display(),
            registry_source.display(),
            registry_source.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&cargo_script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&cargo_script, permissions).unwrap();
    }

    let languages = Languages::new(&[language_definition("Demo", &["demo"])]);
    let roots =
        RuntimeRoots::new(temp_dir.path().join("bundle"), temp_dir.path().join("user"), None);
    let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();
    let mut overrides = RuntimeLanguageOverrides::new();
    overrides.insert(
        "Demo".to_string(),
        RuntimeLanguageConfig {
            grammar: Some(runtime_grammar_config("tree-sitter-demo", "tree_sitter_demo", "1.2.3")),
            ..RuntimeLanguageConfig::default()
        },
    );
    loader.reload_merged_languages(&languages, &overrides, None).unwrap();

    let original_cargo_home = env::var_os("CARGO_HOME");
    let original_cargo = env::var_os("CARGO");
    unsafe {
        env::set_var("CARGO_HOME", &cargo_home);
        env::set_var("CARGO", &cargo_script);
    }

    let fetched = loader
        .fetch_grammar_sources(
            &[String::from("Demo")],
            false,
            temp_dir.path().join("sources").as_path(),
            true,
        )
        .unwrap();

    unsafe {
        if let Some(value) = original_cargo_home {
            env::set_var("CARGO_HOME", value);
        } else {
            env::remove_var("CARGO_HOME");
        }
        if let Some(value) = original_cargo {
            env::set_var("CARGO", value);
        } else {
            env::remove_var("CARGO");
        }
    }

    assert_eq!(fetched[0].crate_name, "tree-sitter-demo");
    assert_eq!(fetched[0].resolved_rev, None);
    assert!(fetched[0].source_dir.join("tree-sitter.json").exists());
    assert!(fetched[0].source_dir.join("src").join("parser.c").exists());
}

#[test]
fn runtime_loader_builds_runtime_assets_from_fetched_sources() {
    let _guard = env_lock();
    let loader = default_runtime_loader();
    let temp_dir = TempDir::new().unwrap();
    let source_root = temp_dir.path().join("sources");
    let output_root = temp_dir.path().join("runtime");
    let cache_home = temp_dir.path().join("cache");
    fs::create_dir_all(&cache_home).unwrap();
    let original_host = env::var_os("HOST");
    let original_target = env::var_os("TARGET");
    let original_home = env::var_os("HOME");
    let original_xdg_cache_home = env::var_os("XDG_CACHE_HOME");

    unsafe {
        env::remove_var("HOST");
        env::remove_var("TARGET");
        env::set_var("HOME", temp_dir.path());
        env::set_var("XDG_CACHE_HOME", &cache_home);
    }

    let built = loader.build_runtime_assets(
        &[String::from("rust")],
        false,
        &source_root,
        &output_root,
        true,
        false,
    );

    unsafe {
        if let Some(value) = original_host {
            env::set_var("HOST", value);
        } else {
            env::remove_var("HOST");
        }
        if let Some(value) = original_target {
            env::set_var("TARGET", value);
        } else {
            env::remove_var("TARGET");
        }
        if let Some(value) = original_home {
            env::set_var("HOME", value);
        } else {
            env::remove_var("HOME");
        }
        if let Some(value) = original_xdg_cache_home {
            env::set_var("XDG_CACHE_HOME", value);
        } else {
            env::remove_var("XDG_CACHE_HOME");
        }
    }

    let built = built.unwrap();

    assert_eq!(built.len(), 1);
    assert!(built[0].source_pin.starts_with("crate:"));
    assert_eq!(built[0].resolved_rev, None);
    assert!(built[0].grammar_path.exists());
    assert!(
        built[0]
            .query_paths
            .iter()
            .any(|path| path.ends_with(Path::new("rust").join("highlights.scm")))
    );
    assert!(
        built[0]
            .query_paths
            .iter()
            .any(|path| path.ends_with(Path::new("rust").join("indents.scm")))
    );
    assert!(output_root.join("queries").join("rust").join("indents.scm").exists());
    assert!(output_root.join("queries").join("toml").join("highlights.scm").exists());
    assert!(output_root.join("queries").join("graphql").join("highlights.scm").exists());
}

#[test]
fn runtime_loader_builds_runtime_assets_without_host_load_validation() {
    let _guard = env_lock();
    let loader = default_runtime_loader();
    let temp_dir = TempDir::new().unwrap();
    let source_root = temp_dir.path().join("sources");
    let output_root = temp_dir.path().join("runtime");

    let built = loader
        .build_runtime_assets(
            &[String::from("rust")],
            false,
            &source_root,
            &output_root,
            true,
            true,
        )
        .unwrap();

    assert_eq!(built.len(), 1);
    assert!(built[0].grammar_path.exists());
}
