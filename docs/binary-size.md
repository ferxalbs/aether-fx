# Production binary size audit

This audit measures the x86_64 macOS release artifact built with Rust 1.98.0. The production
artifact uses fat LTO, one codegen unit, aborting panic, stripped symbols, no debug information,
and no native-CPU specialization. Agent decision paths retain speed optimization. Linux section totals will differ, so release automation should
run the Linux startup probe and retain its exact artifact byte count.

## Result

Changing the general release optimization goal from speed (`opt-level=3`) to size
(`opt-level="s"`), while retaining `opt-level=3` for `aether-agent`, reduced the stripped `aether`
binary from 8,402,256 to 5,737,800 bytes: 2,664,456 bytes saved (31.71%). The result is 553,656
bytes below the 6 MiB stretch target.

| Mach-O section | Before | After | Change |
| --- | ---: | ---: | ---: |
| `__TEXT,__text` | 6,995,873 | 4,609,412 | -2,386,461 |
| `__TEXT,__const` | 931,576 | 655,992 | -275,584 |
| `__TEXT,__cstring` | 93,402 | 94,687 | +1,285 |
| `__TEXT,__unwind_info` | 19,392 | 22,472 | +3,080 |
| `__TEXT,__eh_frame` | 17,128 | 11,480 | -5,648 |
| `__DATA,__const` | 271,420 | 271,484 | +64 |

An unstripped diagnostic relink supplied symbol sizes; it was not the measured production
artifact. Its largest remaining symbols were:

| Symbol or subsystem | Bytes |
| --- | ---: |
| AWS-LC Ed25519 scalar-base implementation, alternate | 61,243 |
| AWS-LC Ed25519 scalar-base implementation | 60,989 |
| AWS-LC X25519 scalar-base implementation, alternate | 60,688 |
| AWS-LC X25519 scalar-base implementation | 60,475 |
| `regex_automata::meta::strategy::new` | 53,092 |
| `Agent::run_with_broker` future | 42,285 |
| AWS-LC object-name table | 39,960 |
| `ToolRegistry::dispatch_with_context` future | 38,930 |
| Tool schema definitions | 37,864 |
| `encoding_rs` Big5 low-bit table | 37,680 |

AWS-LC native objects contribute 1,043,011 symbol bytes. BLAKE3 architecture-specific objects
contribute 61,125 bytes. Fat-LTO combines Rust code into one object, so remaining Rust dependency
and application code cannot be attributed to crates reliably from this link map.

## Production dependency and feature audit

`cargo tree -p aether --edges normal` contains neither `aether-eval` nor Criterion. Evaluation is a
separate workspace package; benchmark code is in Cargo benchmark targets and Criterion is only a
development dependency. Release settings disable debug information and incremental compilation.
Rust `cfg(test)` code is not compiled into the production target.

Workspace dependencies already disable default features for BLAKE3, Futures, Globset, Rainy SDK,
Regex Automata, Rustix, Tokio, and Windows bindings. Rainy SDK default features are disabled, which
excludes its rate-limiting and tracing surfaces. Rainy SDK 0.6.50 still requires Reqwest JSON,
streaming, HTTP/2, and Rustls; Reqwest 0.13 selects AWS-LC through its `rustls` feature. Removing
HTTP/2, TLS 1.2, platform certificate verification, streaming, Unicode URL processing, or provider
cryptography would change provider compatibility or security and was rejected.

Serialization and error handling remain necessary for model protocol, bounded tool schemas,
session persistence, continuation state, and typed failure propagation. Lexopt is the only CLI
parser. Formatting code remains visible in several async state machines, but replacing typed
formatting and errors with ad hoc encoders would add risk for small, unproven savings. Regex
Automata's NFA and hybrid engines remain enabled because search throughput and worst-case bounded
behavior have runtime value.

Size optimization materially reduced duplicated async/generic state-machine code and formatting,
serialization, HTTP, and tool-dispatch monomorphizations. Further manual type erasure was rejected:
the largest application futures are behavior-critical, and no isolated duplicate instantiation
offered comparable savings without dynamic dispatch or invasive API changes.

`opt-level="z"` produced a smaller 4,960,080-byte artifact, but a short comparison reported an
18.17% pure-planner regression and a 5.38% bounded-execution regression. It was rejected. Global
`opt-level="s"` alone produced 5,578,096 bytes but slowed the agent benchmark. Retaining
`opt-level=3` only for `aether-agent` produced the final 5,737,800-byte artifact. A 20-sample final
planner run found no statistically significant pure-planning change; bounded execution improved
against the immediately preceding size-optimized run. Process-startup Criterion measured a
51.675 ms median estimate and reported no statistically significant change.

## Startup audit

`run` parses CLI arguments before constructing Tokio. `--version`, `--help`, `doctor`, and
`sessions` do not construct a runtime. Interactive startup constructs a current-thread runtime but
reads the first prompt before loading model configuration, Rainy credentials, workspace state,
tools, agent context, renderer, RepoMap, symbol index, policy, planner, or verification state. EOF
therefore exits before all nonessential agent initialization. RepoMap and symbols remain lazy;
policy, planner, context, verification, and tools are constructed only after actual prompt input.

Ten fresh-process probes on the audit host used `/usr/bin/time -p`, whose 10 ms resolution makes
small differences directional rather than precise:

| Command | Before mean | After mean | Change |
| --- | ---: | ---: | ---: |
| `aether --version` | 110 ms | 51 ms | -59 ms |
| `aether --help` | 110 ms | 50 ms | -60 ms |
| `aether </dev/null` | 108 ms | 53 ms | -55 ms |

`benches/startup.sh` provides the matching Linux fresh-process method with 30 samples by default,
GNU `time`, separate version/help/minimal-agent results, and the exact stripped artifact size.
