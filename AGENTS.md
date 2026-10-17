# min — project instructions

min is a minimal terminal agentic coding harness in Rust. Everything is a
plugin to a tiny, dependency-free core.

The architecture has one non-negotiable law, from which all rules below
follow:

> **The core is only a loop.** min's core contains zero functionality and
> zero policy — it loads plugins and pumps events between them, forever.
> Every capability (UI, LLM, tools, the agent loop itself) is a plugin.
> When adding anything, the default is a new plugin, never core code. Only
> change the core when plugins cannot express the feature at all.

Rules when changing this codebase:

- Keep the core minimal: `min-core` gets no dependencies, no I/O, no async,
  and no functionality of its own — just the loop. New capabilities go in
  plugins (new crate under `crates/`, one line in `src/main.rs`).
- Plugins never import each other — only `min-core`. Cross-plugin
  communication is events, services, or the tool registry.
- Keep everything synchronous and single-threaded; blocking lives in
  `Plugin::wait`.
- Edit precisely: the `edit` tool replaces exact, unique text. Read first.
- Verify with `cargo build` and `cargo test --workspace` after Rust changes.
- Conventional commits (`feat:`, `fix:`, `docs:` …).
