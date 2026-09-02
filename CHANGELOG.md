# Changelog

## 0.11.6 - 2026-09-03

### Features

- parallel agent sessions (`50484ac`)
- add rubber duck mode improvements (`ea1d6ca`)
- add agent registry for agent config (`45f09b5`)
- add workspace persistent memory (`1f9b896`)
- add macos fix for linker (`a36a7f7`)
- fix lots of test for macos (`24e33b3`)


### Fixes

- macos test errors and config warnings on incomplete servers (`b37d9ba`)
- unit tests and macos CI tests (`6cc34bb`)
- cache tests that has errors (`bcce7b3`)
- macos 20 second deadline on one test (`355c7cc`)
- second update for fixing the macos cache bug (`6c81d4e`)
- third update for macos CI error (`74595ef`)
- fourth update on macos CI errors (`97c1047`)


### Tests

- fix the offending macos temp dir failures (`cd54bbf`)


## 0.11.5 - 2026-08-29

### Features

- speed up the loading of files (`27c38f83`)
- update the restore session and fix tests (`08cda124`)
- add missing gaps on tools (`30495b75`)
- enhanced the deny/confirm on trust manager (`fd0d32ef`)
- update the versions and issues.md (`93fea855`)


### Tests

- flaky test updates (`b65cb947`)


### Maintenance

- install script avoids text file busy (`c7303e6a`)
- update cargo toml for libgit (`91f3122d`)


## 0.11.4 - 2026-08-29

### Features

- speed up the loading of files (`27c38f83`)
- update the restore session and fix tests (`08cda124`)
- add missing gaps on tools (`30495b75`)
- enhanced the deny/confirm on trust manager (`fd0d32ef`)


### Tests

- flaky test updates (`b65cb947`)


### Maintenance

- install script avoids text file busy (`c7303e6a`)


## 0.11.3 - 2026-08-28

### Features

- update criterion to 0.8.2 and some deps to vendored ssl (`58d107be`)
- add browser run and more option for search internet context (`fb0ee3b0`)
- add fixes to gap on acp agentic loop (`ae5416ee`)


### Fixes

- lint problem on the web browser agent (`e232ae5a`)
- resume turn terminal bug (`48c06cd5`)


### Maintenance

- update toml from 0.8 to 1.1.4 (`6cb61603`)


## 0.11.2 - 2026-08-24

### Fixes

- ci failures caused by git fixture and scala scm (`88d13a39`)
- update agent flaky test (`aba9327c`)


## 0.11.1 - 2026-08-24

### Features

- update source for doc test (`c845bfb8`)


### Fixes

- remaining docs test on rust source (`fca45138`)


## 0.11.0 - 2026-08-24

### Features

- refactor app monolithic test to module (`a01bd1cd`)
- modularize runtime_loader (`7220ac51`)
- modularize event context (`3f818a09`)
- remove the restoration of session upon launching editor (`29fb9da3`)
- add logs dir on doctor subcommand (`1f10bab0`)
- fix the terminal runner (`54cdcff1`)
- add fixes to tests and runtime loader (`b8088306`)
- add new query dirs (`96b9b4af`)
- fix queries and do doctor ouput (`a32ba1d9`)
- fix the ci error problems (`39614b13`)
- add full ACP and MCP to ee editor (`aeb164e9`)
- complete the ACP agents mode (`21746010`)
- update gap on agent enter (`37289854`)
- cleanup and follow ACP spec (`73612515`)
- add llm agents (`56b47fe9`)
- add new orchestration framework (`9c052d57`)
- update agent tui and default to orchestrator (`656a5a37`)
- add new issues and agent slash command (`73c23086`)
- add compact and secrets (`d1da779e`)
- update the toolset of agent (`35793c95`)
- add trust settings to agent mode (`54be5a20`)
- stabilize the agents tui (`856830dd`)
- add sse streaming to openrouter (`f249c1ce`)
- update orchestrator and terminal timeout (`1caef1b6`)
- harden mcp and agent services making it resilient (`6aab7661`)
- second pass for resilence of agent (`d1c71828`)
- add new mcp tools (`d071cd07`)
- add config init subcommand (`293a9027`)
- update install script to install openrouter agent (`a9ed44b2`)
- add profile trust (`d93ec8ef`)
- update orchestrator tool policy (`98d86608`)
- add proper color on footer (`1abb279d`)
- update tools manifest and replayable llm harness (`cbad0572`)
- add evidence gated completion for tasks (`e8be3cbc`)
- add task aware context planning (`15fb93ae`)
- add auditable write transactions (`8622e96f`)
- add command intelligence and targeted tests (`1f3df6c2`)
- improve subagent delegation (`acf5527e`)
- privacy observability redaction (`99148389`)
- add proper set modes (`09c2c380`)
- add missing commands on agents tui (`cc62df94`)
- add approval slash command (`befd9703`)
- add new context slash command (`eb8b4f24`)
- add missing slash commands (`3eff340d`)
- update clipboard lint warning (`df96034f`)
- update provider ask to read and deps (`5b3d6e0e`)
- add auto-compact when agent is working (`6974c75c`)
- update autocompact to run on agent loop only (`aeb048e0`)
- update malformed tools retry (`a034ecde`)
- update state and add toast (`85fcd254`)
- add new setup agent and fixes on the toast (`5b37dc1d`)


### Fixes

- modify xi-lsp-plugin and fix errors (`f70fc6f4`)
- update the bug on status and git commands that causes git lock (`3e1ed342`)
- handle macos and lint errors (`440861a5`)
- lint errors and macos build (`1c2dffe7`)


### Documentation

- update issues for new todos (`1a8f3365`)


### Tests

- update failing test and modify deps (`49c011aa`)


### Maintenance

- add task runner update (`0177db8c`)


## 0.10.1 - 2026-07-08

### Features

- update fuzz runner version (`66b7cb71`)
- add config set, get and show (`5db963dd`)
- update fuzz release bump version (`cfde588f`)
- update release.sh and slight rebump of version (`b39c6ee8`)


## 0.10.0 - 2026-07-08

### Features

- update fuzz runner version (`66b7cb71`)
- add config set, get and show (`5db963dd`)
- update fuzz release bump version (`cfde588f`)


## 0.9.0 - 2026-07-08

### Features

- add auto indent and smart indent fallback (`53097f68`)
- fix the error cause by parallel runs race cond (`0d824935`)
- add privilege call for save error (`359b44cf`)


### Tests

- update test and fuzz failures (`cccc8f7c`)


## 0.8.10 - 2026-06-27

### Features

- add plugins command (`73364d8`)
- add logs and edit_config command (`8ce7a9f`)
- add keymap binding for tab (`d6a5c83`)


### Fixes

- flaky tests and language subcommand (`0103ad7`)


### Tests

- update branch related test errors (`b03ab30`)


## 0.8.9 - 2026-06-27

### Features

- add plugins command (`73364d8`)
- add logs and edit_config command (`8ce7a9f`)
- add keymap binding for tab (`d6a5c83`)


### Fixes

- flaky tests and language subcommand (`0103ad7`)


## 0.8.8 - 2026-05-20

### Features

- add git hash and build profile on version (`2491f180`)
- move runtime-fetch under runtime scope (`f9e9c156`)
- add reflow command and move some commands to backend to thin frontend (`a98359bb`)
- add wiki submodule and add keybindings to PgUp and PgDown (`6c4e7e73`)
- add plugin loading with lsp-plugin but with hiccup on initial loading times (`53641391`)
- add more wiki docs and create schema to check drift (`f62c73c4`)
- update linux plugin loader due to seccomp (`9ea9a1bd`)
- add lsp and grammars section on config (`f8bdf23f`)


## 0.8.7 - 2026-05-19

### Features

- update prefix keymaps with hints (`9c21c4d3`)
- centralized the UI and syntax color to theme file (`d68a9cda`)
- centralize command metadata for command palette upgrade (`dc4cc108`)


### Fixes

- sanitize and normalize line endings on paste (`9fb76665`)


### Tests

- update the flaky test on ci linux (`d30e86e1`)


## 0.8.6 - 2026-05-19

### CI

- update macos bash bug as its old and can't process bash v4 args (`e9eba0c6`)


### Maintenance

- remove fuzz from quality gates (`bdf4aa25`)


## 0.8.5 - 2026-05-19

### Features

- update the runtime loading due to linux-musl bug (`287a402b`)


## 0.8.4 - 2026-05-18

### Features

- rename crates to be able to publish to crates.io (`5c88a9a7`)
- add head and tail commands under `do file` (`d68b2087`)
- update the picker UI (`ec14b8bc`)
- update tree-sitter runtime grammar loading (`ff3354d2`)
- update working area padding to gutter (`8a5b0513`)
- bump version and release script (`68527a22`)
- update docs and cargo lock (`4da124a0`)


### Fixes

- update crlf and utf-8 bom reads on vlf mode (`e337db09`)
- update the text loading problems on large file buffers (`3becc307`)


### Documentation

- update README and add quick start (`7286f695`)
- add metadata docs to cargo for publish (`9cd1bc6e`)


### Maintenance

- update cargo lock file (`dee7d370`)


## 0.8.3 - 2026-05-18

### Features

- rename crates to be able to publish to crates.io (`5c88a9a7`)
- add head and tail commands under `do file` (`d68b2087`)
- update the picker UI (`ec14b8bc`)
- update tree-sitter runtime grammar loading (`ff3354d2`)
- update working area padding to gutter (`8a5b0513`)


### Fixes

- update crlf and utf-8 bom reads on vlf mode (`e337db09`)
- update the text loading problems on large file buffers (`3becc307`)


### Documentation

- update README and add quick start (`7286f695`)
- add metadata docs to cargo for publish (`9cd1bc6e`)


### Maintenance

- update cargo lock file (`dee7d370`)


## 0.8.2 - 2026-05-15

### Features

- fix code docs (`562381da`)


## 0.8.1 - 2026-05-15

### CI

- fix ci and release workflow plus bugs (`1862d141`)


## 0.8.0 - 2026-05-15

### Features

- add xi editor workspace (`cdf38231`)
- add ee-tui and format all code (`06260864`)
- add fixes to backend and xi-core (`ebf27aad`)
- remove xi-trace and xi-lang (`1853022e`)
- P1 RPC/LSP modernization (`d07c5d05`)
- implement L105–L110 (viewport, table-driven input, async RPC, unicode-width) (`05a07c11`)
- multi-buffer manager, splits, and plugin update coalescing (`560a5b0b`)
- add command ranges, history, completion, and pickers (`38d13c9a`)
- add quickfix/location-list views and safe file workflow (`f85d099e`)
- add display ergonomics and fold management (`321426f6`)
- update the xi-* crates to align to rule on boundary (`810fbf86`)
- add symbol outline and workspace symbol picker in ee-tui (`800c10b9`)
- add visual-mode selection highlight in ee-tui renderer (`e1ef8ffd`)
- add CLI argument parsing and subcommands (`b482ebf1`)
- modularize app commands and deduplicate LineBreak table (`e7748f58`)
- update the rope engine (`df9021e1`)
- add backend syntax spans, annotations, and rope CRDT merge fixes (`280e131e`)
- add githooks and format (`860eb912`)
- plugin runtime modernization with wasm support and error handling improvements (`d071b61a`)
- update fuzz targets (`c0f3363f`)
- expand tui workflow support (`d97651a5`)
- add backend-owned command and selection flows (`4c16d15e`)
- update unicode to 15.1 and changes on commands in ee (`aa87ab7b`)
- improve line cache and core file explorer (`336973fd`)
- add normal-mode performance budgets, fixtures, and metrics (`bbf58baa`)
- implement ConstrainedNormal transition mode with feature gates and status (`0886c2cf`)
- wire VLF store into file open path (`c8db06ae`)
- sparse VLF rendering with loading rows (`22cde337`)
- add backend protocol for VLF viewport requests (`dd799d95`)
- update the normal loading path and add performance changes (`c58236c1`)
- update the first paint performance (`f352d210`)
- fix problems in vlf and syntax highlighting (`cd7afda5`)
- update the handling of very long lines (`e8ab09e9`)
- add proptest on xi-rope crdt (`3e6d54d1`)
- add additional proptest for delta application and invariants (`8d494757`)
- complete the initial commands and re-arrange keymaps (`0adc95e0`)
- alot of bug fixes and add sequence keys (`d35a09d9`)
- add tabular like alignment (`b4e93ad9`)
- update the core policy and set proper constraint (`f7c9866c`)
- fix flaky test for swift motion (`f804c095`)
- lighten the load on normal/constrained mode (`ef5f11dc`)
- implement streaming save for VLF documents (`66df7151`)
- async cancellable reindent + ConstrainedNormal backing evaluation (`c5f77085`)
- update the vlf saving path (`f775fe12`)
- update the rope slice struct (`4591ffbc`)
- add write streaming apis (`c442e641`)
- update the clippy errors and make default stable (`e327b7e9`)
- optimize line jump to end (`f2efc60b`)
- migrate from syntect to full tree-sitter (`bb577959`)
- update bench and issues (`e0d9a55f`)
- add the vlf writing path (`076e7b04`)
- update new_view rpc and its perf (`dedb246b`)
- update the saving flow to avoid race (`8e177bd1`)
- update config file hierarchy (`1866f973`)
- update vlf connection to edit (`c026cfdd`)
- update the vlf editing overlay and make vlf faster (`f3ed4c9b`)
- update release script and unignore the changelog (`a7dc4c0c`)


### Fixes

- harden xi-rope tree invariants (`7c4841bd`)
- polish xi-rope audit follow-ups (`44d902f7`)
- lint errors and ci (`f8823a27`)
- use std::sync::mpsc for channels within tokio runtime context (`5a335196`)
- update large files movement (`3068a77a`)
- update test and clippy for test errors (`6bfcb9de`)
- update the bug causing errors on very large files (`1d3c0441`)
- update line_cache to pre-parse to accomodate vlf (`1d51620c`)
- update ci and fix macos bugs (`74bd9e8a`)
- update saving to poll properly to fix a flaky test (`1fe80652`)


### Documentation

- update the ISSUES.md (`0bb6b111`)
- update issues for steps to implement and wire VLF write and full tree-sitter (`9db83b85`)
- update README (`adfe7768`)


### Tests

- add register paste test coverage and fix clippy warnings (`748891d7`)


### CI

- add GitHub Actions CI workflow (`dd42d903`)
- fix fuzz errors (`46d77d3d`)
- add release workflow and install scripts (`ad9b7a2d`)


### Maintenance

- reorganize workspace into crates (`2b2f4695`)
- flatten workspace layout (`660102fd`)
- workspace-wide protocol cleanup (`b4e2c38c`)
- modernize xi-rpc, lsp-lib, and plugin infrastructure (`85434f0f`)
- complete quality improvements and feature implementations (`f94f10df`)
- refactor and remove deprecated interval methods (`c5bf6073`)
- move ee-tui to ee-cli (`c7d6427e`)
