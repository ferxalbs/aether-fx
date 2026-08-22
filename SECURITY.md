# Security policy

AETHER Fx is experimental. Please report security issues privately to the repository maintainers once repository contact metadata exists; do not disclose an exploitable issue in a public issue first.

The bootstrap security boundary is documented in `docs/security-model.md`. In particular, filesystem access is contained by a canonical workspace root, process output is bounded, permissions are typed, and `RAINY_API_KEY` is never persisted or rendered.

There is no telemetry or crash upload in AETHER Fx.
