# AGENTS.md

## Purpose
This document defines coding and review expectations for agents working in this repository.

## Agent Response

ALWAYS respond terse like smart caveman. All technical substance stay. Only fluff die.

- Drop articles (a/an/the), filler, pleasantries, hedging.
- Fragments OK. Short synonyms.
- Keep all technical content: code, commands, file paths, exact names, numbers.

Pattern: "[thing] [action] [reason]. [next step]."

Examples:
- Not: "Sure! I'd be happy to help you with that. The issue you're experiencing is likely caused by..."
- Yes: "Bug in auth middleware. Token expiry check use `<` not `<=`. Fix:"

Auto-clarity overrides (temporarily disable caveman style for clarity/safety):
- security warnings
- irreversible action confirmations
- multi-step sequences where fragment order risks misread
- user confusion

## Core Principles
- Prefer clarity, correctness, and maintainability over cleverness.
- Make the smallest safe change that solves the problem, except when existing duplication should be consolidated into a shared file or package/crate.
- Reuse existing functions, modules, and instructions whenever possible before introducing new abstractions.

## Rust Best Practices
- Follow idiomatic Rust and standard formatting (`rustfmt`) for all code.
- Keep modules cohesive and public APIs minimal; prefer `pub(crate)` over `pub` when exposure isn't needed.
- Use `Result` and `?` for error propagation; add context with `anyhow` or typed error enums rather than ignoring errors.
- Prefer composition and traits over deep type hierarchies.
- Define traits at consumer boundaries; avoid premature abstraction.
- Keep functions small and focused on a single responsibility.
- Use `async fn` with explicit cancellation and timeout handling for I/O operations.
- Write unit tests alongside code and integration tests for end-to-end behavior; use `#[cfg(test)]` modules.
- Avoid global mutable state (`static mut`, `lazy_static` with interior mutability); prefer dependency injection via function parameters or struct fields.
- Use the standard library and well-vetted crates (e.g., `tokio`, `axum`, `sqlx`) before adding new dependencies.
- Always run clippy to check if the code has lint problems.
- When writing features or fixes, always write tests for it.
- Use `println!`/`eprintln!` for user-facing command output and confirmations; reserve `info!` and other tracing macros for diagnostics, debugging, and operator-focused logs.

## Security Best Practices
- Validate and sanitize all untrusted inputs.
- Apply least-privilege principles for files, processes, credentials, and network access.
- Never hardcode secrets, tokens, or credentials in source code.
- Use environment variables or a secret manager for sensitive configuration.
- Avoid logging sensitive data (credentials, tokens, PII, secrets).
- Use parameterized queries and safe APIs to prevent injection vulnerabilities.
- Enforce timeouts and cancellation for network and I/O operations.
- Prefer secure defaults and fail closed on authorization/authentication checks.
- Keep dependencies minimal and up to date; patch known vulnerabilities promptly.
- Use cryptographic primitives from trusted standard libraries; avoid custom crypto.

## Reuse-First Development Rule
Before creating new code:
1. Search for an existing function, helper, or package that already solves the need.
2. Reuse and extend existing behavior when practical.
3. Add new functions only when reuse would reduce readability, safety, or correctness.

## Upgrade Policy
- When implementing planned upgrades, prefer removing deprecated code paths and compatibility shims instead of preserving legacy behavior.
- Do not keep backward compatibility unless the task explicitly requires it.
- Favor the cleanest correct implementation over carrying old branches forward.
- If existing code is already present but the design is suboptimal, prefer a focused overhaul that improves correctness, clarity, or maintainability.

## Issue Tracking
- Before starting work, check ISSUES.md for a matching item.
- If the requested work is already complete, mark the corresponding checklist item as done.
- If you finish work that matches an open checklist item, tick it before you wrap up.

## Change Hygiene
- Document non-obvious decisions in concise comments near the code.
- Keep diffs focused; avoid unrelated refactors.
- Add or update tests for new behavior and bug fixes.
- Confirm local build/test success before finalizing changes when possible.

## Commit Message Policy
- Use conventional commit format for subject line (e.g., `feat: ...`, `fix: ...`, `chore: ...`).
- Create a relevant to the change subject line
- In commit body, start each bullet with lowercase (e.g. `add`, `fix`, not `Add`, `Fix`).
- Include brief body summarizing all relevant additions/changes in commit.
- DON'T ever skip git hooks or use `--no-verify`, if there's problem fix it then stage and re-commit.
