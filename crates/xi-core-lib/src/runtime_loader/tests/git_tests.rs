//! Runtime-loader tests: git.
use super::*;

fn run_git_fixture(repo: &Path, args: &[&str]) {
    let status = Command::new("git").arg("-C").arg(repo).args(args).status().unwrap();
    assert!(status.success(), "git {:?} failed in {}", args, repo.display());
}

fn git_output(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git").arg("-C").arg(repo).args(args).output().unwrap();
    assert!(output.status.success(), "git {:?} failed in {}", args, repo.display());
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn create_demo_git_repo(temp_dir: &TempDir) -> (PathBuf, String, String, String) {
    let repo = temp_dir.path().join("demo-repo");
    run_git_fixture(temp_dir.path(), &["init", "demo-repo"]);
    run_git_fixture(&repo, &["config", "user.name", "EE Tests"]);
    run_git_fixture(&repo, &["config", "user.email", "ee-tests@example.com"]);
    run_git_fixture(&repo, &["config", "commit.gpgsign", "false"]);
    run_git_fixture(&repo, &["config", "tag.gpgsign", "false"]);
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::create_dir_all(repo.join("queries")).unwrap();

    fs::write(
        repo.join("tree-sitter.json"),
        r#"{
  "grammars": [
    {
      "name": "Demo",
      "scope": "source.demo",
      "file-types": ["demo"],
      "path": ".",
      "highlights": "queries/highlights.scm",
      "locals": "queries/locals.scm",
      "tags": "queries/tags.scm"
    }
  ]
}"#,
    )
    .unwrap();
    fs::write(repo.join("src").join("parser.c"), "int tree_sitter_demo(void) { return 1; }\n")
        .unwrap();
    fs::write(repo.join("queries").join("highlights.scm"), "((identifier) @variable.first)")
        .unwrap();
    fs::write(repo.join("queries").join("locals.scm"), "((identifier) @local.reference)").unwrap();
    fs::write(repo.join("queries").join("tags.scm"), "((identifier) @definition.function)")
        .unwrap();
    run_git_fixture(&repo, &["add", "."]);
    run_git_fixture(&repo, &["commit", "-m", "initial"]);
    let first_rev = git_output(&repo, &["rev-parse", "HEAD"]);
    run_git_fixture(&repo, &["tag", "-a", "v1.0.0", "-m", "v1.0.0"]);

    fs::write(repo.join("queries").join("highlights.scm"), "((identifier) @variable.second)")
        .unwrap();
    run_git_fixture(&repo, &["add", "."]);
    run_git_fixture(&repo, &["commit", "-m", "branch update"]);
    let branch_name = git_output(&repo, &["branch", "--show-current"]);
    let second_rev = git_output(&repo, &["rev-parse", "HEAD"]);

    (repo, first_rev, branch_name, second_rev)
}

fn demo_git_loader(repo: &Path, ref_kind: &str, ref_value: &str, symbol: &str) -> RuntimeLoader {
    let languages = Languages::new(&[language_definition("Demo", &["demo"])]);
    let roots = RuntimeRoots::new(repo.join("bundle"), repo.join("user"), None);
    let mut loader = RuntimeLoader::new(roots, Vec::new()).unwrap();
    let mut overrides = RuntimeLanguageOverrides::new();
    let source = match ref_kind {
        "branch" => RuntimeGrammarGitSource {
            url: repo.display().to_string(),
            branch: Some(ref_value.to_string()),
            tag: None,
            rev: None,
        },
        "tag" => RuntimeGrammarGitSource {
            url: repo.display().to_string(),
            branch: None,
            tag: Some(ref_value.to_string()),
            rev: None,
        },
        "rev" => RuntimeGrammarGitSource {
            url: repo.display().to_string(),
            branch: None,
            tag: None,
            rev: Some(ref_value.to_string()),
        },
        other => panic!("unsupported ref kind {other}"),
    };
    overrides.insert(
        "Demo".to_string(),
        RuntimeLanguageConfig {
            grammar: Some(RuntimeGrammarConfig {
                library: Some(String::from("tree-sitter-demo")),
                symbol: Some(symbol.to_string()),
                source: Some(RuntimeGrammarSource::Git(source)),
            }),
            supported_query_kinds: Some(BTreeSet::from([
                RuntimeQueryKind::Highlights,
                RuntimeQueryKind::Locals,
                RuntimeQueryKind::Tags,
            ])),
            ..RuntimeLanguageConfig::default()
        },
    );
    loader.reload_merged_languages(&languages, &overrides, None).unwrap();
    loader
}

#[test]
fn runtime_loader_fetches_git_branch_source_and_reuses_checkout() {
    let _guard = env_lock();
    let temp_dir = TempDir::new().unwrap();
    let (repo, _tag_rev, branch_name, branch_rev) = create_demo_git_repo(&temp_dir);
    let loader = demo_git_loader(&repo, "branch", &branch_name, "tree_sitter_demo");
    let source_root = temp_dir.path().join("sources");

    let fetched =
        loader.fetch_grammar_sources(&[String::from("Demo")], false, &source_root, false).unwrap();
    assert_eq!(fetched[0].resolved_rev.as_deref(), Some(branch_rev.as_str()));
    assert!(fetched[0].source_pin.contains(&format!("branch:{branch_name}")));

    fs::write(fetched[0].source_dir.join("cache-marker"), "keep\n").unwrap();
    let fetched_again =
        loader.fetch_grammar_sources(&[String::from("Demo")], false, &source_root, false).unwrap();
    assert_eq!(fetched_again[0].resolved_rev.as_deref(), Some(branch_rev.as_str()));
    assert!(fetched_again[0].source_dir.join("cache-marker").exists());
}

#[test]
fn runtime_loader_fetches_git_tag_source_with_resolved_commit() {
    let _guard = env_lock();
    let temp_dir = TempDir::new().unwrap();
    let (repo, tag_rev, _branch_name, _branch_rev) = create_demo_git_repo(&temp_dir);
    let loader = demo_git_loader(&repo, "tag", "v1.0.0", "tree_sitter_demo");

    let fetched = loader
        .fetch_grammar_sources(
            &[String::from("Demo")],
            false,
            &temp_dir.path().join("sources"),
            false,
        )
        .unwrap();

    assert_eq!(fetched[0].resolved_rev.as_deref(), Some(tag_rev.as_str()));
    assert!(fetched[0].source_pin.contains("tag:v1.0.0"));
}

#[test]
fn runtime_loader_fetches_git_rev_source_with_exact_commit() {
    let _guard = env_lock();
    let temp_dir = TempDir::new().unwrap();
    let (repo, tag_rev, _branch_name, _branch_rev) = create_demo_git_repo(&temp_dir);
    let loader = demo_git_loader(&repo, "rev", &tag_rev, "tree_sitter_demo");

    let fetched = loader
        .fetch_grammar_sources(
            &[String::from("Demo")],
            false,
            &temp_dir.path().join("sources"),
            false,
        )
        .unwrap();

    assert_eq!(fetched[0].resolved_rev.as_deref(), Some(tag_rev.as_str()));
}

#[test]
fn runtime_loader_rejects_missing_git_ref() {
    let _guard = env_lock();
    let temp_dir = TempDir::new().unwrap();
    let (repo, _tag_rev, _branch_name, _branch_rev) = create_demo_git_repo(&temp_dir);
    let loader = demo_git_loader(&repo, "tag", "missing-tag", "tree_sitter_demo");

    let error = loader
        .fetch_grammar_sources(
            &[String::from("Demo")],
            false,
            &temp_dir.path().join("sources"),
            false,
        )
        .unwrap_err();

    assert!(error.to_string().contains("missing tag `missing-tag`"));
}

#[test]
fn runtime_loader_builds_runtime_assets_from_git_sources_and_manifest_queries() {
    let _guard = env_lock();
    let temp_dir = TempDir::new().unwrap();
    let (repo, _tag_rev, branch_name, branch_rev) = create_demo_git_repo(&temp_dir);
    let loader = demo_git_loader(&repo, "branch", &branch_name, "tree_sitter_demo");

    let built = loader
        .build_runtime_assets(
            &[String::from("Demo")],
            false,
            &temp_dir.path().join("sources"),
            &temp_dir.path().join("runtime"),
            true,
            true,
        )
        .unwrap();

    assert_eq!(built[0].resolved_rev.as_deref(), Some(branch_rev.as_str()));
    assert!(built[0].grammar_path.exists());
    assert!(built[0].query_paths.iter().any(|path| {
        path.ends_with(Path::new(&runtime_query_dir_name("Demo")).join("highlights.scm"))
    }));
    assert!(
        temp_dir
            .path()
            .join("runtime")
            .join("queries")
            .join(runtime_query_dir_name("Demo"))
            .join("tags.scm")
            .exists()
    );
}

#[test]
fn runtime_loader_build_fails_when_git_source_missing_parser() {
    let _guard = env_lock();
    let temp_dir = TempDir::new().unwrap();
    let (repo, _tag_rev, branch_name, _branch_rev) = create_demo_git_repo(&temp_dir);
    fs::remove_file(repo.join("src").join("parser.c")).unwrap();
    run_git_fixture(&repo, &["add", "-u"]);
    run_git_fixture(&repo, &["commit", "-m", "remove parser"]);
    let loader = demo_git_loader(&repo, "branch", &branch_name, "tree_sitter_demo");

    let error = loader
        .build_runtime_assets(
            &[String::from("Demo")],
            false,
            &temp_dir.path().join("sources"),
            &temp_dir.path().join("runtime"),
            true,
            true,
        )
        .unwrap_err();

    assert_eq!(error.kind(), RuntimeOperationErrorKind::GrammarSource);
    assert!(error.to_string().contains("missing parser source"));
}

#[test]
fn runtime_loader_build_fails_for_bad_git_tree_sitter_manifest() {
    let _guard = env_lock();
    let temp_dir = TempDir::new().unwrap();
    let (repo, _tag_rev, branch_name, _branch_rev) = create_demo_git_repo(&temp_dir);
    fs::write(repo.join("tree-sitter.json"), "{not json\n").unwrap();
    run_git_fixture(&repo, &["add", "tree-sitter.json"]);
    run_git_fixture(&repo, &["commit", "-m", "break manifest"]);
    let loader = demo_git_loader(&repo, "branch", &branch_name, "tree_sitter_demo");

    let error = loader
        .build_runtime_assets(
            &[String::from("Demo")],
            false,
            &temp_dir.path().join("sources"),
            &temp_dir.path().join("runtime"),
            true,
            true,
        )
        .unwrap_err();

    assert!(error.to_string().contains("failed parsing tree-sitter manifest"));
}

#[test]
fn runtime_loader_build_fails_for_grammar_symbol_mismatch() {
    let _guard = env_lock();
    let temp_dir = TempDir::new().unwrap();
    let (repo, _tag_rev, branch_name, _branch_rev) = create_demo_git_repo(&temp_dir);
    let loader = demo_git_loader(&repo, "branch", &branch_name, "tree_sitter_not_demo");
    let original_host = env::var_os("HOST");
    let original_target = env::var_os("TARGET");

    unsafe {
        env::remove_var("HOST");
        env::remove_var("TARGET");
    }

    let error = loader
        .build_runtime_assets(
            &[String::from("Demo")],
            false,
            &temp_dir.path().join("sources"),
            &temp_dir.path().join("runtime"),
            true,
            false,
        )
        .unwrap_err();

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
    }

    assert!(matches!(
        error.kind(),
        RuntimeOperationErrorKind::GrammarSource | RuntimeOperationErrorKind::RuntimeAsset
    ));
    assert!(!error.to_string().trim().is_empty());
}
