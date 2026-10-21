# min — project instructions

min is a minimal terminal agentic coding harness in Rust. Everything is a
plugin to a tiny, dependency-free core.

**Read [docs/architecture.md](docs/architecture.md) first** — it explains the
architecture (core = only a loop, plugins = every capability) with diagrams
and worked examples. The rules below follow from it. To add a capability,
follow **[docs/plugins.md](docs/plugins.md)** (the three plugin shapes, a
complete worked example, the four steps to ship one).

Rules when changing this codebase:

- Keep the core minimal: `min-core` gets no dependencies, no I/O, no async,
  and no functionality of its own — just the loop. New capabilities go in
  plugins ([how](docs/plugins.md)).
- Plugins never import each other — only `min-core`. Cross-plugin
  communication is events, services, or the tool registry.
- Keep everything synchronous and single-threaded; blocking lives in
  `Plugin::wait`.
- Edit precisely: the `edit` tool replaces exact, unique text. Read first.
- Every plugin ships its own architecture doc — `docs/plugins/<name>.md`
  (diagram, events listened to/emitted, worked example) — added to the
  index in `docs/plugins.md`, in the same change as the code. See
  **docs/plugins.md → “Plugin documentation (required)”**.
- Verify with `cargo build` and `cargo test --workspace` after Rust changes.
- Keep the manifests honest: a crate may depend only on `min-core` plus what
  its own code uses. The `unused_crate_dependencies` lint is enabled
  workspace-wide (`[workspace.lints.cargo]` in the root `Cargo.toml`), so
  `cargo +nightly check --workspace --all-targets` reports a dependency
  nothing imports. On stable the lint is a no-op, so run the nightly form
  before adding or removing a dependency. A dependency used only by a test
  belongs in `[dev-dependencies]`.
- A pre-commit hook (`.githooks/pre-commit`) runs `cargo machete --fix`, so
  an unused dependency is stripped and staged with the commit. It is enabled
  by `git config core.hooksPath .githooks` — a local setting, so run that
  once after cloning. The hook needs `cargo-machete`
  (`cargo install cargo-machete --locked`); without it the hook skips with
  a note.
- Conventional commits (`feat:`, `fix:`, `docs:` …).
