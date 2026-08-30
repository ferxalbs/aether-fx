# Rainy integration

The only provider boundary is `aether-rainy`. It depends on the verified `rainy-sdk 0.6.16` crate and uses its `RainyClient`, `ResponsesRequest`, `create_response_stream`, and `get_models_catalog` APIs. The adapter maps the SDK's dynamic Responses stream values into AETHER `ModelEvent` values without leaking SDK types.

The SDK version that was requested as a future `6.5.x` release was not present in crates.io or the official repository at bootstrap time. AETHER therefore uses the latest real stable SDK that supports the foundational Responses/stream/catalog surface: `0.6.16` (SDK Rust MSRV 1.92, compatible with AETHER Rust 1.98). This is an explicit blocker for any SDK 6.x-specific contract; no fabricated API is used.

Model catalogs are lazy and cached by the adapter layer when the caller asks for them. Startup and the initial terminal prompt do not construct a Rainy client or perform a network request. `RAINY_API_KEY` is required only for an explicit inference/catalog operation.

Normal inference uses deterministic model precedence in the binary: explicit `--model`, resumed
session model, non-secret config `default_model`, compatibility `AETHER_MODEL`, then interactive
selection when a TTY is available. The adapter does not use the first catalog item as an implicit
model and does not fetch the catalog merely to validate a selected model. The `models` subcommand
and `/model` remain explicit catalog-discovery paths.

## Normalized catalog contract

`RainyBackend::discover_model_views` converts the live SDK entries into one bounded `ModelView`
projection shared by `/model`, `aether models`, `/status`, and JSON output. The projection consumes
the SDK's authoritative fields rather than model-name heuristics:

- identity and display name;
- physical/model context, authenticated plan context, and effective usable context;
- product tier and billing class;
- tools, reasoning, reasoning modes/values/parameter paths, numeric thinking-budget bounds and
  optional dynamic/disable values, supported parameters, and input/output modalities;
- organization entitlement, privacy compatibility/mode, and three-valued availability;
- policy status, disclosure requirement/notice, training-with-inputs, ZDR availability, and bounded
  retention metadata.

The normalization order is v2 capability data, then v1 capability flags, then supported parameter
names where the SDK does not provide a richer block. Missing facts remain `unknown`. Explicit
organization/privacy denial or an explicit lack of tool support makes an entry unavailable for the
coding shell; unknown metadata remains selectable only after an explicit acknowledgement. A
disclosure-required entry is shown with policy details before confirmation. All catalog text is
bounded and terminal-sanitized before it reaches a selector.

The primary selector is intentionally compact: human name plus model ID, effective context, tier,
tools, reasoning, and access. The JSON form retains the normalized fields for machine consumers;
it does not expose an unbounded copy of the provider response.

Rainy's transport/retry behavior remains Rainy's/SDK's responsibility. AETHER emits semantic step IDs and does not blindly retry ambiguous tool/model operations. The verified 0.6.16 ResponsesRequest surface has no documented idempotency field, so AETHER does not invent one; the IDs remain the local semantic identity until an SDK contract exposes transport integration. Reasoning and continuation fields are opaque values; raw chain-of-thought is never rendered or exposed as a tool result.

## Continuation invalidation and typed recovery

`rainy-sdk 0.6.16` has no continuation-specific error type. AETHER maps to `BackendError::InvalidContinuation` only with structured protocol evidence:

- locally, a continuation object is present but has no non-empty `previous_response_id` string;
- a Rainy `InvalidRequest` whose machine-readable `code` is `previous_response_id`, `invalid_previous_response_id`, or `INVALID_PREVIOUS_RESPONSE_ID`, and only when that request attached a previous response id.

Other SDK variants (`Network`, `Api`, `Provider`, `Authentication`, and `InvalidRequest` with any other code) stay ordinary backend errors. The agent never inspects user-facing error text. On `InvalidContinuation` at the first step of a turn that still has continuation, it clears continuation, reconstructs from bounded local context, and retries once. Unrelated backend errors are not retried as continuation failures.

Resume also drops continuation when the live workspace root differs or inspected files are stale or missing, so Rainy remote context cannot outrank the filesystem.

## Responses stream completion

The adapter treats `response.completed` as the only successful terminal event. It requires the event to carry a non-empty response ID and stores that ID as opaque `previous_response_id` continuation metadata; continuation is never fabricated when transport state disappears. Explicit `response.failed` and `response.incomplete` events become typed backend errors with bounded safe diagnostic text.

An EOF before a valid terminal event is `IncompleteStream`, including after text deltas or a parseable function-call event. The adapter does not emit `Done` for that stream. Partial function-call argument deltas are discarded, and a completed function-call event followed by EOF still fails the enclosing agent step before any tool execution. If the receiving side is dropped, the adapter observes the bounded output channel's closed state and stops consuming the Rainy stream.

The catalog cache uses a bounded five-minute TTL. Bootstrap refreshes stale entries synchronously on an explicit catalog/model operation; stale-while-refresh background refresh is intentionally not enabled until its task lifetime and cancellation semantics are proven.
