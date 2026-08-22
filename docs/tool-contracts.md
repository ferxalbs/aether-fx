# Tool contracts

The model-visible registry has exactly nine names:

| Tool | Permission | v0.1 contract |
| --- | --- | --- |
| `read` | ReadOnly | One or more bounded workspace file reads with line ranges, UTF-8/binary detection, partial output, and line numbers |
| `list` | ReadOnly | Ignore-aware bounded directory entries with depth, hidden policy, and metadata |
| `find` | ReadOnly | Ignore-aware path/name globs such as `**/*.rs` |
| `search` | ReadOnly | Ignore-aware bounded literal/regex content search with globs, case options, context, and binary exclusion |
| `write` | WorkspaceWrite | Bounded UTF-8 replacement/create with optional BLAKE3 precondition and atomic same-directory temp file where supported |
| `patch` | WorkspaceWrite | Multiple strict hunks/files, preconditions, dry-run, and best-effort rollback |
| `shell` | ProcessExecute | Direct `program` plus `arguments`, `cwd`, exit status, duration, bounded stdout/stderr, and truncation metadata |
| `process` | ProcessPersistent | `start`, `read`, `write`, `status`, and `kill` with stable process IDs |
| `git` | ReadOnly in v0.1 | Structured status, diff, show, log, and branch inspection through the installed Git executable |

Internal helpers are not registered as model-visible tools. Each request and result is typed and serializable; output is capped before it reaches model context.
