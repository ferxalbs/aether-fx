# Rainy integration

The only provider boundary is `aether-rainy`. It depends on the exact `rainy-sdk 0.6.50` crate and uses its `RainyClient`, `ResponsesRequest`, `create_response_stream`, and `get_models_catalog` APIs. The adapter consumes the SDK's typed public `ResponsesStreamEvent` stream and maps it into AETHER `ModelEvent` values without leaking SDK types.

The manifest keeps Rainy's minimal feature set with `default-features = false`; no account/session,
legacy, rate-limiting, or tracing surface is enabled. AETHER keeps its own environment-key,
configuration, session, and state boundaries; the SDK's opt-in account/session APIs are not part of
the integration.

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
- model context, tools, reasoning, supported parameters, and input/output modalities;
- bounded display and selection metadata used by `/model`, `aether models`, `/status`, and JSON
  output;
- compatibility fields retained by AETHER's public model projection, with plan/effective context,
  tier/billing, organization/privacy, and data-policy values left unknown when the SDK catalog does
  not provide them.

The normalization order is the SDK's v1 capability flags, then exact supported parameter names where
the SDK does not provide a flag. Missing facts remain `unknown`; no model-name or provider heuristic
fills them in. Reasoning parameter names can identify separate `effort` and `budget` display modes,
but the adapter does not invent effort values, numeric bounds, or effort-to-budget conversions.
Explicit lack of tool support makes an entry unavailable for the coding shell; unknown metadata
remains selectable only after an explicit acknowledgement. All catalog text is bounded and
terminal-sanitized before it reaches a selector.

The primary selector is intentionally compact: human name plus model ID, effective context, tier,
tools, reasoning, and access. The JSON form retains the normalized fields for machine consumers;
it does not expose an unbounded copy of the provider response.

Rainy's authentication, redirect policy, request bounds, SSE parsing, and retry behavior remain
Rainy's/SDK's responsibility. AETHER does not maintain a second SSE parser and calls the SDK's
Responses stream once for each semantic model step; it adds no retry around inference. The adapter
does not invent an idempotency field or provider routing layer. Reasoning and continuation fields
remain opaque to the agent; raw chain-of-thought is never rendered or exposed as a tool result.

## Continuation invalidation and typed recovery

`rainy-sdk 0.6.50` has no continuation-specific error type. AETHER maps to
`BackendError::InvalidContinuation` only with structured protocol evidence:

- locally, a continuation object is present but has no non-empty `previous_response_id` string;
- a Rainy `InvalidRequest` whose machine-readable `code` is `previous_response_id`, `invalid_previous_response_id`, or `INVALID_PREVIOUS_RESPONSE_ID`, and only when that request attached a previous response id.

Other typed SDK variants stay ordinary bounded `BackendError::Operation` values with their retryable
flag preserved; raw SDK messages, secrets, and unbounded provider bodies do not cross the boundary.
The agent never inspects user-facing error text. On `InvalidContinuation` at the first step of a turn
that still has continuation, it clears continuation, reconstructs from bounded local context, and
retries once. Unrelated backend errors are not retried as continuation failures.

Resume also drops continuation when the live workspace root differs or inspected files are stale or missing, so Rainy remote context cannot outrank the filesystem.

## Responses stream completion

The adapter treats `response.completed` as the only successful terminal event. It requires the event
to carry a non-empty response ID and stores that ID as opaque `previous_response_id` continuation
metadata; continuation is never fabricated when transport state disappears. Explicit `response.failed`
and `response.incomplete` events become typed backend errors with bounded safe diagnostic text.

An EOF before a valid terminal event is `IncompleteStream`, including after text deltas or a parseable function-call event. The adapter does not emit `Done` for that stream. Partial function-call argument deltas are discarded, and a completed function-call event followed by EOF still fails the enclosing agent step before any tool execution. If the receiving side is dropped, the adapter observes the bounded output channel's closed state and stops consuming the Rainy stream.

The catalog cache uses a bounded five-minute TTL. Bootstrap refreshes stale entries synchronously on an explicit catalog/model operation; stale-while-refresh background refresh is intentionally not enabled until its task lifetime and cancellation semantics are proven.

The adapter tests exercise both the typed mapping seam and the SDK boundary with a local HTTP stream:
fragmented transport bytes, CRLF/LF, multiple `data:` lines, a split UTF-8 character, unknown
events, `[DONE]`, malformed JSON, oversized frames, and disconnect-after-partial-stream behavior.
They also verify that a failed inference transport is attempted once. Rainy's own parser tests remain
the full parser conformance suite; AETHER only checks the integration contract it consumes.
