// Copyright 2026 The ee authors. All rights reserved.

//! External formatter execution for the LSP plugin.
//!
//! A formatter reads the buffer text on stdin and writes the formatted text to
//! stdout. The runner enforces a wall-clock timeout, an output byte cap, and
//! UTF-8 output; any breach fails closed so the buffer is never corrupted.

use std::fmt;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use crate::types::FormatterConfig;
use lsp_types::{Position, Range, TextEdit};

/// Maximum bytes preserved from stderr, used for diagnostics.
const MAX_STDERR_BYTES: usize = 4 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ExternalFormatError {
    /// Binary not found on PATH (or spawn failed).
    Spawn(String),
    /// Command exited non-zero.
    NonZeroExit { code: Option<i32>, stderr: String },
    /// Wall-clock timeout exceeded.
    TimedOut(u64),
    /// stdout exceeded the configured cap.
    OutputTooLarge { limit: usize },
    /// stdout was not valid UTF-8.
    NotUtf8,
}

impl fmt::Display for ExternalFormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(message) => write!(f, "failed to start formatter: {message}"),
            Self::NonZeroExit { code, stderr } => {
                let code = code.map(|code| code.to_string()).unwrap_or_else(|| String::from("?"));
                let stderr =
                    if stderr.is_empty() { String::new() } else { format!("; stderr: {stderr}") };
                write!(f, "formatter exited with code {code}{stderr}")
            }
            Self::TimedOut(ms) => write!(f, "formatter timed out after {ms} ms"),
            Self::OutputTooLarge { limit } => {
                write!(f, "formatter output exceeded {limit} bytes")
            }
            Self::NotUtf8 => write!(f, "formatter output is not valid UTF-8"),
        }
    }
}

/// Run an external formatter over `input`. Returns the formatted text.
///
/// The command runs on a worker thread; the caller waits at most
/// `formatter.timeout_ms`. On timeout the worker is abandoned (the process is
/// not killed) and an error is returned.
pub(crate) fn run_external_formatter(
    formatter: &FormatterConfig,
    input: &str,
    cwd: Option<&Path>,
) -> Result<String, ExternalFormatError> {
    let (tx, rx) = mpsc::channel::<Result<String, ExternalFormatError>>();
    let command = formatter.command.clone();
    let args = formatter.args.clone();
    let cwd = cwd.map(Path::to_path_buf);
    let timeout = Duration::from_millis(formatter.timeout_ms);
    let cap = formatter.max_output_bytes;
    let input = input.to_string();

    std::thread::spawn(move || {
        let result = run_formatter_process(&command, &args, &input, cwd.as_deref(), cap);
        let _ = tx.send(result);
    });

    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            Err(ExternalFormatError::TimedOut(formatter.timeout_ms))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(ExternalFormatError::Spawn(String::from("worker thread disconnected")))
        }
    }
}

fn run_formatter_process(
    command: &str,
    args: &[String],
    input: &str,
    cwd: Option<&Path>,
    cap: usize,
) -> Result<String, ExternalFormatError> {
    let mut child = Command::new(command)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(cwd.unwrap_or_else(|| Path::new(".")))
        .spawn()
        .map_err(|err| ExternalFormatError::Spawn(format!("{command}: {err}")))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| ExternalFormatError::Spawn(String::from("formatter stdin was not piped")))?;
    let _ = stdin.write_all(input.as_bytes());
    drop(stdin);

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Some(mut pipe) = child.stdout.take() {
        let mut limited = pipe.by_ref().take(u64::try_from(cap).unwrap_or(u64::MAX) + 1);
        limited.read_to_end(&mut stdout).map_err(|err| {
            ExternalFormatError::Spawn(format!("failed reading formatter stdout: {err}"))
        })?;
    }
    if let Some(mut pipe) = child.stderr.take() {
        let mut limited =
            pipe.by_ref().take(u64::try_from(MAX_STDERR_BYTES).unwrap_or(u64::MAX) + 1);
        let _ = limited.read_to_end(&mut stderr);
    }

    let status = child.wait().map_err(|err| {
        ExternalFormatError::Spawn(format!("failed waiting for formatter: {err}"))
    })?;

    if stdout.len() > cap {
        return Err(ExternalFormatError::OutputTooLarge { limit: cap });
    }
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr).trim_end().to_string();
        return Err(ExternalFormatError::NonZeroExit { code: status.code(), stderr });
    }

    String::from_utf8(stdout).map_err(|_| ExternalFormatError::NotUtf8)
}

/// Build a single whole-document replacement edit, or `None` when the
/// formatted text equals the input (no-op format).
pub(crate) fn full_document_edit(text: &str, formatted: &str) -> Option<TextEdit> {
    if text == formatted {
        return None;
    }
    let newline_count = text.bytes().filter(|byte| *byte == b'\n').count();
    let last_line = text.rsplit('\n').next().unwrap_or_default();
    let end = Position {
        line: u32::try_from(newline_count).expect("line count should fit in u32"),
        character: u32::try_from(last_line.encode_utf16().count())
            .expect("line length should fit in u32"),
    };
    Some(TextEdit {
        range: Range { start: Position { line: 0, character: 0 }, end },
        new_text: formatted.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use tempfile::TempDir;

    fn fixture_script(mode: &str) -> &'static str {
        match mode {
            // Tiny shfmt stand-in: `tr x y` normalizes the buffer.
            "transform" => "#!/usr/bin/env sh\ntr 'x' 'y'\n",
            "fail" => "#!/usr/bin/env sh\necho boom >&2\nexit 1\n",
            "sleep" => "#!/usr/bin/env sh\nsleep 30\n",
            "big" => {
                "#!/usr/bin/env sh\ni=0\nwhile [ \"$i\" -lt 10000 ]; do echo xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; i=$((i+1)); done\n"
            }
            "binary" => "#!/usr/bin/env sh\nprintf '\\377\\376\\375'\n",
            _ => unreachable!("unknown fixture mode {mode}"),
        }
    }

    fn fixture_formatter(mode: &str, dir: &TempDir) -> FormatterConfig {
        let script = dir.path().join(format!("fake-formatter-{mode}.sh"));
        fs::write(&script, fixture_script(mode)).unwrap();
        // Run through `sh script` instead of exec'ing the file: exec of a
        // freshly written script races with the kernel (ETXTBSY) under
        // parallel test threads.
        FormatterConfig::external(String::from("sh"), vec![script.to_string_lossy().to_string()])
    }

    #[test]
    fn transforms_input_on_stdin_and_reads_stdout() {
        let dir = TempDir::new().unwrap();
        let config = fixture_formatter("transform", &dir);
        let output = run_external_formatter(&config, "xax\nbx\n", None).unwrap();
        assert_eq!(output, "yay\nby\n");
    }

    #[test]
    fn full_document_edit_is_none_when_unchanged() {
        assert_eq!(full_document_edit("same", "same"), None);
    }

    #[test]
    fn full_document_edit_replaces_whole_document() {
        let edit = full_document_edit("a\nbb\n", "A\nB\n").unwrap();
        assert_eq!(edit.range.start, Position { line: 0, character: 0 });
        assert_eq!(edit.range.end, Position { line: 2, character: 0 });
        assert_eq!(edit.new_text, "A\nB\n");

        let no_trailing = full_document_edit("a\nbb", "A\nB").unwrap();
        assert_eq!(no_trailing.range.end, Position { line: 1, character: 2 });
    }

    #[test]
    fn nonzero_exit_reports_code_and_stderr() {
        let dir = TempDir::new().unwrap();
        let config = fixture_formatter("fail", &dir);
        let err = run_external_formatter(&config, "x", None).unwrap_err();
        assert!(matches!(err, ExternalFormatError::NonZeroExit { code: Some(1), .. }));
        assert!(err.to_string().contains("stderr: boom"));
    }

    #[test]
    fn missing_command_fails_closed() {
        let config =
            FormatterConfig::external(String::from("definitely-not-a-formatter-xyz"), Vec::new());
        let err = run_external_formatter(&config, "x", None).unwrap_err();
        assert!(matches!(err, ExternalFormatError::Spawn(_)));
    }

    #[test]
    fn timeout_abandons_slow_formatter() {
        let dir = TempDir::new().unwrap();
        let mut config = fixture_formatter("sleep", &dir);
        config.timeout_ms = 150;
        let err = run_external_formatter(&config, "x", None).unwrap_err();
        assert_eq!(err, ExternalFormatError::TimedOut(150));
    }

    #[test]
    fn oversized_output_is_rejected() {
        let dir = TempDir::new().unwrap();
        let mut config = fixture_formatter("big", &dir);
        config.max_output_bytes = 1024;
        let err = run_external_formatter(&config, "x", None).unwrap_err();
        assert!(matches!(err, ExternalFormatError::OutputTooLarge { .. }));
    }

    #[test]
    fn non_utf8_output_is_rejected() {
        let dir = TempDir::new().unwrap();
        let config = fixture_formatter("binary", &dir);
        let err = run_external_formatter(&config, "x", None).unwrap_err();
        assert_eq!(err, ExternalFormatError::NotUtf8);
    }

    #[test]
    fn cwd_is_used_when_provided() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path().join("work");
        fs::create_dir_all(&cwd).unwrap();
        let config = FormatterConfig::external(String::from("pwd"), Vec::new());
        let output = run_external_formatter(&config, "x", Some(&cwd)).unwrap();
        assert_eq!(output.trim_end(), cwd.to_string_lossy());
    }

    #[test]
    fn no_change_input_passes_through_unchanged() {
        let dir = TempDir::new().unwrap();
        let config = fixture_formatter("transform", &dir);
        let output = run_external_formatter(&config, "yay\n", None).unwrap();
        assert_eq!(output, "yay\n");
    }
}
