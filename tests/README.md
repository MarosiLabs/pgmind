# tests/ — cross-artifact integration tests

Extension-internal tests live in `extension/` (`#[pg_test]`, run via `cargo pgrx test`). This directory holds integration tests that span artifacts — extension + CLI + MCP server — starting in Phase 4 (sync torture suite) and Phase 5 (Walkthrough B scripted end-to-end).
