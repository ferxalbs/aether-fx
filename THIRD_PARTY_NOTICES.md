# Third-party notices

The direct dependency baseline is:

| Dependency | Version | Declared license |
| --- | ---: | --- |
| blake3 | 1.8.7 | CC0-1.0 OR Apache-2.0 |
| criterion | 0.8.2 | MIT OR Apache-2.0 |
| futures-util | 0.3.34 | MIT OR Apache-2.0 |
| globset | 0.4.20 | MIT OR Unlicense |
| grep-regex | 0.1.14 | MIT OR Unlicense |
| grep-searcher | 0.1.17 | MIT OR Unlicense |
| ignore | 0.4.33 | MIT OR Unlicense |
| lexopt | 0.3.2 | MIT |
| rainy-sdk | 0.6.50 | Apache-2.0 |
| regex-automata | 0.4.18 | MIT OR Apache-2.0 |
| reqwest | 0.13.4 | MIT OR Apache-2.0 |
| rustix | 1.1.4 | Apache-2.0 OR MIT OR Apache-2.0 WITH LLVM-exception |
| semver | 1.0.28 | MIT OR Apache-2.0 |
| serde / serde_json | 1.0.229 / 1.0.151 | MIT OR Apache-2.0 |
| sha2 | 0.10.9 | MIT OR Apache-2.0 |
| thiserror | 2.0.20 | MIT OR Apache-2.0 |
| tokio | 1.53.1 | MIT |
| unicode-segmentation | 1.13.3 | MIT OR Apache-2.0 |
| unicode-width | 0.2.2 | MIT OR Apache-2.0 |
| windows-sys | 0.61.2 | MIT OR Apache-2.0 |

The resolved Rainy transitive graph includes webpki-roots under CDLA-Permissive-2.0; that permissive license is explicitly allowed in deny.toml. cargo-deny also reports explainable duplicate transitive versions, but no banned source/license/advisory.

AETHER Fx is licensed under Apache-2.0. Direct dependency versions and their declared licenses are recorded in `docs/toolchain-baseline.md`; the complete resolved dependency graph is `Cargo.lock`.

The dependency policy is enforced by `deny.toml`. CI runs `cargo deny check` and `cargo audit` when those tools are available in the workflow environment. This file is intentionally not a hand-copied license bundle; release jobs generate an SBOM and must retain the corresponding dependency metadata.
