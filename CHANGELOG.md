# Change Log

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/)
and this project adheres to [Semantic Versioning](https://semver.org/).

## v0.2.1

### Changed

- Use `workspace_folders` instead of deprecated `root_uri` in examples.
- Update `thiserror` to 2.

### Others

- Add backref from `AnyEvent` to `LspService::emit` in docs.

## v0.2.0

### Changed

- Update `lsp-types` to 0.95.0

### Added

- `lsp-types` dependency crate is now re-exported under the crate root.

## v0.1.0

### Changed

- Updated `lsp-types` to 0.94.1 which comes with a few more LSP methods.
  They are added into `trait Language{Server,Client}` respectively
  (under `omni-trait` feature).

- Updated many other dependencies.

### Others

- Some random documentation tweak and fixes.
