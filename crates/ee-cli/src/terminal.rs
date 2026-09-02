use std::env;
use std::ffi::OsStr;
use std::io;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalCommand {
    pub(crate) title: String,
    pub(crate) command: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalRunResult {
    pub(crate) command: String,
    pub(crate) cwd: String,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) success: bool,
    pub(crate) exit_code: Option<i32>,
    pub(crate) duration: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ElevationTool {
    Sudo,
    Run0,
    Su,
}

impl ElevationTool {
    pub(crate) fn binary_name(self) -> &'static str {
        match self {
            Self::Sudo => "sudo",
            Self::Run0 => "run0",
            Self::Su => "su",
        }
    }
}

pub(crate) fn detect_elevation_tool() -> Option<ElevationTool> {
    detect_elevation_tool_in_path(env::var_os("PATH").as_deref())
}

fn detect_elevation_tool_in_path(path_env: Option<&OsStr>) -> Option<ElevationTool> {
    [ElevationTool::Sudo, ElevationTool::Run0, ElevationTool::Su]
        .into_iter()
        .find(|tool| command_exists_in_path(tool.binary_name(), path_env))
}

fn command_exists_in_path(command: &str, path_env: Option<&OsStr>) -> bool {
    let Some(path_env) = path_env else {
        return false;
    };

    env::split_paths(path_env).any(|dir| dir.join(command).is_file())
}

pub(crate) fn build_elevated_copy_command(
    tool: ElevationTool,
    draft_path: &Path,
    target_path: &Path,
) -> String {
    let copy_command = format!(
        "cp -- {} {}",
        shell_quote(draft_path.to_string_lossy().as_ref()),
        shell_quote(target_path.to_string_lossy().as_ref())
    );
    match tool {
        ElevationTool::Sudo => format!("sudo {copy_command}"),
        ElevationTool::Run0 => format!("run0 {copy_command}"),
        ElevationTool::Su => format!("su root -c {}", shell_quote(&copy_command)),
    }
}

pub(crate) fn shell_quote(raw: &str) -> String {
    if raw.is_empty() {
        return String::from("''");
    }
    let mut quoted = String::with_capacity(raw.len() + 2);
    quoted.push('\'');
    for ch in raw.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

pub(crate) fn parse_command(command: &str) -> Result<Option<TerminalCommand>, &'static str> {
    if let Some(raw) = command.strip_prefix('!') {
        let raw = raw.trim_start();
        return if raw.is_empty() {
            Err("shell: usage: :!command")
        } else {
            Ok(Some(TerminalCommand { title: format!("term: {raw}"), command: raw.to_owned() }))
        };
    }

    if let Some(raw) = strip_keyword(command, "term")
        .or_else(|| strip_keyword(command, "terminal"))
        .or_else(|| strip_keyword(command, "sh"))
        .or_else(|| strip_keyword(command, "run_shell_command"))
    {
        return if raw.is_empty() {
            Err("term: usage: :term shell-command")
        } else {
            Ok(Some(TerminalCommand { title: format!("term: {raw}"), command: raw.to_owned() }))
        };
    }

    if let Some(raw) = strip_keyword(command, "make").or_else(|| strip_keyword(command, "build")) {
        let command = join_command("cargo build", raw);
        return Ok(Some(TerminalCommand { title: format!("build: {command}"), command }));
    }

    if let Some(raw) = strip_keyword(command, "test") {
        let command = join_command("cargo test", raw);
        return Ok(Some(TerminalCommand { title: format!("test: {command}"), command }));
    }

    if let Some(raw) = strip_keyword(command, "run") {
        let command = join_command("cargo run", raw);
        return Ok(Some(TerminalCommand { title: format!("run: {command}"), command }));
    }

    Ok(None)
}

pub(crate) fn run_command(command: &TerminalCommand, cwd: &Path) -> io::Result<TerminalRunResult> {
    run_command_with_input(&command.command, cwd, None)
}

pub(crate) fn run_command_with_input(
    command: &str,
    cwd: &Path,
    input: Option<&str>,
) -> io::Result<TerminalRunResult> {
    let started = Instant::now();
    let mut child = shell_command(command, cwd);
    child
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = child.spawn()?;
    if let Some(input) = input
        && let Some(stdin) = child.stdin.take()
    {
        write_command_input(stdin, input)?;
    }
    let output = child.wait_with_output()?;

    Ok(TerminalRunResult {
        command: command.to_owned(),
        cwd: cwd.display().to_string(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        success: output.status.success(),
        exit_code: output.status.code(),
        duration: started.elapsed(),
    })
}

fn write_command_input(mut stdin: impl Write, input: &str) -> io::Result<()> {
    match stdin.write_all(input.as_bytes()) {
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        result => result,
    }
}

pub(crate) fn render_transcript(result: &TerminalRunResult) -> String {
    let status = match (result.success, result.exit_code) {
        (true, Some(code)) => format!("ok (exit {code})"),
        (false, Some(code)) => format!("failed (exit {code})"),
        (true, None) => String::from("ok"),
        (false, None) => String::from("failed (signal)"),
    };

    let mut out = String::new();
    out.push_str(&format!("$ {}\n", result.command));
    out.push_str(&format!("cwd: {}\n", result.cwd));
    out.push_str(&format!("status: {}\n", status));
    out.push_str(&format!("duration: {}\n", format_duration(result.duration)));

    if result.stdout.is_empty() && result.stderr.is_empty() {
        out.push_str("\n(no output)\n");
        return out;
    }

    if !result.stdout.is_empty() {
        out.push_str("\n[stdout]\n");
        out.push_str(&result.stdout);
        if !result.stdout.ends_with('\n') {
            out.push('\n');
        }
    }

    if !result.stderr.is_empty() {
        out.push_str("\n[stderr]\n");
        out.push_str(&result.stderr);
        if !result.stderr.ends_with('\n') {
            out.push('\n');
        }
    }

    out
}

fn strip_keyword<'a>(command: &'a str, keyword: &str) -> Option<&'a str> {
    if command == keyword {
        return Some("");
    }

    let rest = command.strip_prefix(keyword)?;
    let first = rest.chars().next()?;
    if first.is_whitespace() { Some(rest.trim_start()) } else { None }
}

fn join_command(base: &str, raw: &str) -> String {
    if raw.is_empty() { base.to_owned() } else { format!("{base} {raw}") }
}

pub(crate) fn shell_command(command: &str, cwd: &Path) -> Command {
    #[cfg(windows)]
    let mut child = {
        let mut command_builder = Command::new("cmd");
        command_builder.arg("/C").arg(command);
        command_builder
    };

    #[cfg(not(windows))]
    let mut child = {
        let shell = shell_program_from_env(env::var("SHELL").ok().as_deref());
        let mut command_builder = Command::new(shell);
        command_builder.arg("-c").arg(command);
        command_builder
    };

    child.current_dir(cwd);
    child
}

#[cfg(not(windows))]
fn shell_program_from_env(shell: Option<&str>) -> String {
    shell
        .filter(|shell| !shell.is_empty())
        .filter(|shell| {
            let path = Path::new(shell);
            !path.is_absolute() || path.is_file()
        })
        .unwrap_or("sh")
        .to_owned()
}

fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    let millis = duration.subsec_millis();
    format!("{secs}.{millis:03}s")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn build_elevated_copy_command_quotes_paths() {
        let draft = PathBuf::from("/tmp/draft file's.txt");
        let target = PathBuf::from("/root/target file.txt");

        let sudo = build_elevated_copy_command(ElevationTool::Sudo, &draft, &target);
        assert_eq!(sudo, "sudo cp -- '/tmp/draft file'\\''s.txt' '/root/target file.txt'");

        let su = build_elevated_copy_command(ElevationTool::Su, &draft, &target);
        assert!(su.starts_with("su root -c 'cp -- "));
        assert!(su.contains("/tmp/draft file"));
        assert!(su.contains("/root/target file.txt"));
    }

    #[test]
    fn detect_elevation_tool_prefers_declared_order() {
        let temp = tempfile::tempdir().unwrap();
        let sudo = temp.path().join("sudo");
        let run0 = temp.path().join("run0");
        let su = temp.path().join("su");
        std::fs::write(&sudo, "#!/bin/sh\n").unwrap();
        std::fs::write(&run0, "#!/bin/sh\n").unwrap();
        std::fs::write(&su, "#!/bin/sh\n").unwrap();

        let path = env::join_paths([temp.path()]).unwrap();
        assert_eq!(
            detect_elevation_tool_in_path(Some(path.as_os_str())),
            Some(ElevationTool::Sudo)
        );
        std::fs::remove_file(&sudo).unwrap();
        assert_eq!(
            detect_elevation_tool_in_path(Some(path.as_os_str())),
            Some(ElevationTool::Run0)
        );
        std::fs::remove_file(&run0).unwrap();
        assert_eq!(detect_elevation_tool_in_path(Some(path.as_os_str())), Some(ElevationTool::Su));
    }

    #[test]
    fn parse_terminal_aliases_and_shell_shorthand() {
        let shell = parse_command("!printf 'ok'").unwrap().unwrap();
        assert_eq!(shell.command, "printf 'ok'");
        assert_eq!(shell.title, "term: printf 'ok'");

        let term = parse_command("term cargo test -p ee-cli").unwrap().unwrap();
        assert_eq!(term.command, "cargo test -p ee-cli");

        let sh = parse_command("sh printf 'ok'").unwrap().unwrap();
        assert_eq!(sh.command, "printf 'ok'");

        let run_shell_snake = parse_command("run_shell_command cargo check").unwrap().unwrap();
        assert_eq!(run_shell_snake.command, "cargo check");

        let build = parse_command("make --workspace").unwrap().unwrap();
        assert_eq!(build.command, "cargo build --workspace");

        let test = parse_command("test keymap").unwrap().unwrap();
        assert_eq!(test.command, "cargo test keymap");
    }

    #[test]
    fn parse_command_rejects_empty_shell_invocations() {
        assert_eq!(parse_command("!").unwrap_err(), "shell: usage: :!command");
        assert_eq!(parse_command("term").unwrap_err(), "term: usage: :term shell-command");
    }

    #[test]
    fn run_command_with_input_writes_to_stdin() {
        let cwd = std::env::current_dir().unwrap();
        let result = run_command_with_input("cat", &cwd, Some("alpha")).unwrap();

        assert!(result.success);
        assert_eq!(result.stdout, "alpha");
    }

    #[test]
    fn command_input_tolerates_child_closing_stdin() {
        struct ClosedStdin;

        impl Write for ClosedStdin {
            fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        write_command_input(ClosedStdin, "unused selection").unwrap();
    }

    #[cfg(not(windows))]
    #[test]
    fn shell_program_uses_sh_when_absolute_shell_is_missing() {
        let shell = shell_program_from_env(Some("/definitely/missing/ee-shell"));

        assert_eq!(shell, "sh");
    }

    #[cfg(not(windows))]
    #[test]
    fn shell_command_uses_non_login_shell_flag() {
        let cwd = std::env::current_dir().unwrap();
        let child = shell_command("printf 'ok'", &cwd);
        let args =
            child.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect::<Vec<_>>();

        assert_eq!(args, vec![String::from("-c"), String::from("printf 'ok'")]);
    }

    #[test]
    fn render_transcript_marks_stdout_and_stderr() {
        let transcript = render_transcript(&TerminalRunResult {
            command: String::from("cargo test"),
            cwd: String::from("/tmp/repo"),
            stdout: String::from("ok\n"),
            stderr: String::from("warn\n"),
            success: false,
            exit_code: Some(101),
            duration: Duration::from_millis(1530),
        });

        assert!(transcript.contains("$ cargo test"));
        assert!(transcript.contains("status: failed (exit 101)"));
        assert!(transcript.contains("duration: 1.530s"));
        assert!(transcript.contains("[stdout]"));
        assert!(transcript.contains("[stderr]"));
    }
}
