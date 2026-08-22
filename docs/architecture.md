# Architecture

AETHER Fx is one binary and one process. The dependency direction is intentionally narrow:

```text
aether
  ├── aether-agent
  │     └── aether-core
  ├── aether-rainy
  │     ├── aether-agent (implements ModelBackend)
  │     └── rainy-sdk
  ├── aether-tools
  │     └── aether-core
  └── aether-terminal
        └── aether-core
```

The runtime path is:

```text
terminal input → aether-agent → ModelBackend → aether-rainy → Rainy SDK → Rainy API
                         └────── ToolExecutor → aether-tools
agent events ───────────────────────────────────────→ aether-terminal
```

`aether-core` owns IDs, bounded values, typed errors, paths, permissions, events, and model/tool request primitives. It has no terminal, Rainy, or platform dependency.

`aether-agent` owns the backend-neutral turn loop, cancellation, continuation, and session state. Its `ModelBackend` contract uses bounded Tokio channels and does not expose Rainy types.

`aether-rainy` is the only crate that imports `rainy_sdk`. It maps the SDK's dynamic Responses stream values into AETHER `ModelEvent` values and treats provider reasoning metadata as opaque continuation data. No provider endpoint, model catalog, or key appears elsewhere.

`aether-tools` owns exactly nine model-visible tools and implements workspace containment, output bounds, permissions, filesystem, process, search, patch, and read-oriented Git behavior.

`aether-terminal` owns raw input, restoration, ANSI/VT presentation, incremental buffer rendering, and platform modules. Tools and agent code never write ANSI.

There is no RPC, daemon, database, plugin system, WebView, TUI framework, or alternate allocator in this bootstrap.
