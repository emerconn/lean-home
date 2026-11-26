# Rust LSP Preference

When working in a Rust project (any workspace containing a `Cargo.toml`):

- Activate the current project in the Serena MCP server if it is not already active.
- Always prefer the Serena / Rust LSP (`rust-analyzer`) MCP tools for symbol lookup,
  go-to-definition, finding references, inspecting types/signatures, checking
  diagnostics, and renaming/refactoring symbols instead of grepping or guessing types.
- After editing `.rs` files, verify changes with LSP diagnostics or `cargo check`.
