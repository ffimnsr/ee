//! Default sequence bindings.
use super::spec::parse_key_press_spec;
use super::*;

pub(crate) fn default_sequence_bindings() -> &'static Vec<SequenceBinding> {
    static BINDINGS: OnceLock<Vec<SequenceBinding>> = OnceLock::new();
    BINDINGS.get_or_init(build_default_sequence_bindings)
}

pub(crate) fn build_default_sequence_bindings() -> Vec<SequenceBinding> {
    use Action::*;

    fn sequence(keys: &[&str]) -> Vec<KeyPress> {
        keys.iter()
            .map(|key| parse_key_press_spec(key).expect("default key sequence must parse"))
            .collect()
    }

    fn bind(
        modes: &[Mode],
        keys: &[&str],
        action: Action,
        description: &str,
    ) -> Vec<SequenceBinding> {
        let sequence = sequence(keys);
        modes
            .iter()
            .copied()
            .map(|mode| SequenceBinding {
                mode,
                sequence: sequence.clone(),
                action: action.clone(),
                description: description.to_owned(),
            })
            .collect()
    }

    let normal_modes = [Mode::Normal, Mode::Visual, Mode::VisualLine, Mode::VisualBlock];

    let mut bindings = Vec::new();
    bindings.extend(bind(&normal_modes, &["space", "f"], NoOp, "files"));
    bindings.extend(bind(&normal_modes, &["space", "f", "f"], FilePicker, "find files"));
    bindings.extend(bind(
        &normal_modes,
        &["space", "f", "d"],
        FilePickerInCurrentDirectory,
        "files in cwd",
    ));
    bindings.extend(bind(&normal_modes, &["space", "f", "e"], FileExplorer, "file explorer"));
    bindings.extend(bind(&normal_modes, &["space", "f", "r"], ChangedFilePicker, "recent changes"));
    bindings.extend(bind(&normal_modes, &["space", "b"], NoOp, "buffers"));
    bindings.extend(bind(&normal_modes, &["space", "b", "b"], BufferPicker, "switch buffer"));
    bindings.extend(bind(
        &normal_modes,
        &["space", "b", "d"],
        DiagnosticsPicker,
        "buffer diagnostics",
    ));
    bindings.extend(bind(&normal_modes, &["space", "b", "l"], LastPicker, "last picker"));
    bindings.extend(bind(&normal_modes, &["space", "s"], NoOp, "search"));
    bindings.extend(bind(&normal_modes, &["space", "s", "s"], GlobalSearch, "search workspace"));
    bindings.extend(bind(&normal_modes, &["space", "s", "m"], SwiftMotion, "swift motion"));
    bindings.extend(bind(
        &normal_modes,
        &["space", "s", "d"],
        RequestDocumentSymbols,
        "document symbols",
    ));
    bindings.extend(bind(
        &normal_modes,
        &["space", "s", "w"],
        RequestWorkspaceSymbols,
        "workspace symbols",
    ));
    bindings.extend(bind(&normal_modes, &["space", "g"], NoOp, "git"));
    bindings.extend(bind(&normal_modes, &["space", "g", "b"], GitBlame, "git blame"));
    bindings.extend(bind(&normal_modes, &["space", "g", "d"], GitDiff, "git diff"));
    bindings.extend(bind(&normal_modes, &["space", "g", "n"], GitNextHunk, "next hunk"));
    bindings.extend(bind(&normal_modes, &["space", "g", "p"], GitPrevHunk, "previous hunk"));
    bindings.extend(bind(&normal_modes, &["space", "c"], NoOp, "code"));
    bindings.extend(bind(&normal_modes, &["space", "c", "a"], RequestCodeActions, "code actions"));
    bindings.extend(bind(&normal_modes, &["space", "c", "h"], RequestHover, "hover"));
    bindings.extend(bind(&normal_modes, &["space", "c", "d"], RequestDefinition, "definition"));
    bindings.extend(bind(&normal_modes, &["space", "c", "r"], RequestReferences, "references"));
    bindings.extend(bind(
        &normal_modes,
        &["space", "c", "i"],
        RequestImplementation,
        "implementation",
    ));
    bindings.extend(bind(&normal_modes, &["space", "w"], NoOp, "windows"));
    bindings.extend(bind(&normal_modes, &["space", "w", "h"], JumpViewLeft, "focus left"));
    bindings.extend(bind(&normal_modes, &["space", "w", "j"], JumpViewDown, "focus down"));
    bindings.extend(bind(&normal_modes, &["space", "w", "k"], JumpViewUp, "focus up"));
    bindings.extend(bind(&normal_modes, &["space", "w", "l"], JumpViewRight, "focus right"));
    bindings.extend(bind(&normal_modes, &["space", "w", "r"], RotateView, "rotate windows"));
    bindings.extend(bind(&normal_modes, &["space", "w", "o"], WindowOnly, "only window"));
    bindings.extend(bind(&normal_modes, &["space", "p"], NoOp, "project"));
    bindings.extend(bind(&normal_modes, &["space", "p", "p"], CommandPalette, "command palette"));
    bindings.extend(bind(&normal_modes, &["space", "p", "f"], FilePicker, "project files"));

    bindings
}
