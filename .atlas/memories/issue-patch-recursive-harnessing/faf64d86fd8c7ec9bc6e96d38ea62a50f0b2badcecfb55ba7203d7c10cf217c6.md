---
id: faf64d86fd8c7ec9bc6e96d38ea62a50f0b2badcecfb55ba7203d7c10cf217c6
title: "Phase H5 complete: full library read parity (I50–I51)"
topic: issue-patch:recursive-harnessing
scope: project
importance: high
tags:
  - issues
  - recursive-harnessing
  - parity
  - phase-h5
  - complete
session_id: null
frontend: null
created_at: 2026-11-24T03:47:32Z
updated_at: 2026-11-24T03:47:32Z
source_id: null
links: []
metadata: {}
op_history: "[{\"op\":\"store\",\"at\":\"2026-11-24T03:47:32Z\",\"fields\":[\"body\"]}]"
---
section: Recursive Harnessing — Parity closure (Part 11.2)
phase: H5 — Full library read parity (I50–I51)
status: complete
files: ee-agent-harnessing/src/{library_context.rs (new), context.rs, driver.rs, driver/stages.rs, driver/stream.rs, task.rs, runstate.rs, lib.rs, leave_one_out.rs}; ee-agent-orchestrator/src/user_experience.rs (harness_dir, discover_at, save_entry_at)
wiring: run_task -> library_context::load -> StageEvidence.library -> context::build writes context/<stage>/library/INDEX.md + <domain>/<seq>-<slug>/{EXPERIENCE.md,harness/*}; reader resolves entry.path.parent()/HARNESS_DIR with symlink/escape checks; caps 16 entries/1 MiB/64 KiB, redaction+grader per file.
tests: cargo test --quiet -p ee-agent-harnessing (43 lib + 15 driver); clippy --no-deps clean; fmt clean.
decisions: library root is the experience directory itself (`<workspace>/.agents/experience`); LibraryLocation added for H6; cues.md removed in favor of stage-local INDEX.md; entry.task now stores the task id for retention.
next: H6 — I52/I53 approval and persistence parity.