# min — project instructions

min is a minimal terminal agentic coding harness in Rust. Everything is a
plugin to a tiny, dependency-free core.

Rules when changing this codebase:

- Keep the core minimal: `min-core` gets no dependencies, no I/O, no async.
  New capabilities go in plugins (new crate under `crates/`, one line in
  `src/main.rs`).
- Plugins never import each other — only `min-core`. Cross-plugin
  communication is events, services, or the tool registry.
- Keep everything synchronous and single-threaded; blocking lives in
  `Plugin::wait`.
- Edit precisely: the `edit` tool replaces exact, unique text. Read first.
- Verify with `cargo build` and `cargo test --workspace` after Rust changes.
- Conventional commits (`feat:`, `fix:`, `docs:` …).
