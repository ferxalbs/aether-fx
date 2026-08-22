# Performance plan

The benchmark suite includes a conditional AETHER-versus-rg comparison over the docs/ dataset. On this bootstrap machine, rg 15.1.0 is installed; the comparison records dataset, files, bytes, pattern, warm/cold state, iterations, machine, OS, AETHER version, and rg version when run. No performance claim is published until those measurements exist.

Exploratory AETHER 0.1.0 comparison run on 2026-08-21: 6 files and 11,043 bytes, pattern AETHER, 100 Criterion samples after a 3-second warmup, macOS 15.7.7 on an Intel Core i5-10400F. In-process AETHER dispatch measured [633.10 µs, 636.32 µs] and a freshly spawned rg 15.1.0 process measured [51.184 ms, 51.359 ms]. This is not an apples-to-apples throughput claim because the rg measurement includes process startup while AETHER is already running.

The initial targets are engineering targets, not achieved claims:

| Operation | Target |
| --- | ---: |
| Startup to interactive prompt | < 50 ms |
| Idle RSS | < 30 MB |
| Local tool dispatch | < 1 ms |
| Renderer update | < 8 ms |
| Ctrl+C cancellation | < 50 ms |

The first benchmark suite covers process startup, argument parsing, renderer/buffer work, event dispatch, small and large bounded reads, listing, finding, content search, patch validation/application, session replay, and dispatch. It is deterministic and excludes network calls.

The release profile is `opt-level=3`, fat LTO, one codegen unit, aborting panic, stripped symbols, no debug info, and no incremental compilation. Public builds do not use `target-cpu=native`.
