//! Guards the repository rule: the frontend is Rust (plus a static shell).
//!
//! Authored `.js`, `.ts`, `.jsx`, `.tsx`, `package.json`, and any bundler
//! config fail this test. The only permitted generated artifacts live under
//! `dist/` (wasm-bindgen output and the copied shell).

use std::path::{Path, PathBuf};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn walk(current: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(current).expect("read dir") {
        let entry = entry.expect("entry");
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if path.is_dir() {
            if matches!(name.as_str(), "target" | "dist" | ".git") {
                continue;
            }
            walk(&path, files);
            continue;
        }
        files.push(path);
    }
}

#[test]
fn only_rust_and_static_shell_files_are_authored() {
    let root = manifest_dir();
    let mut files = Vec::new();
    walk(&root, &mut files);
    let allowed_root = ["Cargo.toml", "README.md", "index.html", "style.css"];
    for path in files {
        let relative = path.strip_prefix(&root).expect("relative");
        let relative_text = relative.to_string_lossy().replace('\\', "/");
        if relative_text.starts_with("src/") || relative_text.starts_with("tests/") {
            assert_eq!(
                path.extension().and_then(|extension| extension.to_str()),
                Some("rs"),
                "{relative_text} must be Rust"
            );
            continue;
        }
        assert!(
            allowed_root.contains(&relative_text.as_str()),
            "unexpected authored file {relative_text}; only Rust plus the static shell is allowed"
        );
    }
}

#[test]
fn no_authored_javascript_or_typescript_anywhere_outside_dist() {
    let root = manifest_dir();
    let mut files = Vec::new();
    walk(&root, &mut files);
    for path in files {
        let relative = path.strip_prefix(&root).expect("relative");
        let text = relative.to_string_lossy().replace('\\', "/");
        let extension =
            path.extension().and_then(|extension| extension.to_str()).unwrap_or_default();
        assert!(
            !matches!(extension, "js" | "mjs" | "ts" | "tsx" | "jsx"),
            "authored script source {text} is forbidden; wasm-bindgen output belongs in dist/"
        );
        assert_ne!(
            path.file_name().and_then(|name| name.to_str()),
            Some("package.json"),
            "the frontend must not use a node package manifest"
        );
    }
}

#[test]
fn wasm_dist_respects_the_size_cap_when_present() {
    let dist = manifest_dir().join("dist");
    let wasm = dist.join("ee_web_bg.wasm");
    let Ok(metadata) = std::fs::metadata(&wasm) else {
        // The release artifact is a build gate (`scripts/build-web-frontend.sh`),
        // not a test fixture; skip when it has not been built.
        return;
    };
    assert!(
        metadata.len() <= 2 * 1024 * 1024,
        "{} is {} bytes; the wasm cap is 2 MiB",
        wasm.display(),
        metadata.len()
    );
}
