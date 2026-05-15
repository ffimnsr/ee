# ee-editor

`ee` is a modern editor built around a fast, backend-agnostic core and a terminal-first frontend.
This repository is a fork and evolution of the original `xi-editor` architecture, with a focus on:

- high-performance editing for large files
- safe and maintainable Rust implementation
- terminal UI experience via `ee-cli`
- language-aware tooling through tree-sitter and LSP support
- plugin-friendly architecture and extensible backend services

This repo is primarily a Rust workspace with a CLI frontend in `crates/ee-cli` and shared editor-core libraries under `crates/xi-core-lib`.

## What makes ee special?

- **Fast and responsive**: backend edits, parsing, and rendering are designed to avoid stalls, even for large buffers.
- **Large-file friendly**: persistent rope data structures and streaming workflows make very large files practical.
- **Terminal-focused UI**: `ee` uses `ratatui` and `crossterm` for a polished terminal-based editing experience.
- **Safe Rust core**: shared libraries are implemented in Rust and organized for frontend-agnostic reuse.
- **Tree-sitter powered**: syntax parsing, highlighting, and language-feature support are built on tree-sitter grammars.
- **Extensible backend**: plugin and RPC-driven design makes it easier to add code actions, completions, diagnostics, and external tooling.

## Repository layout

- `crates/ee-cli`: terminal frontend and user interface for `ee`
- `crates/xi-core-lib`: shared editor core, language support, async runtime glue, and text model APIs
- `crates/xi-core`: original xi backend adapter crate
- `crates/xi-lsp-lib`: LSP integration and language service support
- `crates/xi-plugin-lib`: plugin RPC helpers
- `crates/xi-plugin-derive`: derive macros for plugin-related types
- `crates/xi-rope`: rope text storage implementation
- `crates/xi-rpc`: RPC layer for backend/frontend communication
- `crates/xi-unicode`: unicode support utilities
- `fuzz`: fuzzing targets and artifacts

## Install

### From source

The easiest way to install locally from this repository is:

```sh
cargo install --path crates/ee-cli
```

Once installed, run the editor with:

```sh
ee <path/to/file>
```

### Official installer

This repository includes a Unix installer at `install.sh` that downloads and installs a release binary from GitHub.

#### Install with curl

```sh
curl -fsSL https://raw.githubusercontent.com/ffimnsr/ee/main/install.sh | sh
```

#### Install with wget

```sh
wget -qO- https://raw.githubusercontent.com/ffimnsr/ee/main/install.sh | sh
```

If you prefer to inspect the script first, download it explicitly and run it locally:

```sh
curl -fsSL -o install.sh https://raw.githubusercontent.com/ffimnsr/ee/main/install.sh
sh install.sh
```

The installer supports `bash`, `zsh`, and `fish` completions and installs the binary into `~/.local/bin` by default.

If `~/.local/bin` is not on your `PATH`, add it to your shell profile:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

### Requirements

- Rust `1.95` or newer
- Unix-like shell for `install.sh`
- `cargo` toolchain for local development and builds

## Build and run

### Build the workspace

```sh
cargo build --workspace
```

### Build the release binary

```sh
cargo build --workspace --release
```

### Run `ee` directly from source

```sh
cargo run -p ee-cli -- <path/to/file>
```

### Run the terminal frontend only

```sh
cargo run -p ee-cli -- <path/to/file>
```

## Development

### Formatting

```sh
cargo fmt --all
```

### Linting

```sh
cargo clippy --all -- -D warnings
```

For stable-toolchain checks:

```sh
cargo +stable clippy --workspace --all-targets --all-features -- -D warnings
```

### Tests

```sh
cargo test --workspace
```

For full workspace coverage with stable Rust:

```sh
cargo +stable test --workspace --all-features
```

### Useful tasks

This repository provides `tasks.yaml` for common development flows:

- `format`: format source with `cargo fmt`
- `lint`: run `cargo clippy --all -D warnings`
- `test-stable`: run stable Rust tests
- `install`: install `ee` locally from `crates/ee-cli`

## Design and architecture

### Frontend / backend separation

`ee` keeps the terminal UI separate from the editor core. The frontend handles input, layout, and rendering, while the backend owns buffer state, edit operations, parsing, and language-aware features.

### Backend-agnostic core

`xi-core-lib` is designed to be reusable without tying it to a specific UI. That makes it possible to build multiple frontends on the same editor runtime.

### Language support

The project uses `tree-sitter` for syntax parsing and language features. There is also first-class support for LSP and completion workflows through `xi-lsp-lib`.

### Plugin and RPC model

The editor core communicates through JSON/RPC messages. This keeps external integrations and plugin extensions language-agnostic and easier to evolve.

## Contributing

Contributions are welcome. Open issues and pull requests on GitHub and follow the repository's existing code style.

## Authors

This fork is maintained by the `ee` project contributors. See [AUTHORS](AUTHORS) for history and acknowledgements.

## License

This project is licensed under the Apache 2.0 [license](LICENSE).
