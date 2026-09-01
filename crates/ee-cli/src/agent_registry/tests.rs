use super::*;

fn registry_result(json: &str) -> Result<Vec<RegistryAgent>, String> {
    let registry: Registry = serde_json::from_str(json).expect("registry JSON");
    validate_registry(registry)
}

fn registry(json: &str) -> Vec<RegistryAgent> {
    registry_result(json).expect("valid registry")
}

fn binary(archive: &str, cmd: &str, sha256: Option<&str>) -> BinaryDistribution {
    BinaryDistribution {
        archive: archive.to_owned(),
        cmd: cmd.to_owned(),
        args: Vec::new(),
        env: BTreeMap::new(),
        sha256: sha256.map(str::to_owned),
    }
}

fn install_manifest(directory: &Path, metadata: BinaryCacheMetadata) -> InstallManifest {
    InstallManifest {
        schema: INSTALL_MANIFEST_SCHEMA,
        metadata,
        files: snapshot_install_files(directory).unwrap(),
    }
}

#[cfg(unix)]
fn write_executable(path: &Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt as _;

    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn registry_accepts_well_formed_v1_and_rejects_other_versions() {
    for version in ["1.0.0", "1.12.003"] {
        assert!(validate_registry_version(version).is_ok(), "rejected {version}");
    }
    for version in ["2.0.0", "1", "1.2", "1.2.3.4", "1.x.0", "01.0.0"] {
        assert!(validate_registry_version(version).is_err(), "accepted {version}");
    }
}

#[test]
fn registry_keeps_valid_peers_and_skips_malformed_agents() {
    let json = r#"{"version":"1.1.0","agents":[
            {"id":"valid-agent","name":"Valid","description":"works","version":"01.2.003","distribution":{"npx":{"package":"valid-agent@01.2.003"}}},
            {"id":"Bad_Id","name":"Bad","description":"bad id","version":"1.2.3","distribution":{"npx":{"package":"bad@1.2.3"}}},
            {"id":"empty-name","name":" ","description":"bad","version":"1.2.3","distribution":{"npx":{"package":"empty@1.2.3"}}},
            {"id":"empty-description","name":"Bad","description":" ","version":"1.2.3","distribution":{"npx":{"package":"empty@1.2.3"}}},
            {"id":"bad-version","name":"Bad","description":"bad","version":"1.2","distribution":{"npx":{"package":"bad@1.2"}}}
        ]}"#;
    let agents = registry(json);
    assert_eq!(agents.iter().map(|agent| agent.id.as_str()).collect::<Vec<_>>(), ["valid-agent"]);
}

#[test]
fn malformed_entries_do_not_poison_valid_peers_or_claim_ids() {
    let json = r#"{"version":"1.0.0","agents":[
            {"id":"same","name":"","description":"","version":"bad","distribution":{}},
            {"id":"wrong-type","name":"Wrong","description":"bad args","version":"1.0.0","distribution":{"npx":{"package":"wrong@1.0.0","args":"not-an-array"}}},
            {"id":"same","name":"Valid","description":"works","version":"1.0.0","distribution":{"npx":{"package":"same@1.0.0"}}}
        ]}"#;
    assert_eq!(registry(json).iter().map(|agent| agent.id.as_str()).collect::<Vec<_>>(), ["same"]);
}

#[test]
fn duplicate_valid_ids_remain_fatal() {
    let json = r#"{"version":"1.0.0","agents":[
            {"id":"same","name":"One","description":"works","version":"1.0.0","distribution":{"npx":{"package":"same@1.0.0"}}},
            {"id":"same","name":"Two","description":"works","version":"1.0.0","distribution":{"npx":{"package":"same@1.0.0"}}}
        ]}"#;
    assert!(registry_result(json).is_err());
}

#[test]
fn only_selected_platform_distribution_is_validated() {
    let platform = current_platform().unwrap();
    let json = format!(
        r#"{{"version":"1.0.0","agents":[{{
                "id":"platform-agent","name":"Platform","description":"works","version":"1.0.0",
                "distribution":{{"binary":{{
                    "{platform}":{{"archive":"https://example.com/agent.zip?download=.dmg","cmd":"agent"}},
                    "foreign-broken":{{"archive":"http://insecure.example/a.dmg","cmd":"../bad","sha256":"bad"}}
                }}}}
            }}]}}"#
    );
    assert_eq!(registry(&json).len(), 1);
}

#[test]
fn live_registry_distribution_forms_remain_accepted() {
    let platform = current_platform().unwrap();
    let json = format!(
        r#"{{"version":"1.0.0","agents":[
                {{"id":"codex-acp","name":"Codex","description":"live scoped npx","version":"1.7.0","distribution":{{"npx":{{"package":"@agentclientprotocol/codex-acp@1.7.0"}}}}}},
                {{"id":"fast-agent","name":"Fast","description":"live uvx equals","version":"0.10.1","distribution":{{"uvx":{{"package":"fast-agent-acp==0.10.1"}}}}}},
                {{"id":"minion-code","name":"Minion","description":"live uvx at","version":"0.1.44","distribution":{{"uvx":{{"package":"minion-code@0.1.44"}}}}}},
                {{"id":"binary-agent","name":"Binary","description":"live archive","version":"1.0.0","distribution":{{"binary":{{"{platform}":{{"archive":"https://example.com/agent.tar.bz2","cmd":"./agent"}}}}}}}}
            ]}}"#
    );
    assert_eq!(registry(&json).len(), 4);
}

#[test]
fn package_runners_require_exact_agent_version_pin() {
    for (runner, package) in [
        ("npx", "name@1.2.3"),
        ("npx", "@scope/name@1.2.3"),
        ("uvx", "name==1.2.3"),
        ("uvx", "name@1.2.3"),
    ] {
        let distribution = PackageDistribution {
            package: package.to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
        };
        assert!(validate_package(runner, &distribution, "1.2.3").is_ok());
        assert!(validate_package(runner, &distribution, "1.2.4").is_err());
    }
    for (runner, package) in [
        ("npx", "name"),
        ("npx", "@scope/name"),
        ("npx", "https://example.com/agent.tgz@1.2.3"),
        ("npx", "git+https://example.com/agent@1.2.3"),
        ("npx", "name@npm:other@1.2.3"),
        ("uvx", "name"),
        ("uvx", "https://example.com/agent.whl@1.2.3"),
    ] {
        let distribution = PackageDistribution {
            package: package.to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
        };
        assert!(
            validate_package(runner, &distribution, "1.2.3").is_err(),
            "accepted {runner} package {package}"
        );
    }
}

#[test]
fn registry_process_metadata_rejects_nul_but_accepts_non_shell_env_names() {
    assert!(
        validate_process_metadata(
            &[String::from("--acp")],
            &BTreeMap::from([(String::from("AGENT-MODE"), String::new())]),
        )
        .is_ok()
    );
    for name in ["", "BAD=NAME", "BAD\0NAME"] {
        assert!(validate_env(&BTreeMap::from([(name.to_owned(), String::new())])).is_err());
    }
    assert!(
        validate_env(&BTreeMap::from([(String::from("GOOD"), String::from("bad\0value"))]))
            .is_err()
    );
    assert!(validate_process_metadata(&[String::from("bad\0arg")], &BTreeMap::new()).is_err());
}

#[test]
fn package_launch_always_uses_exact_registry_package() {
    let package = PackageDistribution {
        package: String::from("@scope/agent@1.2.3"),
        args: vec![String::from("--acp")],
        env: BTreeMap::new(),
    };
    assert_eq!(package_launch_args("npx", &package), ["--yes", "@scope/agent@1.2.3", "--acp"]);
    assert_eq!(package_launch_args("uvx", &package), ["@scope/agent@1.2.3", "--acp"]);
}

#[test]
fn version_probe_requires_exact_version_boundaries() {
    assert!(version_token_matches(b"agent 1.2.3\n", "1.2.3"));
    assert!(version_token_matches(b"agent v1.2.3\n", "1.2.3"));
    for output in [
        b"agent 11.2.30".as_slice(),
        b"agent 1.2.30".as_slice(),
        b"agent 1.2.3.4".as_slice(),
        b"agent 1.2.3-beta".as_slice(),
        b"agent 1.2.3+build".as_slice(),
        b"agent x1.2.3".as_slice(),
    ] {
        assert!(!version_token_matches(output, "1.2.3"));
    }
}

#[cfg(unix)]
#[test]
fn installed_binary_is_reused_only_at_exact_registry_version() {
    let temp = tempfile::tempdir().unwrap();
    let command = temp.path().join("example-agent");
    write_executable(&command, "#!/bin/sh\nprintf '%s\\n' 'example-agent 11.2.30'\n");
    assert!(!command_reports_version(&command, "1.2.3").unwrap());

    write_executable(&command, "#!/bin/sh\nprintf '%s\\n' 'example-agent 1.2.3'\n");
    assert!(command_reports_version(&command, "1.2.3").unwrap());
}

#[cfg(unix)]
#[test]
fn version_probe_kills_descendants_holding_output_pipes() {
    let temp = tempfile::tempdir().unwrap();
    let command = temp.path().join("forking-agent");
    write_executable(&command, "#!/bin/sh\n(sleep 30) &\nprintf '%s\\n' 'forking-agent 1.2.3'\n");
    let started = Instant::now();
    assert!(command_reports_version(&command, "1.2.3").unwrap());
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn binary_cache_identity_covers_all_registry_artifact_metadata() {
    let base = binary("https://example.com/agent.zip", "bin/agent", None);
    let base_metadata = binary_cache_metadata("linux-x86_64", &base);
    let base_identity = binary_cache_identity(&base_metadata);
    let variants = [
        binary_cache_metadata("linux-aarch64", &base),
        binary_cache_metadata(
            "linux-x86_64",
            &binary("https://example.com/other.zip", "bin/agent", None),
        ),
        binary_cache_metadata(
            "linux-x86_64",
            &binary("https://example.com/agent.zip", "bin/agent", Some(&"a".repeat(64))),
        ),
        binary_cache_metadata(
            "linux-x86_64",
            &binary("https://example.com/agent.zip", "other/agent", None),
        ),
    ];
    assert!(variants.iter().all(|variant| binary_cache_identity(variant) != base_identity));
}

#[cfg(unix)]
#[test]
fn cache_reuse_requires_matching_manifest_and_command_digest() {
    let temp = tempfile::tempdir().unwrap();
    let command = Path::new("bin/agent");
    fs::create_dir_all(temp.path().join("bin")).unwrap();
    write_executable(&temp.path().join(command), "original");
    fs::write(temp.path().join("runtime.dat"), "runtime").unwrap();
    let metadata = binary_cache_metadata(
        "linux-x86_64",
        &binary("https://example.com/agent.zip", "bin/agent", None),
    );
    let manifest = install_manifest(temp.path(), metadata.clone());
    write_install_manifest(temp.path(), &manifest).unwrap();
    assert!(validate_cached_install(temp.path(), command, &metadata).unwrap());

    write_executable(&temp.path().join(command), "tampered");
    assert!(!validate_cached_install(temp.path(), command, &metadata).unwrap());
    write_executable(&temp.path().join(command), "original");
    assert!(validate_cached_install(temp.path(), command, &metadata).unwrap());

    fs::write(temp.path().join("runtime.dat"), "tampered").unwrap();
    assert!(!validate_cached_install(temp.path(), command, &metadata).unwrap());
    fs::write(temp.path().join("runtime.dat"), "runtime").unwrap();
    assert!(validate_cached_install(temp.path(), command, &metadata).unwrap());

    fs::write(temp.path().join("unexpected.dat"), "extra").unwrap();
    assert!(!validate_cached_install(temp.path(), command, &metadata).unwrap());
}

#[cfg(unix)]
#[test]
fn cache_reuse_rejects_symlinked_install_or_manifest() {
    use std::os::unix::fs::symlink;

    let parent = tempfile::tempdir().unwrap();
    let actual = parent.path().join("actual");
    fs::create_dir_all(actual.join("bin")).unwrap();
    let command = Path::new("bin/agent");
    write_executable(&actual.join(command), "agent");
    let metadata = binary_cache_metadata(
        "linux-x86_64",
        &binary("https://example.com/agent.zip", "bin/agent", None),
    );
    let manifest = install_manifest(&actual, metadata.clone());
    write_install_manifest(&actual, &manifest).unwrap();

    let linked_install = parent.path().join("linked");
    symlink(&actual, &linked_install).unwrap();
    assert!(!validate_cached_install(&linked_install, command, &metadata).unwrap());

    fs::remove_file(actual.join(INSTALL_MANIFEST)).unwrap();
    let external_manifest = parent.path().join("manifest.json");
    fs::write(&external_manifest, serde_json::to_vec(&manifest).unwrap()).unwrap();
    symlink(&external_manifest, actual.join(INSTALL_MANIFEST)).unwrap();
    assert!(!validate_cached_install(&actual, command, &metadata).unwrap());
}

#[cfg(unix)]
#[test]
fn publishing_does_not_replace_valid_cache() {
    let parent = tempfile::tempdir().unwrap();
    let install = parent.path().join("identity");
    let staged = parent.path().join("staged");
    fs::create_dir_all(install.join("bin")).unwrap();
    fs::create_dir_all(staged.join("bin")).unwrap();
    let command = Path::new("bin/agent");
    let metadata = binary_cache_metadata(
        "linux-x86_64",
        &binary("https://example.com/agent.zip", "bin/agent", None),
    );
    write_executable(&install.join(command), "valid");
    let manifest = install_manifest(&install, metadata.clone());
    write_install_manifest(&install, &manifest).unwrap();
    write_executable(&staged.join(command), "replacement");

    publish_staged_install(&staged, &install, command, &metadata).unwrap();
    assert_eq!(fs::read_to_string(install.join(command)).unwrap(), "valid");
    assert!(staged.exists());
}

#[test]
fn archive_classifier_uses_url_path_and_explicit_allowlist() {
    for (url, expected) in [
        ("https://example.com/a.zip?format=.dmg", ArchiveFormat::Zip),
        ("https://example.com/a.tar.gz?x=1", ArchiveFormat::TarGz),
        ("https://example.com/a.tgz", ArchiveFormat::TarGz),
        ("https://example.com/a.tar.bz2", ArchiveFormat::TarBz2),
        ("https://example.com/a.tbz2", ArchiveFormat::TarBz2),
        ("https://example.com/agent", ArchiveFormat::Raw),
        ("https://example.com/agent.exe", ArchiveFormat::Raw),
        ("https://example.com/agent.bin", ArchiveFormat::Raw),
    ] {
        assert_eq!(classify_archive(url).unwrap(), expected, "wrong format for {url}");
    }
    for suffix in
        ["dmg", "pkg", "deb", "rpm", "msi", "appimage", "tar", "tar.xz", "txz", "7z", "gz", "bz2"]
    {
        let url = format!("https://example.com/agent.{suffix}");
        assert!(classify_archive(&url).is_err(), "accepted {url}");
    }
}

#[test]
fn archive_paths_reject_traversal_windows_roots_and_device_aliases() {
    assert_eq!(safe_relative_path("./bin/agent").unwrap(), PathBuf::from("bin/agent"));
    for path in [
        "../agent",
        "/tmp/agent",
        "C:\\agent.exe",
        "..\\agent.exe",
        "\\\\server\\agent",
        "NUL",
        "bin/Con.txt",
        "aux/agent",
        "COM1.exe",
        "lpt9.log",
        "bin/agent.",
        "bin/agent ",
    ] {
        assert!(safe_relative_path(path).is_err(), "accepted {path}");
    }
}

#[cfg(unix)]
#[test]
fn extracted_files_preserve_only_owner_execute_intent() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = tempfile::tempdir().unwrap();
    let helper = temp.path().join("helper");
    write_archive_file(&mut io::Cursor::new(b"helper"), &helper, 6, Some(0o6755)).unwrap();
    assert_eq!(fs::metadata(&helper).unwrap().permissions().mode() & 0o7777, 0o700);

    let data = temp.path().join("data");
    write_archive_file(&mut io::Cursor::new(b"data"), &data, 4, Some(0o666)).unwrap();
    assert_eq!(fs::metadata(&data).unwrap().permissions().mode() & 0o7777, 0o600);
}

#[test]
fn sha256_validation_requires_exact_hex_digest() {
    assert!(validate_sha256(&"a".repeat(64)).is_ok());
    assert!(validate_sha256(&"g".repeat(64)).is_err());
    assert!(validate_sha256(&"a".repeat(63)).is_err());
}
