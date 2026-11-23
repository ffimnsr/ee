//! Declarative middleware hooks from `.agents/middleware.toml`.
//!
//! A repository may describe a bounded hook set that the tool loop interprets
//! at the existing post-tool-result seam. Hooks only append text to a tool
//! result; they never suppress a result, change success, or block a valid
//! completion. The file is active exactly when it is present and valid: a
//! malformed file, an unknown key, or an out-of-range value is ignored loudly.
//!
//! The schema is intentionally strict and small:
//!
//! ```toml
//! warn_no_artifact_after_calls = 5
//!
//! [[hint_on_error_signature]]
//! pattern = "error[E0308]"
//! hint = "Fix the reported type mismatch before rerunning."
//! max_uses = 2
//!
//! verify_after_write = true
//! ```

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::markdown_config;
use crate::tools::{SideEffectClass, ToolResult};

/// Workspace middleware file relative to a workspace root.
pub const WORKSPACE_MIDDLEWARE_FILE: &str = ".agents/middleware.toml";
/// User middleware file relative to the `ee` config root.
pub const USER_MIDDLEWARE_FILE: &str = "middleware.toml";
/// Maximum bytes read from one middleware file.
pub const MAX_MIDDLEWARE_FILE_BYTES: usize = 64 * 1024;
/// Maximum calls accepted by `warn_no_artifact_after_calls`.
pub const MAX_NO_ARTIFACT_THRESHOLD: usize = 64;
/// Maximum `hint_on_error_signature` entries.
pub const MAX_ERROR_HINTS: usize = 8;
/// Maximum characters in one error-signature pattern.
pub const MAX_HINT_PATTERN_CHARS: usize = 128;
/// Maximum characters in one hint text.
pub const MAX_HINT_TEXT_CHARS: usize = 512;
/// Maximum uses per error hint in one turn.
pub const MAX_HINT_USES: usize = 16;
/// Per-turn cap for the post-write verification reminder.
pub const MAX_VERIFY_AFTER_WRITE_USES: usize = 8;

/// One recurring-error hint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorSignatureHint {
    /// Case-insensitive substring matched against a failed tool result.
    pub pattern: String,
    /// Text appended after the matching failure.
    pub hint: String,
    /// Maximum times this hint fires in one turn.
    pub max_uses: usize,
}

/// Validated middleware hook set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MiddlewareSpec {
    /// Warn once after this many tool calls in a turn that wrote no artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warn_no_artifact_after_calls: Option<usize>,
    /// Bounded hints appended after matching tool failures.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hint_on_error_signature: Vec<ErrorSignatureHint>,
    /// Remind the model to verify after each successful write.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub verify_after_write: bool,
}

impl MiddlewareSpec {
    /// Whether the spec configures no hook at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.warn_no_artifact_after_calls.is_none()
            && self.hint_on_error_signature.is_empty()
            && !self.verify_after_write
    }

    /// Validates bounds; any violation makes the whole file invalid.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(threshold) = self.warn_no_artifact_after_calls
            && !(1..=MAX_NO_ARTIFACT_THRESHOLD).contains(&threshold)
        {
            return Err(format!(
                "warn_no_artifact_after_calls must be 1-{MAX_NO_ARTIFACT_THRESHOLD}"
            ));
        }
        if self.hint_on_error_signature.len() > MAX_ERROR_HINTS {
            return Err(format!(
                "hint_on_error_signature accepts at most {MAX_ERROR_HINTS} entries"
            ));
        }
        for (index, hint) in self.hint_on_error_signature.iter().enumerate() {
            if hint.pattern.trim().is_empty() {
                return Err(format!("hint_on_error_signature[{index}].pattern is empty"));
            }
            if hint.pattern.chars().count() > MAX_HINT_PATTERN_CHARS {
                return Err(format!(
                    "hint_on_error_signature[{index}].pattern exceeds {MAX_HINT_PATTERN_CHARS} characters"
                ));
            }
            if hint.hint.trim().is_empty() {
                return Err(format!("hint_on_error_signature[{index}].hint is empty"));
            }
            if hint.hint.chars().count() > MAX_HINT_TEXT_CHARS {
                return Err(format!(
                    "hint_on_error_signature[{index}].hint exceeds {MAX_HINT_TEXT_CHARS} characters"
                ));
            }
            if !(1..=MAX_HINT_USES).contains(&hint.max_uses) {
                return Err(format!(
                    "hint_on_error_signature[{index}].max_uses must be 1-{MAX_HINT_USES}"
                ));
            }
        }
        Ok(())
    }
}

/// Discovers the effective middleware hook set.
///
/// Workspace roots are tried in canonical order and win over the user file. An
/// absent file emits nothing; an invalid file is ignored loudly and discovery
/// continues with the next candidate.
#[must_use]
pub fn discover(
    cwd: &Path,
    workspace_roots: &[PathBuf],
    user_config_root: Option<&Path>,
) -> Option<MiddlewareSpec> {
    for root in crate::prompt::skills::workspace_skill_roots(cwd, workspace_roots) {
        let path = root.join(WORKSPACE_MIDDLEWARE_FILE);
        match load(&path, &root) {
            Ok(Some(spec)) => return Some(spec),
            Ok(None) => {}
            Err(error) => warn_ignore(&path, &error),
        }
    }
    let root = canonical_dir(user_config_root?)?;
    let path = root.join(USER_MIDDLEWARE_FILE);
    match load(&path, &root) {
        Ok(Some(spec)) => Some(spec),
        Ok(None) => None,
        Err(error) => {
            warn_ignore(&path, &error);
            None
        }
    }
}

fn canonical_dir(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok().filter(|path| path.is_dir())
}

fn warn_ignore(path: &Path, error: &str) {
    tracing::warn!(path = %path.display(), %error, "ignoring middleware file");
}

fn load(path: &Path, root: &Path) -> Result<Option<MiddlewareSpec>, String> {
    if !path.is_file() {
        return Ok(None);
    }
    let (_, content) = markdown_config::read_bounded(path, root, MAX_MIDDLEWARE_FILE_BYTES)?;
    let spec: MiddlewareSpec =
        toml::from_str(&content).map_err(|error| format!("invalid TOML: {error}"))?;
    spec.validate()?;
    Ok((!spec.is_empty()).then_some(spec))
}

/// Per-turn middleware counters.
#[derive(Debug, Clone)]
pub struct MiddlewareState {
    spec: MiddlewareSpec,
    tool_calls: usize,
    wrote_artifact: bool,
    warned_no_artifact: bool,
    verify_uses: usize,
    hint_uses: Vec<usize>,
}

impl MiddlewareState {
    /// Starts one turn's counter state.
    #[must_use]
    pub fn new(spec: MiddlewareSpec) -> Self {
        let hint_uses = vec![0; spec.hint_on_error_signature.len()];
        Self {
            spec,
            tool_calls: 0,
            wrote_artifact: false,
            warned_no_artifact: false,
            verify_uses: 0,
            hint_uses,
        }
    }

    /// Observes one local tool call and returns text to append to its result.
    ///
    /// Never mutates the result itself and never reports failure: an empty
    /// return means no hook applies.
    #[must_use]
    pub fn after_tool_call(
        &mut self,
        tool_name: &str,
        class: Option<SideEffectClass>,
        result: &ToolResult,
    ) -> Option<String> {
        self.tool_calls += 1;
        let wrote = result.success && matches!(class, Some(SideEffectClass::Write));
        if wrote {
            self.wrote_artifact = true;
        }

        let mut notes = Vec::new();
        if let Some(threshold) = self.spec.warn_no_artifact_after_calls
            && !self.warned_no_artifact
            && !self.wrote_artifact
            && self.tool_calls >= threshold
        {
            self.warned_no_artifact = true;
            notes.push(format!(
                "Middleware warning: {threshold} tool calls this turn and no file artifact has \
                 been written yet. If this task produces a file, write it before finishing."
            ));
        }

        if !result.success {
            let haystack = result.text_output.to_ascii_lowercase();
            for (index, hint) in self.spec.hint_on_error_signature.iter().enumerate() {
                if self.hint_uses[index] >= hint.max_uses {
                    continue;
                }
                if haystack.contains(&hint.pattern.to_ascii_lowercase()) {
                    self.hint_uses[index] += 1;
                    notes.push(format!("Middleware hint: {}", hint.hint));
                }
            }
        }

        if wrote && self.spec.verify_after_write && self.verify_uses < MAX_VERIFY_AFTER_WRITE_USES {
            self.verify_uses += 1;
            notes.push(format!(
                "Middleware reminder: `{tool_name}` wrote a file. Re-read the written content and \
                 verify it before claiming completion."
            ));
        }

        (!notes.is_empty()).then(|| notes.join("\n\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolErrorKind;

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create dirs");
        std::fs::write(path, content).expect("write file");
    }

    fn spec(
        threshold: Option<usize>,
        hints: Vec<ErrorSignatureHint>,
        verify: bool,
    ) -> MiddlewareSpec {
        MiddlewareSpec {
            warn_no_artifact_after_calls: threshold,
            hint_on_error_signature: hints,
            verify_after_write: verify,
        }
    }

    #[test]
    fn absent_and_empty_files_emit_nothing() {
        let workspace = tempfile::tempdir().expect("workspace");
        assert!(discover(workspace.path(), &[], None).is_none());

        write(&workspace.path().join(WORKSPACE_MIDDLEWARE_FILE), "# no hooks\n");
        assert!(discover(workspace.path(), &[], None).is_none());
    }

    #[test]
    fn malformed_unknown_and_out_of_range_files_are_ignored() {
        let workspace = tempfile::tempdir().expect("workspace");
        let path = workspace.path().join(WORKSPACE_MIDDLEWARE_FILE);

        write(&path, "not toml = [");
        assert!(discover(workspace.path(), &[], None).is_none());

        write(&path, "unknown_hook = true\n");
        assert!(discover(workspace.path(), &[], None).is_none());

        write(&path, "warn_no_artifact_after_calls = 0\n");
        assert!(discover(workspace.path(), &[], None).is_none());

        let too_many_hints = (0..=MAX_ERROR_HINTS)
            .map(|index| {
                format!(
                    "[[hint_on_error_signature]]\npattern = \"e{index}\"\nhint = \"h\"\nmax_uses = 1\n"
                )
            })
            .collect::<String>();
        write(&path, &too_many_hints);
        assert!(discover(workspace.path(), &[], None).is_none());
    }

    #[test]
    fn workspace_file_wins_over_user_file() {
        let workspace = tempfile::tempdir().expect("workspace");
        let user = tempfile::tempdir().expect("user");
        write(
            &workspace.path().join(WORKSPACE_MIDDLEWARE_FILE),
            "warn_no_artifact_after_calls = 2\n",
        );
        write(&user.path().join(USER_MIDDLEWARE_FILE), "warn_no_artifact_after_calls = 9\n");

        let found = discover(workspace.path(), &[], Some(user.path())).expect("spec");
        assert_eq!(found.warn_no_artifact_after_calls, Some(2));
    }

    #[test]
    fn warn_no_artifact_fires_exactly_at_the_threshold() {
        let mut state = MiddlewareState::new(spec(Some(3), Vec::new(), false));
        let read = ToolResult::success("ok");
        assert!(state.after_tool_call("read_file", Some(SideEffectClass::Read), &read).is_none());
        assert!(state.after_tool_call("read_file", Some(SideEffectClass::Read), &read).is_none());
        let warning = state
            .after_tool_call("read_file", Some(SideEffectClass::Read), &read)
            .expect("warning at the threshold");
        assert!(warning.contains("3 tool calls"));
        assert!(
            state.after_tool_call("read_file", Some(SideEffectClass::Read), &read).is_none(),
            "the warning fires once per turn"
        );
    }

    #[test]
    fn a_write_before_the_threshold_suppresses_the_warning() {
        let mut state = MiddlewareState::new(spec(Some(2), Vec::new(), false));
        let read = ToolResult::success("ok");
        let write = ToolResult::success("written");
        assert!(
            state.after_tool_call("write_file", Some(SideEffectClass::Write), &write).is_none()
        );
        for _ in 0..4 {
            assert!(
                state.after_tool_call("read_file", Some(SideEffectClass::Read), &read).is_none()
            );
        }
    }

    #[test]
    fn error_hints_respect_max_uses_and_match_case_insensitively() {
        let mut state = MiddlewareState::new(spec(
            None,
            vec![ErrorSignatureHint {
                pattern: "error[E0308]".to_string(),
                hint: "Fix the type mismatch.".to_string(),
                max_uses: 2,
            }],
            false,
        ));
        let failure = ToolResult::failure(
            ToolErrorKind::Backend,
            "cargo check failed: ERROR[E0308] mismatch",
        );
        let first = state.after_tool_call("cargo_check", Some(SideEffectClass::Execute), &failure);
        assert!(first.expect("first hint").contains("Fix the type mismatch."));
        assert!(
            state
                .after_tool_call("cargo_check", Some(SideEffectClass::Execute), &failure)
                .is_some()
        );
        assert!(
            state
                .after_tool_call("cargo_check", Some(SideEffectClass::Execute), &failure)
                .is_none(),
            "max_uses caps the hint"
        );
        let unrelated = ToolResult::failure(ToolErrorKind::Backend, "some other error");
        assert!(
            state
                .after_tool_call("cargo_check", Some(SideEffectClass::Execute), &unrelated)
                .is_none()
        );
    }

    #[test]
    fn verify_after_write_is_capped_and_skips_failed_writes() {
        let mut state = MiddlewareState::new(spec(None, Vec::new(), true));
        let write = ToolResult::success("written");
        let failed = ToolResult::failure(ToolErrorKind::PermissionDenied, "permission denied");
        assert!(
            state.after_tool_call("write_file", Some(SideEffectClass::Write), &failed).is_none(),
            "a failed write is not an artifact"
        );
        let mut notes = 0;
        for _ in 0..(MAX_VERIFY_AFTER_WRITE_USES + 4) {
            if state.after_tool_call("write_file", Some(SideEffectClass::Write), &write).is_some() {
                notes += 1;
            }
        }
        assert_eq!(notes, MAX_VERIFY_AFTER_WRITE_USES);
    }
}
