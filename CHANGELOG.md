# Changelog

All notable changes to `hedron-core` are recorded here.

## Unreleased

### Security

- Move `rusqlite` from 0.32.1 to 0.42.0, which brings the bundled
  `libsqlite3-sys` from 0.30.1 to 0.40.0. This clears RUSTSEC-2026-0459
  (fixed in `libsqlite3-sys` 0.39.0) for this crate and for downstream
  workspaces that depend on `hedron-core` through git.

### Changed

- MSRV raised from 1.83 to 1.89, the floor for `rusqlite` 0.42. CI and the
  nightly workflow now run on rustc 1.89, and the README badge matches.
- Dependencies are caret ranges instead of exact pins. The pins existed only
  to hold rustc 1.83. `Cargo.lock` still records the tested versions, and
  downstream workspaces can now unify `serde`, `serde_json`, `blake3`,
  `indexmap`, and `jiff` with their own trees.
- No public API changes. `rusqlite` 0.42 needed no source changes here.

### Fixed

- Clear the existing `cargo clippy --all-targets -- -D warnings` findings:
  `is_none_or` in place of `map_or(true, ..)`, a char-array split pattern,
  no trailing `return` in `store_mode`, and `?` in the MCP handler. Two
  `type_complexity` and two `too_many_arguments` sites are allowed in place.
