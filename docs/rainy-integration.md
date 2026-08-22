# Rainy integration

The only provider boundary is `aether-rainy`. It depends on the verified `rainy-sdk 0.6.14` crate and uses its `RainyClient`, `ResponsesRequest`, `create_response_stream`, and `get_models_catalog` APIs. The adapter maps the SDK's dynamic Responses stream values into AETHER `ModelEvent` values without leaking SDK types.

The SDK version that was requested as a future `6.5.x` release was not present in crates.io or the official repository at bootstrap time. AETHER therefore uses the latest real stable SDK that supports the foundational Responses/stream/catalog surface: `0.6.14` (SDK Rust MSRV 1.92, compatible with AETHER Rust 1.98). This is an explicit blocker for any SDK 6.x-specific contract; no fabricated API is used.

Model catalogs are lazy and cached by the adapter layer when the caller asks for them. Startup and the initial terminal prompt do not construct a Rainy client or perform a network request. `RAINY_API_KEY` is required only for an explicit inference/catalog operation.

Rainy's transport/retry behavior remains Rainy's/SDK's responsibility. AETHER emits semantic step IDs and does not blindly retry ambiguous tool/model operations. The verified 0.6.14 ResponsesRequest surface has no documented idempotency field, so AETHER does not invent one; the IDs remain the local semantic identity until an SDK contract exposes transport integration. Reasoning and continuation fields are opaque values; raw chain-of-thought is never rendered or exposed as a tool result.

The catalog cache uses a bounded five-minute TTL. Bootstrap refreshes stale entries synchronously on an explicit catalog/model operation; stale-while-refresh background refresh is intentionally not enabled until its task lifetime and cancellation semantics are proven.
