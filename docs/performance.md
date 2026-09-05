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


Phase 1 design measurements to record locally are:

| Measurement | Result | Status |
| --- | --- | --- |
| Current-thread responsiveness while a blocking tool runs | async timer regression test passes | verified by workspace test |
| Persistent output memory | 16 × 2 × 256 KiB maximum stream storage | design bound |
| Cancellation latency | cooperative checkpoints plus 10 ms polling for async process I/O | diagnostic benchmark pending |
| Release binary size | 4,997,200 bytes (4.77 MiB); 225 locked packages | measured 2026-08-22 |

The benchmark suite covers process startup, argument parsing, renderer/buffer work, event dispatch,
small and large bounded reads, listing, finding, content search, patch validation/application,
session replay, blocking-dispatch overhead, cancellation checkpoints, permission decisions,
continuation bookkeeping, atomic write, staged patch application, persistent-process buffer reads,
short registry lookups, and bounded shell selector/palette filtering. It is deterministic and
excludes network calls; process-buffer benchmarks use a bounded fixture and terminate it after
measurement.

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

Session-safety hardening uses schema version 6 committed `TurnSnapshot` and resumable `Checkpoint`
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

Evidence-loop hardening verification on 2026-08-28 used the same release deterministic runner after
adding bounded diagnostic/mutation source promotion, live-hash-checked read reuse, scoped
instruction selection, and runtime-owned work outlines. The regression suite remained 9/9 in both
runs. Optimized execution used 49 model requests versus 55 baseline, 40 requested tool calls
versus 46, and 39 executed calls versus 45; six observations were reused and 10 verification
attempts produced one expected failed-then-recovered verification. Context input was 38,860 bytes
versus 48,848 baseline (-20.45%), and total model-visible bytes were 296,061 versus 337,543
(-12.29%). The optimized runner used 13 process spawns, completed in 16,191 ms of agent wall
time, and retained 0 unresolved work items. A direct `/usr/bin/time -l` probe of the release eval
process measured 91,009,024 bytes maximum RSS; the capability process measured 89,989,120 bytes.
These are single-host diagnostics, not cross-machine guarantees.

The release `aether` binary measured 6,012,656 bytes after the hardening work, versus 5,963,440
bytes at the starting tree (+49,216 bytes, +0.83%). A direct `--version` probe measured 7,364,608
bytes maximum RSS. The binary delta is bounded but not a claim that all platforms produce the same
size.

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

Adaptive repository orchestration acceptance run on 2026-08-29 used the release deterministic
evaluator after adding the Turn Director, bounded WorkingSet, evidence reactions, recovery state,
scope-aware verification, revision-aware steering, and fail-closed completion review. All 9/9
regression fixtures passed in both baseline and optimized traces. The optimized trace used
49/55 model requests, 39/40 executed tool calls, one locally reused observation, 12 automatic
evidence reactions, 12 mutation-refresh events, 13 process spawns, and 16,339 ms of agent wall
time. Context input was 39,986/50,138 bytes and total model-visible bytes were 297,187/338,833
bytes (optimized/baseline); the 40,000-byte context gate passed by 14 bytes. The focused
capability suite passed 5/5, including checkpoint resume, two recovery cases, one dirty-user
change case, one stale-observation case, zero corruptions, and zero false completions. It
recorded 8 automatic evidence reactions, 8 mutation-refresh events, and 8,061 ms of wall time.
These are deterministic single-host diagnostics, not cross-machine guarantees; the new
hydration, escalation, and completion-rejection counters remained zero in these release traces
because those branches are covered by focused unit/evaluator tests rather than this particular
fixture mix.

The current stripped release `aether` binary is 6,061,872 bytes (5.78 MiB), measured after the
orchestration changes: +98,432 bytes (+1.65%) versus the 5,963,440-byte post-checkpoint build.
A direct `/usr/bin/time -l target/release/aether --version` probe reported 7,352,320 bytes peak
RSS and 0.06 seconds wall time on this host. The binary-size increase is bounded state and
policy machinery; these figures are local diagnostics, not cross-platform guarantees.

## Agent-runtime phase closure baseline — 2026-08-30

This is the frozen single-host reference for the closed agent-runtime phase. The release
deterministic regression suite passed 9/9 fixtures in both baseline and optimized traces. The
optimized trace used 49/55 model requests, 40/46 requested tool calls, and 39/45 executed tool
calls (optimized/baseline), with one reused observation. Context input was 39,995/50,147 bytes,
so the 40,000-byte optimized gate passed with 5 bytes of headroom. Total model-visible bytes were
199,384/338,842 and tool-schema bytes were 159,389/288,695, a 44.8% schema reduction. It
recorded 12 automatic evidence reactions, 12 mutation-refresh events, 10 verification attempts,
one verification failure, one recovery attempt, two observed failure-state reductions, one expected
stale-evidence guardrail rejection, zero completion rejections, zero unresolved work items, and a
9/9 planner hit rate. Aggregate agent wall time was 18,678 ms.

The capability suite passed 5/5: checkpoint/resume 5/5, recovery 2, dirty-user-change
preservation 1/1, stale-observation handling 1, corruptions 0, false completions 0, and
unresolved work 0. The Git-backed Zero-Waste/control-loop ablation passed 2/2: the optimized
runtime preserved correctness and performed one runtime-owned automatic verification, while the
negative premature-completion case remained rejected three times in both control paths. A fresh
external `/usr/bin/time -l` run measured 91,807,744 bytes peak RSS and 19.55 seconds for the
regression evaluator, and 93,708,288 bytes peak RSS and 21.06 seconds for the capability
evaluator.

The stripped release binaries measured 6,123,416 bytes (`aether`) and 3,355,160 bytes
(`aether-eval`). Direct `aether --version` and `--help` probes completed in approximately 0.06
seconds; `--version` peaked at 7,458,816 bytes RSS and EOF/minimal startup peaked at 7,663,616
bytes. LCOV from the full workspace test run measured 20,136 of 24,886 lines covered (80.91%).
These are deterministic, local diagnostics rather than cross-platform guarantees. Reproduce the
reference with the release build, the two `aether-eval` commands in `evals/README.md`,
`cargo llvm-cov --workspace --all-features --locked --lcov`, and the preflight commands in
`AGENTS.md`.

## Interactive shell and model experience measurements — 2026-08-30

The release-facing shell was measured on the same local macOS host after `cargo build --release
--locked`. The stripped `target/release/aether` binary was 6,365,736 bytes (6.07 MiB), an
increase of 242,320 bytes (236.6 KiB, 3.96%) over the frozen agent-runtime binary at 6,123,416
bytes. This remains below the preferred 250 KiB feature-growth budget. Direct
`/usr/bin/time -l` probes measured 0.07 s and 7,385,088 bytes peak RSS for `--version`, 0.07 s
and 7,417,856 bytes for `--help`, and 0.07 s and 7,761,920 bytes for a piped EOF shell
startup. These are process probes, not cross-platform interactive-startup guarantees.

The full Criterion suite's selector probes measured 18.025 µs for slash-palette open/filter/confirm
and 395.74 µs for filtering and confirming a 1,000-entry model list (point estimates). Both are
local processing over an already loaded catalog and remain below the corresponding 2 ms / 1 ms
engineering targets. The benchmark does not include Rainy catalog retrieval, terminal transport
latency, or network time.

Headless acceptance probes sent `/help`, `/status`, `/doctor`, and `/exit` through a pipe with
`--non-interactive`: they produced 2,155 bytes, no stderr, no ANSI escape, and no prompt glyph.
The JSON shell sent `/help` and `/exit` with `--json --non-interactive`: it produced one JSON
line, no stderr, and no ANSI decoration. A PTY smoke drove `/help` and `/exit` through the native
palette and exited successfully. The default doctor remained offline; no live Rainy credential
was used in these measurements.

The frozen agent-runtime acceptance metrics above remain the comparison reference. The new UI
work does not alter WorkState, TurnDirectorState, adaptive tool selection, context/evidence
freshness, verification, permission admission, expected-hash mutation, dirty-worktree ownership,
scheduler behavior, or completion review; workspace tests and offline evaluator gates are required
again before release claims are made.
The final all-features LCOV run covered 21,936 of 27,711 lines (79.16%) across the workspace;
the earlier 80.91% figure above is the frozen-runtime baseline from its separate measurement.

## Rainy SDK 0.6.50 migration closure — 2026-08-31

The final locked release build after the Rainy SDK migration measured 6,320,592 bytes (6.03 MiB).
The earlier 6,365,736-byte value in the preceding shell-experience record is retained as historical
evidence; this final rebuild is the authoritative value for the migrated tree. Five direct
`/usr/bin/time -l` probes per command on the local macOS host measured the following; the table
reports medians with the observed range in parentheses:

| Probe | Wall time | Maximum RSS |
| --- | ---: | ---: |
| `aether --version` | 0.05 s (0.05–0.05) | 7,360,512 bytes (7,360,512–7,376,896) |
| `aether --help` | 0.05 s (0.05–0.05) | 7,368,704 bytes (7,368,704–7,368,704) |
| EOF/minimal startup | 0.15 s (0.15–0.15) | 7,655,424 bytes (7,643,136–7,671,808) |

The complete release Criterion suite finished successfully using the Plotters backend because
Gnuplot was not installed. Selected point estimates were 48.008 ms for process startup, 74.148 µs
for a small-file read, 1.7508 ms for a large-file read, 1.5275 ms for directory listing, 795.37 µs
for content search, 107.16 µs for tool dispatch, 60.085 µs for blocking dispatch, 14.027 µs for
slash-palette filtering, 381.69 µs for filtering a 1,000-entry model catalog, 1.7985 ms for an
atomic write, 1.9262 ms for staged patch application, and 4.1539 ms for session replay. These are
single-host diagnostics; the benchmark suite does not make Rainy network requests.

The final dependency graph contained 242 locked packages, 190 unique normal-tree nodes, and one
`rainy-sdk` version (`0.6.50`). Its migration removed the former `eventsource-stream`/`nom` parser
path and did not enable Rainy's account/session, legacy, rate-limiting, or tracing features. The
workspace `cargo deny check` and `cargo audit` runs passed with no banned-license, source, or
security-advisory findings. The all-features LCOV run covered 22,049 of 27,781 lines (79.37%).

The release deterministic evaluators passed the frozen 9/9 regression suite, the 5/5 capability
suite, and the 2/2 control-loop ablation after the migration. The final regression trace used
49/55 model requests, 39/40 executed tool calls, 39,995/50,147 context bytes, 199,384/338,842
model-visible bytes, and 13 process spawns; the capability trace used 15 requests, 9 tools, and
18,345 context bytes. The capability and control-loop gates also passed.
Offline native shell probes passed without a Rainy credential. No live provider smoke was run, so
network latency and remote service behavior remain unverified in this local freeze.
The final source-tree evaluator rerun reproduced the frozen 9/9 regression and 5/5 capability
results, with diagnostic agent wall times of 17,223 ms and 8,219 ms respectively; these are local
measurements rather than release-time guarantees.

## Native updater acceptance — 2026-09-01

The locked stripped macOS `aether` release binary measured 6,412,584 bytes (6.12 MiB) after adding
the explicit HTTPS/SHA-256 updater. Fresh local probes measured 0.06 seconds for `--version`,
0.05 seconds for `--help`, and 0.06 seconds for EOF/minimal startup. The version and help paths do
not construct the HTTP client; minimal startup also completed without a provider credential or
network operation. These are single-host measurements, not cross-platform guarantees.

## Optional Obscura provider — inactive/active measurement boundary

The Obscura integration is designed as a cold optional path. `aether --version`, `--help`, normal
startup, and turns without `/browser` do not spawn Obscura, open an MCP channel, create a browser
profile, or query Obscura release metadata. The compiled adapter contributes to AETHER's binary
and baseline startup only; the browser engine, its process memory, and its browser work are not
part of AETHER's idle RSS measurement.

Provider measurements must be reported separately for these states:

| State | Include | Do not attribute to AETHER idle cost |
| --- | --- | --- |
| AETHER inactive | AETHER RSS, startup, schemas, and local state checks | Obscura process or browser engine |
| AETHER + adapter | AETHER RSS and binary/schema delta with no provider process | Browser engine memory or network |
| Obscura active | AETHER plus the one Obscura process and MCP session | None; report parent and external RSS separately |
| Obscura/browser engine | External process RSS, startup, navigation, and page cost | AETHER binary or inactive RSS |

The active path reuses one Obscura process and one browser session for all turns. Only the six
fixed browser schemas are added at activation, and browser calls are serialized over one
`BrowserSession`; starting a fresh browser per turn would violate this resource goal. No live
Obscura benchmark or cross-platform RSS claim is published here yet. Release validation should
record fresh-process wall time/RSS for `--version`, normal EOF, `/browser status`, provider launch,
and one bounded audit, with the external process identified separately.

## Alpha-06 release-preparation measurements — 2026-09-03

On the local x86_64 macOS host with Rust `1.98.0`, the stripped native release binary measured
6,806,872 bytes before the alpha-06 version bump and browser extension and 6,810,960 bytes after
the full change, a delta of 4,088 bytes (+0.06%). The `aether-web` crate is not linked into that
binary. These are local artifact measurements, not cross-platform guarantees.

The checked-in browser bundle measured 8,362 bytes for the generated JavaScript glue and 171,256
bytes for the optimized WASM module. The complete static output (`index.html`, CSS, app JavaScript,
and the adjacent WASM bundle) was 225,245 bytes before filesystem block rounding. `npm --prefix web
run build:wasm`, `npm --prefix web run build`, and `node --check web/app.js` completed successfully.

The release-preparation run passed the four required locked native gates, 366/366 tests under
`cargo nextest`, both deterministic evaluator suites (9/9 regression fixtures, 5/5 capability
fixtures, and 2/2 control-loop checks), `cargo deny check`, `cargo audit`, and the full all-features
LCOV run. Browser QA in the Codex in-app browser registered six tools and exercised inspect,
read, search, propose, staged verification, human Accept, human Reject, and shared-state rendering.
The browser automation backend could not directly invoke page tools because its WebMCP command
surface is unsupported by the selected model; registration was verified by the page's own status
badge and the visible workflow was tested through the same callbacks.
