use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, Utc};

fn main() {
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR missing"));
    let repo_root = manifest_dir
        .ancestors()
        .nth(2)
        .expect("workspace layout should include repo root")
        .to_path_buf();

    let hash = git_output(&repo_root, ["rev-parse", "--short", "HEAD"])
        .unwrap_or_else(|| "unknown".to_owned());
    let commit_date = git_output(&repo_root, ["show", "-s", "--format=%cI", "HEAD"])
        .unwrap_or_else(|| "unknown".to_owned());
    let dirty = git_is_dirty(&repo_root);
    let cargo_profile = std::env::var("PROFILE").unwrap_or_else(|_| "unknown".to_owned());
    let rustc_version = rustc_version();
    let build_date = build_date();
    let pkg_version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".to_owned());
    let dirty_suffix = if dirty { " dirty" } else { "" };
    let long_version = format!(
        "{pkg_version} (git {hash}{dirty_suffix}, commit {commit_date}, built {build_date}, profile {cargo_profile}, {rustc_version})"
    );

    println!("cargo:rustc-env=EE_GIT_HASH={hash}");
    println!("cargo:rustc-env=EE_GIT_COMMIT_DATE={commit_date}");
    println!("cargo:rustc-env=EE_GIT_DIRTY={}", if dirty { "true" } else { "false" });
    println!("cargo:rustc-env=EE_CARGO_PROFILE={cargo_profile}");
    println!("cargo:rustc-env=EE_RUSTC_VERSION={rustc_version}");
    println!("cargo:rustc-env=EE_BUILD_DATE={build_date}");
    println!("cargo:rustc-env=EE_LONG_VERSION={long_version}");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=src");
    emit_git_reruns(&repo_root);
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    println!("cargo:rerun-if-env-changed=PROFILE");
    println!("cargo:rerun-if-env-changed=RUSTC");
}

fn build_date() -> String {
    std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .and_then(|seconds| DateTime::<Utc>::from_timestamp(seconds, 0))
        .unwrap_or_else(Utc::now)
        .to_rfc3339()
}

fn git_output<const N: usize>(repo_root: &Path, args: [&str; N]) -> Option<String> {
    Command::new("git").args(args).current_dir(repo_root).output().ok().and_then(|output| {
        if output.status.success() {
            String::from_utf8(output.stdout).ok().map(|value| value.trim().to_owned())
        } else {
            None
        }
    })
}

fn git_is_dirty(repo_root: &Path) -> bool {
    git_output(repo_root, ["status", "--short"]).map(|output| !output.is_empty()).unwrap_or(false)
}

fn rustc_version() -> String {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
    Command::new(rustc)
        .arg("--version")
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                String::from_utf8(output.stdout).ok().map(|value| value.trim().to_owned())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "rustc unknown".to_owned())
}

fn emit_git_reruns(repo_root: &Path) {
    let git_dir = repo_root.join(".git");
    println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
    println!("cargo:rerun-if-changed={}", git_dir.join("index").display());
    let refs_dir = git_dir.join("refs");
    if refs_dir.is_dir() {
        println!("cargo:rerun-if-changed={}", refs_dir.display());
    }
}
