# Obscura provider

AETHER Fx can optionally supervise [Obscura](https://github.com/h4ckf0r0day/obscura) as an
external browser provider. The integration is deliberately opt-in: the normal AETHER path does
not start Obscura, open a browser, create a browser profile, or query an Obscura release catalog.

The first supported pin is [Obscura v0.2.1](https://github.com/h4ckf0r0day/obscura/releases/tag/v0.2.1).
The adapter was checked against the [Obscura documentation](https://docs.obscura.sh/) and the
published release assets on 2026-09-02. The static manifest is source code in
`crates/aether-obscura/src/manifest.rs`; it is not generated or replaced at runtime.

## User flow

The interactive shell recognizes these local commands:

```text
/browser                         install/start and show status
/browser <task>                  run bounded, read-only external research
/browser status                  show the fixed pin and provider health
/browser stop                    stop Obscura and retain its private profile state
/browser update                  install the pin compiled into this AETHER release
```

The first install prints the exact version, target, asset, byte count, source URL, SHA-256, and
MCP protocol version. AETHER asks for a separate interactive confirmation. `--yolo` never answers
this prompt and never authorizes an external download. A non-interactive invocation fails before
creating provider files or starting a process.

After consent, AETHER downloads the fixed asset, follows only HTTPS redirects required by the
GitHub release CDN, checks the exact byte count and SHA-256, extracts only the expected binary and
worker, validates `obscura --version`, and atomically activates the version directory. A failed
download, extraction, integrity check, or version check leaves the previous active installation
untouched. Installation uses no package manager, elevation, shell, credential-bearing URL, or
`RAINY_API_KEY` inheritance.

`/browser <task>` selects the `ExternalResearch` turn mode. That turn can use the six browser
tools plus `read`, `list`, `find`, `search`, and read-only `git`; it cannot use `write`, `patch`,
`shell`, or `process`. The provider remains available to later ordinary repository turns until
`/browser stop`, `/clear`, `/exit`, `/quit`, or normal session shutdown. `/resume` restores only
AETHER's bounded local state and never reanimates Obscura.

## Static release manifest

The v0.2.1 release does not publish a Windows aarch64 asset. AETHER therefore reports that target
as unsupported instead of guessing an asset or downloading a different architecture. The five
published assets currently embedded in the adapter are:

| Target | Asset | Exact size | SHA-256 |
| --- | --- | ---: | --- |
| `x86_64-apple-darwin` | `obscura-x86_64-macos-no-render.tar.gz` | 45,342,726 | `40f6551104b6ede43026710b9fb1a922db000524744fa50b7f2baac77d42f526` |
| `aarch64-apple-darwin` | `obscura-aarch64-macos-no-render.tar.gz` | 43,552,683 | `c14d0565a2c4c432551957e5cef5df120a1290901492569aa148957a5e637d25` |
| `x86_64-unknown-linux-gnu` | `obscura-x86_64-linux-no-render.tar.gz` | 47,658,644 | `bf60fff504f15bf6e16b22cbbeefe99348f247d7f95d6bde5d06b34a7d9d9d9c` |
| `aarch64-unknown-linux-gnu` | `obscura-aarch64-linux-no-render.tar.gz` | 49,404,469 | `e8cf49330a56e695e75c15b5680cfd4293966f3493ccc1698b538350e4f9c112` |
| `x86_64-pc-windows-msvc` | `obscura-x86_64-windows-no-render.zip` | 40,480,291 | `323e0f317af25aaeb18bc2cd0c27db9025361d9b11c7ce278beee42d39073afe` |

Every archive is expected to contain exactly `obscura` and `obscura-worker` (with `.exe` suffixes
on Windows). The expected version output is exactly `obscura 0.2.1`, and the MCP protocol pin is
`2024-11-05`. A new Obscura pin requires a new AETHER release and a new manifest entry; neither
`latest` nor a release API is consulted.

## Process and MCP boundary

AETHER launches one absolute, verified binary directly with the static `mcp` argument. The
process runs with a private working directory, a cleared environment, piped `stdin`/`stdout`, and
continuously drained, 32 KiB-capped `stderr`. The adapter uses one serialized newline-delimited
JSON-RPC channel and one in-flight browser operation. It bounds each frame to 1 MiB, each tool
result to 256 KiB, and each request to 120 seconds. Timeout, cancellation, malformed framing,
unexpected server messages, result overflow, protocol errors, or an unexpected process exit
invalidate the provider; AETHER does not silently restart or download it.

Startup is fixed:

1. `initialize` with protocol `2024-11-05`.
2. Validate the returned protocol and capabilities object.
3. Send `notifications/initialized`.
4. Call `tools/list` and validate the required provider schemas.
5. Ignore every extra announced tool.

The v0.2.1 MCP server does not provide a protocol shutdown method. AETHER closes `stdin`, waits
for two seconds, and terminates the parent process if necessary. The provider's own shutdown
contract is responsible for cleaning browser descendants; the model never receives generic
process-control access.

## Fixed model surface

Only these names are registered with Rainy. The descriptions and schemas are compiled in AETHER;
Obscura descriptions cannot expand the model prompt or grant capabilities.

| AETHER tool | Obscura operation | Behavior |
| --- | --- | --- |
| `browser.tabs` | `browser_tab_list` | Bounded tab and active-tab information |
| `browser.navigate` | `browser_navigate` | Navigate the active tab to a validated HTTP(S) URL |
| `browser.snapshot` | `browser_snapshot` | Bounded URL, title, and visible text |
| `browser.find` | `browser_search` | Bounded visible-text matches and context |
| `browser.wait` | `browser_wait_for` | Bounded wait for a CSS selector |
| `browser.performance_audit` | `browser_navigate` plus a fixed `browser_evaluate` expression | Read-only navigation and structured timing metrics |

The internal `browser_evaluate` call is never model-visible and always uses the fixed audit
expression. AETHER does not expose click, fill, type, submit, key presses, arbitrary JavaScript,
downloads, screenshots, PDF export, network interception, or arbitrary MCP passthrough.

`browser.performance_audit` returns bounded JSON fields for final URL, redirects, navigation
duration, initial response, `DOMContentLoaded`, `load`, FCP, LCP, CLS, resource count, transferred
bytes, warnings, and missing metrics. v0.2.1 cannot provide a complete redirect chain through
this fixed operation, so the normalized result records that limitation in `warnings`.

All browser actions use `BrowserRead` in v1 and an exclusive `BrowserSession(id)` footprint. The
first browser call can receive `AllowSession`; the reserved `BrowserAction` class cannot be
exposed by this release and is never converted into a session grant.

## Private profile and persistence

Provider state is outside the repository under the AETHER state root:

```text
<state-root>/obscura/bin/0.2.1/obscura
<state-root>/obscura/bin/0.2.1/obscura-worker
<state-root>/obscura/profiles/<workspace-id>/browser_storage_state.json
```

Provider directories are private where the platform permits it. The adapter does not pass
`--storage-dir` because v0.2.1's `mcp` subcommand has no such option. Instead, it uses the fixed
provider storage-state operations to save and restore bounded cookies/localStorage state in the
workspace-isolated private profile. Cookies are restored at provider startup; origin storage is
applied after a matching public-origin navigation because v0.2.1 requires an active page for that
part of the operation. Active tabs and the Obscura process are not restored. The state file is
capped at 1 MiB and written atomically; it is never copied into the workspace.

Persisted AETHER JSONL contains only status summaries such as `browser.snapshot ok` or
`browser.snapshot failed`. It does not contain page bodies, HTML, cookies, authorization headers,
MCP payloads, request IDs, process IDs, or page instructions. Browser results are available to the
current live turn and are discarded from durable evidence.

## Network policy and known v0.2.1 limitation

AETHER rejects non-HTTP(S) schemes, embedded credentials, and literal private, metadata,
unique-local, link-local, multicast, unspecified, documentation, and reserved addresses. The
upstream Obscura resolver and redirect checks are required for the effective DNS and redirect
policy, because those checks must cover the address actually contacted. AETHER never passes
`--allow-private-network` or `OBSCURA_ALLOW_PRIVATE_NETWORK`.

The published v0.2.1 binary has one broad private-network opt-in rather than a loopback-only
allowlist. Its safe default consequently blocks `localhost`, `127.0.0.0/8`, and `::1` too. The
adapter rejects those destinations with a clear provider-capability error rather than enabling
the broad flag. Public HTTP/HTTPS research is supported in this release; a local site must be
published through a separately authorized, future narrow loopback contract before it can be
audited by AETHER. `file:`, `data:`, `javascript:`, `chrome:`, and `devtools:` are never passed
to Obscura.

## WebMCP boundary

WebMCP is not dynamically implemented in this release and AETHER has no dependency on a private
OpenAI API. If Obscura later discovers page-provided tools, the adapter must receive validated
capabilities rather than raw page messages. A future dynamic tool identity will include origin,
tab, document, and schema version; navigation invalidates the old page surface; a list-change
event triggers capability validation instead of silently extending the registry. Page-provided
descriptions and instructions remain untrusted content and can never authorize the user or
change AETHER policy.

## Upstream licensing

The v0.2.1 Obscura project and its published binary are Apache-2.0 licensed. The no-render assets
include Obscura's browser engine and worker; release packaging must retain the upstream license
and notices. The upstream repository also identifies vendored `taffy` code under MIT. AETHER's
adapter adds no MCP SDK and keeps its own direct archive/parser dependency notices in
`THIRD_PARTY_NOTICES.md`.
