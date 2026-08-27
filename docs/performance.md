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

Session-safety hardening uses schema version 5 committed `TurnSnapshot` and resumable `Checkpoint`
records, compact structured work state, Unix session modes, continuation invalidation on workspace
drift, typed `InvalidContinuation` recovery, and no-follow session path containment. The earlier
correctness pass took no new Criterion run; session replay now validates schema v5 rather than v2.

Private application-state verification run on 2026-08-22 used the new OS-state layout with
the exact benchmark command, 100 Criterion samples, and the same local host. Measured
medians were 661.76 µs for bounded session replay, 444.97 µs for 10-session discovery,
2.5024 ms for 100-session discovery, and 63.656 µs for combined state-root,
canonical-workspace, workspace-ID, and session-directory resolution. These are local
diagnostics for the new layout; no cross-machine or speculative regression percentage is
published.

The release profile is `opt-level="s"` with `aether-agent` retained at `opt-level=3`, fat LTO, one
codegen unit, aborting panic, stripped symbols, no debug info, and no incremental compilation.
Public builds do not use `target-cpu=native`.

Repository planner verification on 2026-08-24 used the release Criterion benchmark with 10
samples. Pure high-confidence planning measured a 1.627 µs median estimate, below the <20 µs
planning target; bounded execution measured 57.636 µs and includes canonical containment plus the
bounded local read. The
planner retains at most 8 candidates, 4 files/actions, 12 KiB of read bytes, 16 KiB of structured
observation bytes, and 10 ms of execution time. Planning uses only indexed/context evidence, so
the indexed-context path performs zero filesystem I/O.

Deterministic production-loop evaluation on 2026-08-26 used 9 standalone offline Rust fixtures,
real tool execution, and a deterministic fake model trace. It achieved 9/9 success@1 in both
baseline and optimized runs. The optimized run used 49 model requests versus 55 baseline
(-10.91%), 40 requested tool calls versus 46 (-13.04%), and 39 executed tool calls versus 45
(-13.33%). Six exact unchanged observations were reused locally and one mutation recovery call
was still prevented by policy. Model-visible bytes fell from 328,926 to 289,236 (-12.07%), while
context input bytes fell from 40,231 to 32,035 (-20.37%). The optimized model-visible byte
breakdown was 257,201 tool-schema bytes, 11,338 tool-result bytes, 3,650 policy-feedback bytes,
7,559 protocol-envelope bytes, and no separate static provider prefix. It spawned 13 finite
processes and attempted verification 10 times; aggregate agent wall time was 15,792 ms on this
host. CPU time, RSS, and allocation counters are zero when no portable in-process probe is
available; `evals/compare.py` records external wall/RSS measurements for fresh processes. The
suite gates correctness, step count, executed calls, and context bytes; timing is diagnostic.

The full Criterion suite also completed on this host during the same verification pass. Selected
medians were 105.45 µs for tool dispatch, 59.127 µs for blocking dispatch, 71.496 µs for a
small-file read, 1.6755 ms for a large-file read, 783.78 µs for content search, 1.9264 ms for
staged patch application, 75.541 µs for persistent-process buffer reads, 6.0157 µs for process
registry lookup, and 4.1667 ms for session replay. Process startup measured 48.749 ms and the
fresh `rg` comparison measured 50.907 ms; both include process startup and are diagnostic rather
than direct in-process comparisons.

Final exact-tree acceptance measurements on 2026-08-25 used 100 Criterion samples. Selected
medians were 49.144 ms for process startup, 73.074 µs for a small-file read, 1.7302 ms for a
large-file read, 1.5262 ms for directory listing, 825.93 µs for content search, 110.30 µs for
tool dispatch, 59.627 µs for blocking dispatch, 1.7901 ms for atomic writes, 1.9240 ms for staged
patch application, 76.504 µs for persistent-process reads, 5.9977 µs for process lookup, and
4.2554 ms for session replay. Fresh `rg` measured 51.894 ms. Criterion reported only small
single-host run-to-run variations against its saved baseline; the changed prepared-action,
provenance, and inventory code is not exercised by the affected legacy dispatch benchmarks, and
no benchmark command failed.

The previous stripped release `aether` binary was 5,811,656 bytes. The post-checkpoint-work-state
release build on 2026-08-26 is 5,963,440 bytes. Fresh local `/usr/bin/time -l` probes measured
50 ms and 7,356,416 bytes peak RSS for `--version`, 50 ms and 7,356,416 bytes for `--help`, and
150 ms and 7,626,752 bytes for EOF/minimal startup, which exits with the expected no-model
configuration error. The 151,784-byte increase is the bounded long-horizon/checkpoint
implementation; startup and idle RSS remain in the same local range. These are single-host
diagnostics, not cross-platform or cross-machine guarantees.
