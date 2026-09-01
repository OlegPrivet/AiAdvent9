# Repository Guidelines

## Project Structure & Module Organization

This repository contains the `agi` Rust CLI (Rust 2024 edition). `src/main.rs` wires the application together; `src/cli.rs` parses arguments; `src/input.rs` handles terminal text; `src/repl.rs` runs the interactive loop; `src/settings.rs` manages session options; `src/config.rs` loads configuration; and `src/api.rs` implements NeuralDeep requests. Unit tests live beside their modules in `#[cfg(test)]` blocks. API reference material is in `projetcDocs/llm_docs.md` (retain the existing spelling). Cargo metadata is in `Cargo.toml` and `Cargo.lock`.

## Build, Test, and Development Commands

- `cargo build` compiles a debug binary at `target/debug/agi`.
- `NEURALDEEP_API_KEY=... cargo run -- "Explain ownership"` runs the CLI locally. Quote questions containing spaces.
- `cargo test` runs all unit and async integration-style tests.
- `cargo fmt --all -- --check` verifies standard Rust formatting.
- `cargo clippy --all-targets --all-features -- -D warnings` catches common mistakes and treats warnings as failures.
- `cargo build --release` produces an optimized binary under `target/release/`.

## Coding Style & Naming Conventions

Use default `rustfmt` formatting (four-space indentation) and keep modules focused on one responsibility. Follow Rust conventions: `snake_case` for modules, functions, and tests; `UpperCamelCase` for structs and enums; and `SCREAMING_SNAKE_CASE` for constants. Prefer typed errors with `thiserror`, propagate failures with `Result`, and avoid `unwrap`/`expect` outside tests. Keep user-facing CLI and error text consistent with the existing Russian-language messages.

## Testing Guidelines

Add tests in the affected module and name them after observable behavior, such as `rejects_missing_api_key`. Async tests use `#[tokio::test]`. HTTP behavior should be exercised with a local mock server, following `src/api.rs`; tests must not require a real API key or external network access. There is no stated coverage threshold, but cover success paths, malformed responses, and service errors for each change.

## Commit & Pull Request Guidelines

History is small and uses brief, task-focused subjects (for example, `Day 1: простой запрос к апихе`). Keep each commit scoped and write a concise imperative summary. Pull requests should explain the behavior change, list verification commands, link relevant issues, and include terminal output when CLI behavior changes. Before requesting review, run formatting, Clippy, and the full test suite.

## Security & Configuration

Provide credentials only through `NEURALDEEP_API_KEY`. Never commit API keys, shell exports, or captured authorization headers. Keep service defaults centralized in `src/config.rs` and document intentional API contract changes against `projetcDocs/llm_docs.md`.
