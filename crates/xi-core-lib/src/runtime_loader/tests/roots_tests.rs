//! Runtime-loader tests: roots.
use super::*;

#[test]
fn runtime_roots_follow_directory_contract() {
    let roots = RuntimeRoots::new(
        "/opt/ee/runtime",
        RuntimeRoots::user_root_for_data_dir(Path::new("/tmp/data")),
        Some(PathBuf::from("/work/project/.ee")),
    );

    assert_eq!(roots.user_root(), Path::new("/tmp/data/ee"));
    assert_eq!(
        roots.grammar_dir_for(RuntimeConfigSource::User).as_deref(),
        Some(Path::new("/tmp/data/ee/grammars"))
    );
    assert_eq!(
        roots.query_dir_for(RuntimeConfigSource::User, "rust").as_deref(),
        Some(Path::new("/tmp/data/ee/queries/rust"))
    );
    assert_eq!(
        roots.parser_directories(true),
        vec![
            PathBuf::from("/opt/ee/runtime"),
            PathBuf::from("/tmp/data/ee"),
            PathBuf::from("/work/project/.ee")
        ]
    );
    assert_eq!(
        roots.parser_directories(false),
        vec![PathBuf::from("/opt/ee/runtime"), PathBuf::from("/tmp/data/ee")]
    );
}

#[test]
fn runtime_query_dir_names_are_terminal_friendly() {
    assert_eq!(runtime_query_dir_name("rust"), "rust");
    assert_eq!(runtime_query_dir_name("csharp"), "csharp");
    assert_eq!(runtime_query_dir_name("cpp"), "cpp");
    assert_eq!(runtime_query_dir_name("typescript"), "typescript");
}

#[test]
fn bundled_runtime_root_prefers_env_then_release_layouts() {
    let fallback = Path::new("/tmp/runtime-fallback");
    let windows_exe = Path::new("C:/Program Files/ee/ee.exe");

    assert_eq!(
        resolve_bundled_runtime_root(
            Some(Path::new("/custom/runtime")),
            Some(Path::new("/opt/ee/bin/ee")),
            fallback,
            false,
        ),
        PathBuf::from("/custom/runtime")
    );
    assert_eq!(
        resolve_bundled_runtime_root(None, Some(Path::new("/opt/ee/bin/ee")), fallback, false),
        PathBuf::from("/opt/ee/share/ee")
    );
    assert_eq!(
        resolve_bundled_runtime_root(None, Some(windows_exe), fallback, true,),
        PathBuf::from("C:/Program Files/ee/runtime")
    );
}

#[test]
fn bundled_runtime_root_falls_back_to_existing_source_tree_when_release_layout_missing() {
    let temp = tempfile::tempdir().unwrap();
    let fallback = temp.path().join("fallback");
    let release_exe = temp.path().join("target").join("debug").join("deps").join("ee-tests");
    let source_tree_root = temp.path().join("workspace").join("runtime");
    fs::create_dir_all(&fallback).unwrap();
    fs::create_dir_all(source_tree_root.join("queries")).unwrap();

    let resolved = resolve_existing_bundled_runtime_root(
        None,
        Some(&release_exe),
        &fallback,
        Some(&source_tree_root),
        false,
    );

    assert_eq!(resolved, source_tree_root);
}

#[test]
fn runtime_loading_disabled_reason_tracks_supported_targets() {
    assert_eq!(runtime_loading_disabled_reason_for(true), None);
    assert_eq!(
        runtime_loading_disabled_reason_for(false),
        Some("shared-library runtime grammars are only supported on Linux, macOS, and Windows")
    );
}
