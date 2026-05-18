# Changelog

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
