# Backend and Frontend Ownership Rules

## Purpose

Keep `xi-core-lib` frontend agnostic. Keep `ee-tui` focused on terminal interaction and rendering.

## Core Rule

Backend owns editor truth. Frontend owns user interaction and presentation.

Backend APIs should expose semantic editor operations and state updates. Frontends should translate local input into those operations and render backend state in their own UI model.

## Backend Ownership

Place behavior in backend when it must behave identically across TUI, GUI, web, or future frontends.

- Document model: buffers, views, rope text, revisions, undo and redo.
- Text semantics: insert, delete, move, select, find, replace, transpose, duplicate line, number increment and decrement.
- Selection truth: cursor positions, selection regions, movement results, gesture selection results, and backend-requested scroll targets.
- File behavior: open, save, reload, line endings, dirty or pristine state.
- Plugin and LSP integration: diagnostics, hover, completion, formatting, code actions, definitions, references, rename.
- Rendering data: line cache updates, syntax scopes, annotations, diagnostic ranges, style ids.
- Protocol: stable RPC notifications, requests, params, responses, and errors.
- Cross-process behavior: plugin lifecycle, request cancellation, timeouts, revision tracking.

Backend code must not depend on terminal, GUI, ratatui, keybindings, Vim modes, prompt buffers, or frontend layout details.

## Frontend Ownership

Place behavior in frontend when it depends on input device, UI toolkit, terminal capability, or local workflow.

- Input mapping: keys, raw mouse events, counts, operators, text objects, command-line parsing.
- Mode state: normal, insert, visual, operator-pending, search, prompt, confirmation flows.
- UI state: tabs, windows, splits, focused pane, pickers, quickfix panels, location lists.
- Rendering: terminal layout, gutters, statusline, prompt line, popups, overlays, colors, visible whitespace.
- Viewport origin: terminal dimensions, resize handling, scroll offsets, cursor placement on screen.
- Clipboard and registers: unnamed, named, numbered, system clipboard, bracketed paste, OSC 52.
- Local workflow glue: messages, prompts, picker filtering, quickfix navigation, command history.
- Editor config loading: filetype-specific config discovery, parser choice, config-file precedence, and translation from frontend config sources into backend config tables.
- Display cache: frontend may cache backend line updates, but backend remains source of truth.

Frontend code must not reimplement document mutation semantics, undo history, plugin revision logic, LSP protocol handling, or file consistency rules.

Editor config is frontend-owned. Backend is editor-config agnostic and must not load editor config files, choose parser by filetype, or depend on `.editorconfig`, `.ee.toml`, or any other frontend config source. Frontend must resolve whatever config source applies for file type and parser in use, then send resulting semantic config values to backend.

## Boundary Rules

- Frontend sends intent; backend applies editor semantics.
- Backend sends state; frontend chooses display.
- Frontend resolves editor config source and parser; backend only receives resolved config values.
- Raw mouse or touch input is frontend-owned; canonical gesture semantics are backend-owned.
- RPC methods must stay frontend agnostic. Names should describe editor operations, not UI gestures unless operation is inherently a gesture.
- Backend notifications should contain data, not rendering instructions.
- Frontend-specific behavior may wrap backend commands, but must not change backend invariants.
- New behavior touching document state needs backend tests.
- New behavior touching terminal interaction needs frontend tests.

## Current Protocol Decisions

- Paste source remains frontend-owned because `ee-tui` owns registers, system clipboard, bracketed paste, and OSC 52 integration. Paste edit semantics should stay backend-owned.
- `resize` remains frontend-originated because terminal layout and viewport size originate in `ee-tui`, but backend still owns resulting wrap and view-state updates.
- Mouse clicks and drags originate in frontend, which translates terminal coordinates into canonical backend `gesture` edits.
- `add_selection_above`, `add_selection_below`, `insert_tab`, `transpose`, `selection_for_find`, `selection_for_replace`, `selection_into_lines`, `duplicate_line`, `increase_number`, `decrease_number`, and `multi_find` are valid frontend-facing backend edits when they map cleanly to editor semantics.
- `request_hover` is valid backend-facing protocol, but it is request or special-event surface, not ordinary edit surface.
- Legacy `click` and `drag` backend edits remain compatibility shims only. New frontend code should use canonical `gesture.select` and `gesture.drag`.
