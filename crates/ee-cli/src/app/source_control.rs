//! `impl App` methods: source_control domain.
use super::*;

impl App {
    pub(crate) fn refresh_source_control(&mut self) {
        let vlf_buf_ids = self
            .backend
            .all_bufs()
            .iter()
            .filter(|buf| buf.is_vlf)
            .map(|buf| buf.id)
            .collect::<Vec<_>>();
        for buf_id in vlf_buf_ids {
            self.source_control.remove(&buf_id);
        }

        let now = Instant::now();
        let snapshots = self
            .backend
            .all_bufs()
            .iter()
            .filter(|buf| !buf.is_vlf && buf.is_fully_cached())
            // Skip periodic background refresh for constrained-sized buffers:
            // cloning + diffing 50K+ lines in the UI thread would cause frame drops.
            // Explicit git commands still work because refresh_active_git_status
            // is called directly and is not subject to this throttle.
            .filter(|buf| buf.lines.len() <= CONSTRAINED_GIT_REFRESH_MAX_LINES)
            .filter(|buf| {
                self.source_control.get(&buf.id).is_none_or(|cached| {
                    now.duration_since(cached.last_refresh) >= Duration::from_secs(2)
                })
            })
            // Whole-buffer policy-allowed: source-control diff requires full text mirror;
            // guarded by !buf.is_vlf && buf.is_fully_cached() and line-count bound above.
            .map(|buf| (buf.id, buf.path.clone(), buf.lines.clone()))
            .collect::<Vec<_>>();

        for (buf_id, path, lines) in snapshots {
            let fingerprint = git::buffer_fingerprint(path.as_deref(), &lines);
            let stale = self.source_control.get(&buf_id).is_none_or(|cached| {
                cached.fingerprint != fingerprint
                    || cached.path != path
                    || now.duration_since(cached.last_refresh) >= Duration::from_secs(2)
            });
            if !stale {
                continue;
            }

            let status = path
                .as_deref()
                .and_then(|file_path| git::inspect_buffer(file_path, &lines).ok().flatten());
            self.source_control
                .insert(buf_id, GitBufferCache { fingerprint, path, last_refresh: now, status });
        }
    }
    pub(crate) fn input_idle_for(&self, duration: Duration) -> bool {
        Instant::now().duration_since(self.last_input_at) >= duration
    }
    pub(crate) fn git_status(&self, buf_id: crate::buffer::BufferId) -> Option<&GitBufferStatus> {
        if self.backend.all_bufs().iter().any(|buf| buf.id == buf_id && buf.is_vlf) {
            return None;
        }

        self.source_control.get(&buf_id).and_then(|cached| cached.status.as_ref())
    }
    pub(crate) fn current_git_status(&self) -> Option<&GitBufferStatus> {
        self.git_status(self.backend.active().id)
    }
    pub(super) fn refresh_active_git_status(&mut self) -> Option<GitBufferStatus> {
        if self.block_active_vlf_source_control("git status") {
            return None;
        }

        let buf = self.backend.active();
        if !buf.is_fully_cached() {
            self.backend.status_message =
                Some(String::from("git: status unavailable until buffer lines are loaded"));
            return None;
        }
        let fingerprint = git::buffer_fingerprint(buf.path.as_deref(), &buf.lines);
        let status = buf
            .path
            .as_deref()
            .and_then(|path| git::inspect_buffer(path, &buf.lines).ok().flatten());
        self.source_control.insert(
            buf.id,
            GitBufferCache {
                fingerprint,
                path: buf.path.clone(),
                last_refresh: Instant::now(),
                status: status.clone(),
            },
        );
        status
    }
    pub(super) fn jump_to_git_hunk(&mut self, forward: bool) {
        self.last_repeatable_motion = Some(RepeatableMotion::GitHunk { forward });
        let Some(status) = self.refresh_active_git_status() else {
            if self.backend.status_message.is_none() {
                self.backend.status_message =
                    Some(String::from("git: current buffer not in repository"));
            }
            return;
        };
        if status.hunks.is_empty() {
            self.backend.status_message = Some(String::from("git: no hunks in current buffer"));
            return;
        }
        let target = if forward {
            status.next_hunk_line(self.backend.cursor_line)
        } else {
            status.prev_hunk_line(self.backend.cursor_line)
        };
        if let Some(line) = target {
            self.jump_to_line(line);
            self.backend.status_message = Some(format!(
                "git hunk: line {} ({}/{})",
                line + 1,
                status.branch,
                status.repo_relative
            ));
        }
    }
    pub(super) fn jump_to_git_hunk_edge(&mut self, first: bool) {
        let Some(status) = self.refresh_active_git_status() else {
            if self.backend.status_message.is_none() {
                self.backend.status_message =
                    Some(String::from("git: current buffer not in repository"));
            }
            return;
        };
        if status.hunks.is_empty() {
            self.backend.status_message = Some(String::from("git: no hunks in current buffer"));
            return;
        }
        let target = if first { status.first_hunk_line() } else { status.last_hunk_line() };
        if let Some(line) = target {
            self.jump_to_line(line);
            self.backend.status_message = Some(format!(
                "git hunk: line {} ({}/{})",
                line + 1,
                status.branch,
                status.repo_relative
            ));
        }
    }
    pub(super) fn show_git_blame(&mut self) {
        if self.block_active_vlf_source_control("git blame") {
            return;
        }

        let Some(path) = self.backend.active().path.clone() else {
            self.backend.status_message =
                Some(String::from("git blame unavailable for scratch buffer"));
            return;
        };
        match git::blame_line(&path, self.backend.cursor_line) {
            Ok(Some(blame)) => {
                let content = git::format_blame(&blame, self.backend.cursor_line);
                self.hover_popup = Some(HoverPopup {
                    title: format!("Git Blame {}", self.backend.cursor_line + 1),
                    content: content.clone(),
                });
                self.backend.status_message = Some(content);
            }
            Ok(None) => {
                self.backend.status_message =
                    Some(String::from("git blame unavailable for current line"));
            }
            Err(err) => {
                self.backend.status_message = Some(format!("git blame failed: {err}"));
            }
        }
    }
    /// Opens bounded staged, unstaged, and untracked changes for Agents TUI.
    /// Uses libgit2 only; protected paths never contribute diff content.
    #[cfg(feature = "agents")]
    pub(super) fn open_workspace_git_diff(&mut self) {
        let root = match std::fs::canonicalize(&self.working_dir) {
            Ok(root) => root,
            Err(error) => {
                self.backend.status_message = Some(format!("workspace diff unavailable: {error}"));
                return;
            }
        };
        let repository = match git::GitRepository::discover(&root) {
            Ok(Some(repository)) => repository,
            Ok(None) => {
                self.backend.status_message =
                    Some(String::from("workspace diff unavailable: not a Git repository"));
                return;
            }
            Err(error) => {
                self.backend.status_message = Some(format!("workspace diff unavailable: {error}"));
                return;
            }
        };
        let limits = git::GitReadLimits::default();
        let report = match repository.status(limits) {
            Ok(report) => report,
            Err(error) => {
                self.backend.status_message = Some(format!("workspace diff unavailable: {error}"));
                return;
            }
        };
        let protected = |path: &std::path::PathBuf| {
            crate::policy::is_protected_relative_path(&path.to_string_lossy())
        };
        let staged_protected = report.staged.iter().any(protected);
        let unstaged_protected = report.unstaged.iter().any(protected);
        let secrets = self.agents_secret_values();
        let mut output = format!(
            "# Workspace diff\n\nRepository: {}\nBranch: {}\n\n",
            report.repo_root.display(),
            report.branch.as_deref().unwrap_or("(detached)")
        );
        output.push_str("## Staged\n\n");
        if staged_protected {
            output.push_str("Diff omitted: protected path changed.\n\n");
        } else {
            match repository.staged_diff(limits) {
                Ok(diff) if diff.text.trim().is_empty() => {
                    output.push_str("(no staged changes)\n\n")
                }
                Ok(diff) => {
                    output.push_str(&ee_agent_host::redact::redact_secret_values(
                        &diff.text, &secrets,
                    ));
                    if diff.truncated {
                        output.push_str("\n[staged diff truncated]\n");
                    }
                    output.push('\n');
                }
                Err(error) => output.push_str(&format!("staged diff unavailable: {error}\n\n")),
            }
        }
        output.push_str("## Unstaged\n\n");
        if unstaged_protected {
            output.push_str("Diff omitted: protected path changed.\n\n");
        } else {
            match repository.unstaged_diff(limits) {
                Ok(diff) if diff.text.trim().is_empty() => {
                    output.push_str("(no unstaged changes)\n\n")
                }
                Ok(diff) => {
                    output.push_str(&ee_agent_host::redact::redact_secret_values(
                        &diff.text, &secrets,
                    ));
                    if diff.truncated {
                        output.push_str("\n[unstaged diff truncated]\n");
                    }
                    output.push('\n');
                }
                Err(error) => output.push_str(&format!("unstaged diff unavailable: {error}\n\n")),
            }
        }
        output.push_str("## Untracked\n\n");
        if report.untracked.is_empty() {
            output.push_str("(no untracked files)\n");
        } else {
            for path in &report.untracked {
                if protected(path) {
                    output.push_str("- [protected path omitted]\n");
                } else {
                    output.push_str(&format!("- {}\n", path.display()));
                }
            }
        }
        if report.truncated {
            output.push_str(&format!(
                "\n[status truncated: {} files omitted]\n",
                report.omitted_file_count
            ));
        }
        self.open_generated_buffer("agent workspace diff", &output);
        self.backend.status_message = Some(String::from("workspace diff opened"));
    }
    pub(super) fn open_git_diff_view(&mut self, current_hunk_only: bool) {
        let feature = if current_hunk_only { "git hunk diff" } else { "git diff" };
        if self.block_active_vlf_source_control(feature) {
            return;
        }

        let Some(status) = self.refresh_active_git_status() else {
            if self.backend.status_message.is_none() {
                self.backend.status_message =
                    Some(String::from("git diff unavailable for current buffer"));
            }
            return;
        };
        let selected_hunk =
            if current_hunk_only { status.hunk_at_line(self.backend.cursor_line) } else { None };
        if current_hunk_only && selected_hunk.is_none() {
            self.backend.status_message = Some(String::from("git: cursor not inside changed hunk"));
            return;
        }
        let title = if current_hunk_only { "git hunk diff" } else { "git diff" };
        let rendered = git::render_diff(&status, selected_hunk);
        self.open_generated_buffer(title, &rendered);
    }
    pub(super) fn source_control_disabled_message(feature: &str) -> String {
        format!("{feature} disabled in VLF: {VLF_SOURCE_CONTROL_DISABLED_REASON}")
    }
}
