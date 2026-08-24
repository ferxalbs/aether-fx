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

The benchmark suite covers process startup, argument parsing, renderer/buffer work, event dispatch, small and large bounded reads, listing, finding, content search, patch validation/application, session replay, blocking-dispatch overhead, cancellation checkpoints, permission decisions, continuation bookkeeping, atomic write, staged patch application, persistent-process buffer reads, and short registry lookups. It is deterministic and excludes network calls; process-buffer benchmarks use a bounded fixture and terminate it after measurement.

Phase 1 design measurements to record locally are:

| Measurement | Result | Status |
| --- | --- | --- |
| Current-thread responsiveness while a blocking tool runs | async timer regression test passes | verified by workspace test |
| Persistent output memory | 16 × 2 × 256 KiB maximum stream storage | design bound |
| Cancellation latency | cooperative checkpoints plus 10 ms polling for async process I/O | diagnostic benchmark pending |
| Release binary size | 4,997,200 bytes (4.77 MiB); 225 locked packages | measured 2026-08-22 |

The blocking pool is deliberately not an unbounded fan-out: each filesystem/search tool call uses one bounded blocking operation, and traversal/result limits remain enforced inside that operation. Process output is drained continuously into recent-output buffers rather than accumulating an unbounded log.

Quick local Criterion sample on 2026-08-21 (`sample-size 10`, 100 ms warmup, 200 ms measurement; directional rather than release-quality publication data):

| Benchmark | Median estimate |
| --- | ---: |
| blocking dispatch overhead | 45.1 µs |
| cooperative cancellation check | 2.45 ns |
| permission decision | 976 ns |
| multi-turn bookkeeping | 425 ns |
| atomic write | 2.24 ms |
| staged patch application | 2.15 ms |
| persistent process buffer read | 46.6 µs |
| process registry lookup/status | 5.48 µs |

These measurements are local diagnostics, not cross-machine guarantees. The existing AETHER-versus-rg comparison remains intentionally separate because a freshly spawned `rg` process includes process startup overhead.

Phase 1.1 verification run on 2026-08-22 used the same Criterion benchmark suite on the same x86_64 macOS host. Selected current medians were: startup 47.851 ms, blocking dispatch 47.367 µs, permission decision 882.94 ns, atomic write 1.7574 ms, staged patch application 1.9243 ms, persistent-process buffer read 50.791 µs, and process registry lookup 5.2652 µs. Criterion reported a statistically significant blocking-dispatch change of +5.97% against its saved baseline and AETHER search-comparison change of +7.47%; process-buffer read reported no statistically significant change. These are observations for regression review, not cross-machine performance claims, and no microbenchmark optimization was added in this correctness phase.

Phase 2 verification run on 2026-08-22 used the same Criterion suite (`sample-size 10`, 100 ms warmup, 200 ms measurement) on the same x86_64 macOS host. Selected current medians were: process startup 47.382 ms, small-file read 69.493 µs, blocking dispatch 60.242 µs, atomic write 1.9126 ms, staged patch application 1.9403 ms, session replay 32.246 µs, and process registry lookup 5.4360 µs. Criterion reported statistically significant regressions versus the saved baseline for small-file read (+28.5%), blocking dispatch (+25.3%), atomic write (+17.2%), session replay (+11.3%), event dispatch (+2.3%), and process registry lookup (+2.4%). The small-file-read increase is expected: `read` now computes a BLAKE3 content hash for stale-file tracking. Session replay now validates schema version 2, secret refusal, and truncated-tail recovery. Short-sample Criterion noise remains; these are observations for regression review, not cross-machine claims.

Session-safety hardening uses schema version 3 committed `TurnSnapshot` records, Unix session modes, continuation invalidation on workspace drift, typed `InvalidContinuation` recovery, and no-follow session path containment. The earlier correctness pass took no new Criterion run; session replay now validates schema v3 rather than v2.

Private application-state verification run on 2026-08-22 used the new OS-state layout with
the exact benchmark command, 100 Criterion samples, and the same local host. Measured
medians were 661.76 µs for bounded session replay, 444.97 µs for 10-session discovery,
2.5024 ms for 100-session discovery, and 63.656 µs for combined state-root,
canonical-workspace, workspace-ID, and session-directory resolution. These are local
diagnostics for the new layout; no cross-machine or speculative regression percentage is
published.

The release profile is `opt-level=3`, fat LTO, one codegen unit, aborting panic, stripped symbols, no debug info, and no incremental compilation. Public builds do not use `target-cpu=native`.

Repository planner verification on 2026-08-24 used the release Criterion benchmark with 10
samples. Pure high-confidence planning measured a 1.627 µs median estimate, below the <20 µs
planning target; bounded execution measured 57.636 µs and includes canonical containment plus the
bounded local read. The
planner retains at most 8 candidates, 4 files/actions, 12 KiB of read bytes, 16 KiB of structured
observation bytes, and 10 ms of execution time. Planning uses only indexed/context evidence, so
the indexed-context path performs zero filesystem I/O.
