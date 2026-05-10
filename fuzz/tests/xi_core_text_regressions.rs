use arbitrary::{Arbitrary, Unstructured};
use ee_fuzz::{CoreTextInput, run_core_text_input};
use std::fs;
use std::path::PathBuf;

#[test]
fn replay_saved_xi_core_text_crashes() {
    let artifacts_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("artifacts/xi_core_text");
    let Ok(entries) = fs::read_dir(&artifacts_dir) else {
        return;
    };
    let mut artifacts = entries
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("crash-"))
        })
        .collect::<Vec<_>>();
    artifacts.sort();

    if artifacts.is_empty() {
        return;
    }

    for artifact in artifacts {
        let bytes = fs::read(&artifact)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", artifact.display()));
        let mut unstructured = Unstructured::new(&bytes);
        let input = CoreTextInput::arbitrary(&mut unstructured).unwrap_or_else(|err| {
            panic!("failed to decode {} as CoreTextInput: {err}", artifact.display())
        });
        run_core_text_input(input);
    }
}
