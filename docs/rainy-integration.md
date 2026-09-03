# Rainy integration

The only provider boundary is `aether-rainy`. It depends on the exact `rainy-sdk 0.6.51` crate and uses its `RainyClient`, `ResponsesRequest`, `OpenAIChatCompletionRequest`, both typed streaming APIs, and `get_models_catalog`. The adapter consumes the SDK's typed public Responses and OpenAI-compatible Chat Completions streams and maps them into AETHER `ModelEvent` values without leaking SDK types.

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
the SDK's authoritative fields and adds the explicit wire protocol selected for the model:

- identity and display name;
- the selected Responses or Chat Completions transport;
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
Only an entry with tool support explicitly reported as supported is selectable for the coding shell;
unsupported or unknown tool metadata remains visible but unavailable. Unknown access, privacy, and
other metadata may still require an explicit acknowledgement. All catalog text is bounded and
terminal-sanitized before it reaches a selector.

The primary selector is a two-line interaction: the human name is always shown first, with the model
ID and capability summary directly below it. The current model is called out, filtering searches
names and IDs, and visible entries can be chosen with number keys as well as the arrow or `j`/`k`
controls. This keeps the identity readable even on a narrow terminal. The JSON form retains the
normalized fields for machine consumers; it does not expose an unbounded copy of the provider
response.

Rainy's authentication, redirect policy, request bounds, SSE parsing, and retry behavior remain
Rainy's/SDK's responsibility. AETHER does not maintain a second SSE parser and calls one SDK stream
once for each semantic model step; it adds no retry around inference. Reasoning fields remain opaque
to the agent; raw chain-of-thought is never rendered or exposed as a tool result.

## Protocol routing and Chat Completions history

The catalog in `rainy-sdk 0.6.51` reports model capabilities but does not report a per-model wire
protocol. AETHER therefore applies one centralized routing rule: a provider-qualified `openai/*`
model, or a recognized unqualified OpenAI model ID such as `gpt-*` or `o1`/`o3`/`o4`, uses
Responses; every other model ID uses the OpenAI-compatible Chat Completions endpoint. The chosen
transport is displayed beside the model name and ID in the selector, status output, and successful
model-switch message. Request failures include the selected model and transport so a denied tool
capability is diagnosable without guessing which endpoint ran.

The public catalog cannot prove account-level tool entitlement. If inference returns the structured
`TOOLS_NOT_ALLOWED` denial, AETHER treats it as a non-retryable model capability failure, remembers
the model as unavailable for tools for the lifetime of the backend, and shows it as unavailable on
the next model selection. The interactive shell remains open so another model can be selected with
`/model`; AETHER does not retry the denied request or silently send a coding turn without tools.

Chat Completions receives the agent's bounded, protocol-neutral conversation history. That history
contains user/developer messages, the assistant's assembled text and tool calls, and each bounded
tool result. Tool schemas are translated to the OpenAI function wrapper, fragmented streaming tool
call deltas are assembled by index, usage is forwarded when reported, and a terminal finish chunk is
required before AETHER emits `Done`. Chat Completions has no Responses continuation ID; it returns
`Done { continuation: None }` and relies on the next bounded conversation step. A stale Responses
continuation is rejected once and the agent reconstructs the turn from local bounded context.

## Continuation invalidation and typed recovery

`rainy-sdk 0.6.51` has no continuation-specific error type. AETHER maps to
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
events, `[DONE]`, incomplete final frames, malformed JSON, oversized frames, and
disconnect-after-partial-stream behavior.
They also verify that a failed inference transport is attempted once. Rainy's own parser tests remain
the full parser conformance suite; AETHER only checks the integration contract it consumes.
