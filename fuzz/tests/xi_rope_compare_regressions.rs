use arbitrary::{Arbitrary, Unstructured};
use ee_fuzz::{CompareInput, run_compare_input};
use std::fs;
use std::path::PathBuf;

#[test]
fn replay_saved_xi_rope_compare_crashes() {
    let artifacts_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("artifacts/xi_rope_compare");
    let mut artifacts = fs::read_dir(&artifacts_dir)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", artifacts_dir.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("crash-"))
        })
        .collect::<Vec<_>>();
    artifacts.sort();

    // Skip test if no crash artifacts have been recorded
    if artifacts.is_empty() {
        return;
    }

    for artifact in artifacts {
        let bytes = fs::read(&artifact)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", artifact.display()));
        let mut unstructured = Unstructured::new(&bytes);
        let input = CompareInput::arbitrary(&mut unstructured).unwrap_or_else(|err| {
            panic!("failed to decode {} as CompareInput: {err}", artifact.display())
        });
        run_compare_input(input);
    }
}
