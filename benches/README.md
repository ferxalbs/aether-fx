# Benchmarks

Criterion benchmarks live with the package that owns the measured code (`crates/aether-tools/benches/tools.rs`). This directory records the repository-level benchmark plan so the virtual workspace can keep a stable `benches/` boundary without adding a benchmark-only crate.

Required baseline areas are process startup, argument parsing, terminal rendering, event dispatch, small/large reads, directory listing, path finding, content search, patch validation/application, session replay, and tool dispatch. Network and live Rainy calls are never benchmark inputs.
