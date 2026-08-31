# Toolchain and dependency baseline

Verified on 2026-08-31 using Rust/Cargo `1.98.0`, the published `rainy-sdk 0.6.50` crate, and the official Rainy SDK documentation. Versions below are exact manifest selections, not floating requirements. Cargo.lock is authoritative for the transitive graph.

| Component | Resolved version | License | Why it exists | Hot path? |
| --- | --- | --- | --- | --- |
| Rust | 1.98.0 | Rust project terms | Pinned compiler and standard library | Yes |
| Cargo | 1.98.0 | Rust project terms | Pinned build/resolution tool | Yes |
| tokio | 1.53.1 | MIT | One process runtime, bounded channels, async process I/O | Yes |
| serde | 1.0.229 | MIT OR Apache-2.0 | Typed tool/session/backend boundary serialization | Yes |
| serde_json | 1.0.151 | MIT OR Apache-2.0 | Tool schemas, session JSONL, Rainy boundary values | Yes |
| thiserror | 2.0.20 | MIT OR Apache-2.0 | Typed library errors | No |
| lexopt | 0.3.2 | MIT | Small OsString-aware CLI parser | Startup |
| rustix | 1.1.4 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | Safe Unix terminal control, exclusive no-replace rename, and session `openat`/`O_NOFOLLOW` | Terminal/filesystem hot path |
| windows-sys | 0.61.2 | MIT OR Apache-2.0 | Narrow Windows console, `ReplaceFileW`, `MoveFileExW`, and session reparse-point flags | Terminal/filesystem hot path |
| unicode-width | 0.2.2 | MIT OR Apache-2.0 | Terminal cell width | Renderer hot path |
| unicode-segmentation | 1.13.3 | MIT OR Apache-2.0 | Grapheme-safe input/rendering boundaries | Renderer/input |
| ignore | 0.4.33 | Unlicense OR MIT | `.gitignore`-aware walking | Tool hot path |
| globset | 0.4.20 | Unlicense OR MIT | Bounded path glob matching | Tool hot path |
| grep-searcher | 0.1.17 | Unlicense OR MIT | Ripgrep search execution and context | Tool hot path |
| grep-regex | 0.1.14 | Unlicense OR MIT | Ripgrep matcher adapter | Tool hot path |
| regex-automata | 0.4.18 | MIT OR Apache-2.0 | Explicit bounded pattern validation/literal matcher support | Search hot path |
| blake3 | 1.8.7 | CC0-1.0 OR Apache-2.0 OR Apache-2.0 WITH LLVM-exception | Preconditions and content identity | Tool hot path |
| futures-util | 0.3.34 | MIT OR Apache-2.0 | Consume the Rainy SDK stream without async-trait | Rainy stream |
| rainy-sdk | 0.6.50 | Apache-2.0 | Official Rainy API boundary; typed Responses stream, catalog, reasoning, and error APIs | Network path |
| criterion | 0.8.2 | Apache-2.0 OR MIT | Reproducible local benchmarks | Benchmark only |

The verified Rainy SDK is exactly `rainy-sdk 0.6.50`, declared with `default-features = false`. Its
minimal default feature set leaves account/session, legacy, rate-limiting, and tracing surfaces
disabled. The adapter consumes the public typed Responses stream and leaves SSE framing, redirect
blocking, authentication, request/response bounds, and SDK-owned safe-operation retries to Rainy.

No new crate is added. `aether-agent` uses the existing `blake3` crate for stale-file refresh and, on Unix, rustix `fs` for no-follow session directory/file opens. Windows session containment uses existing `windows-sys 0.61.2` `Win32_Storage_FileSystem` reparse flags. Unix `aether-tools` enables rustix `fs` for exclusive rename. No Git dependency, SQLite, vector store, or unpublished Rainy SDK version is used.

The interactive shell/model work adds no package to the resolved dependency graph. The Rainy 0.6.50
upgrade removes its former `eventsource-stream`/`nom` parser path and adds only the rand 0.9 family
and its platform support dependencies; the workspace still has no second SSE parser. The existing
workspace `serde` dependency is now declared directly by `aether` for bounded config and machine
output, and by `aether-rainy` for the normalized catalog projection; this is serialization on
control/catalog paths, not a new provider transport or an additional hot-path framework. The
provider stream remains the existing `rainy-sdk`/`futures-util` boundary, and all UI filtering is
local over the already-loaded bounded catalog.

## Tooling baseline

The CI/release plan also verifies `cargo-deny 0.20.2`, `cargo-audit 0.22.2`, `cargo-llvm-cov 0.9.0`, `cargo-nextest 0.9.143`, `cargo-bloat 0.12.1` (optional report), and `cargo-cyclonedx 0.5.9`.

## Rust 1.98 API review

The bootstrap keeps ordinary formatting and allocation behavior readable. `core::fmt::NumBuffer`, `integer::format_into`, `str::substr_range`, and `slice::subslice_range` were considered but do not simplify a measured hot path yet. The implementation uses stable `Send`/`Sync` boundaries and direct `std::process::Command` argument vectors; no unstable feature is enabled.
