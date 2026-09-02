# Security policy

AETHER Fx is experimental. Please report security issues privately to the repository maintainers once repository contact metadata exists; do not disclose an exploitable issue in a public issue first.

The bootstrap security boundary is documented in `docs/security-model.md`. In particular, filesystem access is contained by a canonical workspace root, process output is bounded, permissions are typed, and `RAINY_API_KEY` is never persisted or rendered.

The explicit `aether update` command is the only AETHER release network path. It uses fixed HTTPS
GitHub URLs, system certificate/proxy behavior, direct target binaries, bounded streaming, and
SHA-256 verification before an atomic replacement. It does not send Rainy or GitHub credentials,
does not elevate privileges, and refuses links, read-only destinations, non-regular files, and
non-writable parent directories.

There is no telemetry or crash upload in AETHER Fx.
