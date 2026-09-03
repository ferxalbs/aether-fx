# AETHER Fx

AETHER Fx is an experimental native terminal coding agent. AETHER is one local binary with a
bounded set of filesystem, process, search, patch, and Git tools.

## Alpha status

`v0.1.0-alpha-06` is an alpha preview. It is not stable or production-ready software. This
release is distributed through GitHub Releases for the six native platforms listed below and adds
an experimental static WebMCP browser vertical slice.

### Experimental WebMCP browser surface

The [AETHER WebMCP vertical slice](docs/webmcp.md) is a small deterministic browser workspace,
not a web IDE. It runs a four-file demo repository in Rust/WASM, registers six bounded page tools
when the host supports `document.modelContext.registerTool`, and routes visible UI and agent calls
through the same state. An agent can inspect, read, search, propose, and verify; only the visible
human Accept/Reject gate can apply the staged patch. It has no native filesystem, shell, network,
credentials, or browser-to-CLI bridge. The public deployment URL is recorded in the linked
challenge submission kit when verified.

## Install

### macOS and Linux

The version-pinned installer downloads only from this repository's GitHub Release, verifies the
archive with `SHA256SUMS`, installs to `$HOME/.local/bin`, and does not edit shell profiles:

```sh
curl -fsSL \
  https://raw.githubusercontent.com/ferxalbs/aether-fx/v0.1.0-alpha-06/install.sh \
  | sh -s -- --version v0.1.0-alpha-06
```

Use `--dir <path>` to choose another user-owned installation directory.

### Windows PowerShell

Download the tagged installer, then run it with the explicit prerelease version. It installs to
`%LOCALAPPDATA%\AETHER\bin` without administrator privileges or PATH changes:

```powershell
Invoke-WebRequest `
  https://raw.githubusercontent.com/ferxalbs/aether-fx/v0.1.0-alpha-06/install.ps1 `
  -OutFile install.ps1

.\install.ps1 -Version v0.1.0-alpha-06
```

Use `-Dir <path>` to choose another user-owned installation directory.

### Manual archive installation

Download the archive for your platform and `SHA256SUMS` from the
[v0.1.0-alpha-06 GitHub Release](https://github.com/ferxalbs/aether-fx/releases/tag/v0.1.0-alpha-06).
Verify the archive before extracting it, then place `aether` or `aether.exe` in a user-owned
directory on your PATH. Each archive contains only the executable, its CycloneDX SBOM, `LICENSE`,
`NOTICE`, and `THIRD_PARTY_NOTICES.md`.

### Update from the CLI

After installing a direct binary, run the explicit updater whenever you want to check
for a newer public AETHER Fx release:

```sh
aether update
aether update --json
```

The updater includes stable releases and prereleases, selects the highest valid SemVer, downloads
the direct binary for the current platform, and verifies it with HTTPS plus the release
`SHA256SUMS` manifest before replacing the executable atomically. It uses no automatic startup
check, cache, backup, telemetry, sudo, UAC, package-manager integration, or permanent helper.
Only a regular writable executable can be updated; symbolic links, reparse points, read-only files,
and non-writable parent directories are rejected. Custom installation directories and writable
`target/debug` or `target/release` binaries are supported. Reqwest uses
the operating system's certificate and proxy behavior, but the release source and asset identity
remain fixed to this repository.

On Windows, the validated replacement is handed to a short-lived hidden helper because the running
`.exe` cannot replace itself synchronously. The command exits with the handoff accepted, and the
next AETHER Fx start confirms the installed version. `/update` provides the same operation inside
the shell and exits the shell after the result.

## Configure

Installation and provider configuration are separate stages. Check the local setup without an API
request:

```sh
aether --version
aether --help
aether doctor
```

Inference and model discovery require a Rainy API key. The key is read only when a Rainy
operation is requested and is never printed, persisted, placed in command arguments, or sent to
the model. The simplest setup is an environment variable:

```sh
export RAINY_API_KEY='your-key'
aether models
```

For a persistent non-secret default, use the private application configuration layer rather than
editing environment state:

```sh
aether config path
aether config show
aether config edit
```

The configuration file is JSON and may contain `default_model` plus an optional direct
`auth.credential_helper` argv array. It never stores an API key. `aether config show` reports only
whether a helper is configured, so its output is safe to paste into an issue. The editor is
invoked directly, without a shell; helper output is bounded and used only for the current
process.

Model resolution is deterministic: explicit `--model`, then the model stored with a resumed
session, then `default_model` from config, then the compatibility `AETHER_MODEL` environment
variable, then interactive `/model` selection when a TTY is available. `/model` changes only
future turns and never edits environment variables. `--model <id>` therefore overrides every
stored or environment default for that invocation.

Use the separate authentication surface when guided setup is preferable:

```sh
aether auth
```

On a TTY, `aether auth` can accept a key without echoing it and keeps it in process memory only.
In a pipe, CI job, or `--non-interactive` invocation it reports status without attempting a
secret prompt. An environment variable or configured helper is preferred for automation.

AETHER does not require Rust, Cargo, Bun, the Rainy SDK, or a Rainy API deployment after
installation. `rainy-sdk` is compiled into AETHER; the Rainy API is the hosted remote inference
service.

## Run

From the repository you want to inspect:

```sh
cd path/to/repository
aether
```

For one-shot use:

```sh
aether --model <id> "inspect this repository and explain the highest-risk issue"
```

The bare `aether` command opens the stateful shell only when stdin and stdout are terminals. A
leading `/` opens the local command palette; slash commands such as `/model`, `/status`,
`/context`, `/config`, `/auth`, `/sessions`, `/resume`, `/doctor`, `/update`, `/help`, `/clear`, and `/exit`
are handled locally and are never sent to Rainy. Natural text containing `/` remains ordinary
prompt text. Arrow keys or `j`/`k` navigate selectors, Enter confirms, and Escape or Ctrl-C
cancels.

For scripts and CI, use `--non-interactive` or a pipe. `--json` emits structured records without
a startup banner, prompt, selector, or ANSI decoration; local slash commands still remain local.
The default `aether doctor` is offline and reports whether the local executable can be updated;
it never contacts GitHub. `aether models` explicitly retrieves the live Rainy catalog, while
`aether doctor --network` adds an explicit catalog/reachability probe.

### Optional native browser provider

Obscura is an optional external provider. Use `/browser` to install the exact pinned release after
an explicit consent prompt, or `/browser <task>` for bounded, read-only web research. Installation
shows the target, asset size, source URL, SHA-256, and MCP protocol; it never resolves `latest`,
uses a package manager/elevation, or treats `--yolo` as download consent. The verified provider
state and workspace-isolated browser storage live outside the repository. `/browser status`,
`/browser stop`, and `/browser update` manage the process and fixed pin; `stop`, `/clear`, and
session exit retain the private profile but do not retain the process or active tabs.

The active model surface contains only `browser.tabs`, `browser.navigate`, `browser.snapshot`,
`browser.find`, `browser.wait`, and `browser.performance_audit`. It does not expose clicks, typing,
submits, arbitrary JavaScript, downloads, screenshots, or generic MCP passthrough. A normal coding
turn can reuse an already healthy provider, while the inactive path adds no Obscura process,
browser profile, socket, or release-catalog request. The current upstream v0.2.1 binary blocks
loopback as part of its broad private-network policy, so AETHER supports public HTTP/HTTPS only
until a narrow loopback contract exists. See [`docs/obscura-provider.md`](docs/obscura-provider.md).

## Resume

Sessions are stored in private OS application-state storage outside the repository:

```sh
aether sessions
aether resume <session-id>
aether resume --latest
```

## Supported platforms

The alpha release provides native archives for:

- macOS x86_64
- macOS aarch64/arm64
- Linux x86_64 GNU
- Linux aarch64 GNU
- Windows x86_64
- Windows aarch64/ARM64

## Security and verification

AETHER keeps filesystem operations inside a canonical workspace, bounds model-visible and process
output, separates typed permissions from rendering, and never stores `RAINY_API_KEY`. The normal
download flow verifies the archive against `SHA256SUMS`; the CLI updater verifies a direct binary
against the same release manifest before replacement. `sha256sum` or `shasum -a 256` is used by
the Unix installer. GitHub CLI users may additionally verify an archive's build attestation:

```sh
gh attestation verify aether-v0.1.0-alpha-06-linux-x86_64-gnu.tar.gz \
  --repo ferxalbs/aether-fx
```

Attestation verification is optional; GitHub CLI is not required to install or run AETHER. See
[`SECURITY.md`](SECURITY.md) and [`docs/security-model.md`](docs/security-model.md) for details.

## Build from source

This section is for developers. Rust `1.98.0` is pinned in `rust-toolchain.toml`.

```sh
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo run --locked -p aether -- --version
```

To rebuild the experimental browser bundle, install the pinned WASM target and
`wasm-bindgen-cli`, then run:

```sh
rustup target add wasm32-unknown-unknown
cargo install --locked --version 0.2.127 wasm-bindgen-cli
npm --prefix web run build:wasm
npm --prefix web run build
```

The static output is `web/dist`; `web/wasm` is checked in so a static host does not need Rust.
See [`docs/webmcp-challenge.md`](docs/webmcp-challenge.md) for the challenge description and
short demo script.

The release workflow builds the production binaries on matching GitHub runners; a locally built
`target/release/aether` is not a release artifact.

## License

Apache-2.0. See [LICENSE](LICENSE).
