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
- Conventional commits (`feat:`, `fix:`, `docs:` …).
