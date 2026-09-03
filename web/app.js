import init, { BrowserRuntime } from "./wasm/aether_web.js";

const MAX_PATH_BYTES = 128;
const MAX_QUERY_BYTES = 256;
const MAX_PATCH_BYTES = 2048;
const MAX_RATIONALE_BYTES = 512;
const MAX_SEARCH_RESULTS = 12;

const WEBMCP_TOOLS = [
  {
    name: "inspect_workspace",
    title: "Inspect workspace",
    description: "Inspect the bounded demo repository inventory, active task, and next safe actions.",
    inputSchema: {
      type: "object",
      properties: {},
      additionalProperties: false,
    },
    annotations: { readOnlyHint: true, untrustedContentHint: true },
  },
  {
    name: "read_file",
    title: "Read file",
    description: "Read one bounded text file from the demo workspace with line numbers.",
    inputSchema: {
      type: "object",
      properties: {
        path: { type: "string", minLength: 1, maxLength: MAX_PATH_BYTES },
        startLine: { type: "integer", minimum: 1 },
        endLine: { type: "integer", minimum: 1 },
      },
      required: ["path"],
      additionalProperties: false,
    },
    annotations: { readOnlyHint: true, untrustedContentHint: true },
  },
  {
    name: "search_workspace",
    title: "Search workspace",
    description: "Search bounded workspace text for a literal query and return matching paths and lines.",
    inputSchema: {
      type: "object",
      properties: {
        query: { type: "string", minLength: 1, maxLength: MAX_QUERY_BYTES },
        path: { type: "string", minLength: 1, maxLength: MAX_PATH_BYTES },
        maxResults: { type: "integer", minimum: 1, maximum: MAX_SEARCH_RESULTS },
      },
      required: ["query"],
      additionalProperties: false,
    },
    annotations: { readOnlyHint: true, untrustedContentHint: true },
  },
  {
    name: "propose_patch",
    title: "Propose patch",
    description: "Stage one exact-match bounded file proposal for human review; never apply it silently.",
    inputSchema: {
      type: "object",
      properties: {
        path: { type: "string", minLength: 1, maxLength: MAX_PATH_BYTES },
        find: { type: "string", minLength: 1, maxLength: MAX_PATCH_BYTES },
        replace: { type: "string", minLength: 1, maxLength: MAX_PATCH_BYTES },
        rationale: { type: "string", minLength: 1, maxLength: MAX_RATIONALE_BYTES },
      },
      required: ["path", "find", "replace", "rationale"],
      additionalProperties: false,
    },
    annotations: { readOnlyHint: false, untrustedContentHint: true },
  },
  {
    name: "verify_patch",
    title: "Verify patch",
    description: "Run the bounded browser-safe contract checks against the staged or accepted view.",
    inputSchema: {
      type: "object",
      properties: {
        scope: { type: "string", enum: ["staged", "accepted"] },
      },
      required: ["scope"],
      additionalProperties: false,
    },
    annotations: { readOnlyHint: false, untrustedContentHint: true },
  },
  {
    name: "get_task_state",
    title: "Get task state",
    description: "Return the current shared task, staged diff, evidence ledger, and verification state.",
    inputSchema: {
      type: "object",
      properties: {},
      additionalProperties: false,
    },
    annotations: { readOnlyHint: true, untrustedContentHint: true },
  },
];

let runtime = null;
let currentState = null;
let selectedPath = "src/auth.rs";
let suggestedPatch = null;
let selectedFileContent = "";
const toolTrace = [];

const element = (selector) => document.querySelector(selector);

function byteLength(value) {
  return new TextEncoder().encode(value).length;
}

function isRecord(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function hasOnlyKeys(input, allowed) {
  return Object.keys(input).every((key) => allowed.includes(key));
}

function pathLooksSafe(path) {
  return typeof path === "string"
    && byteLength(path) >= 1
    && byteLength(path) <= MAX_PATH_BYTES
    && !path.startsWith("/")
    && !path.split(/[\\/]/u).includes("..")
    && !path.includes("\0");
}

function stringWithin(value, maxBytes) {
  return typeof value === "string"
    && value.length > 0
    && byteLength(value) <= maxBytes
    && !value.includes("\0");
}

function validateToolInput(name, input) {
  if (!isRecord(input)) {
    return "input must be an object";
  }
  if (name === "inspect_workspace" || name === "get_task_state") {
    return hasOnlyKeys(input, []) ? null : "input has unsupported fields";
  }
  if (name === "read_file") {
    if (!hasOnlyKeys(input, ["path", "startLine", "endLine"]) || !pathLooksSafe(input.path)) {
      return "path must be a safe relative file path";
    }
    if (input.startLine !== undefined
      && (!Number.isInteger(input.startLine) || input.startLine < 1)) {
      return "startLine must be a positive integer";
    }
    if (input.endLine !== undefined
      && (!Number.isInteger(input.endLine) || input.endLine < 1)) {
      return "endLine must be a positive integer";
    }
    return null;
  }
  if (name === "search_workspace") {
    if (!hasOnlyKeys(input, ["query", "path", "maxResults"])
      || !stringWithin(input.query, MAX_QUERY_BYTES)) {
      return "query must be a bounded non-empty string";
    }
    if (input.path !== undefined && !pathLooksSafe(input.path)) {
      return "path must be a safe relative file path";
    }
    if (input.maxResults !== undefined
      && (!Number.isInteger(input.maxResults)
        || input.maxResults < 1
        || input.maxResults > MAX_SEARCH_RESULTS)) {
      return "maxResults is outside the bounded range";
    }
    return null;
  }
  if (name === "propose_patch") {
    if (!hasOnlyKeys(input, ["path", "find", "replace", "rationale"])
      || !pathLooksSafe(input.path)
      || !stringWithin(input.find, MAX_PATCH_BYTES)
      || !stringWithin(input.replace, MAX_PATCH_BYTES)
      || !stringWithin(input.rationale, MAX_RATIONALE_BYTES)) {
      return "patch fields are missing or outside their bounds";
    }
    const lower = (input.find + "\n" + input.replace + "\n" + input.rationale).toLowerCase();
    if (lower.includes("rainy_api_key")
      || lower.includes("api_key=")
      || lower.includes("authorization: bearer")
      || lower.includes("-----begin ")) {
      return "secret-like content is not accepted";
    }
    return null;
  }
  if (name === "verify_patch") {
    return hasOnlyKeys(input, ["scope"])
      && (input.scope === "staged" || input.scope === "accepted")
      ? null
      : "scope must be staged or accepted";
  }
  return "tool is not registered";
}

function recordTrace(name, source, result) {
  toolTrace.unshift({
    name,
    source,
    ok: result?.ok === true,
  });
  if (toolTrace.length > 8) {
    toolTrace.pop();
  }
  renderTrace();
}

function renderTrace() {
  const list = element("#trace-list");
  list.replaceChildren();
  if (toolTrace.length === 0) {
    const empty = document.createElement("li");
    empty.className = "empty-state";
    empty.textContent = "No tool calls yet.";
    list.append(empty);
    return;
  }
  toolTrace.forEach((trace) => {
    const item = document.createElement("li");
    if (!trace.ok) {
      item.className = "failed";
    }
    const text = document.createElement("div");
    const name = document.createElement("strong");
    name.textContent = trace.name;
    const source = document.createElement("span");
    source.textContent = trace.source === "webmcp" ? "browser agent" : "visible UI";
    text.append(name, source);
    const result = document.createElement("em");
    result.textContent = trace.ok ? "ok" : "failed";
    item.append(text, result);
    list.append(item);
  });
}

function setStatus(status, label) {
  const badge = element("#webmcp-status");
  badge.className = "status-pill " + (
    status === "connected" ? "status-teal"
      : status === "error" ? "status-coral" : "status-yellow"
  );
  badge.textContent = "WebMCP: " + label;
}

function showNotice(message) {
  const notice = element("#webmcp-notice");
  notice.hidden = !message;
  notice.textContent = message || "";
}

function renderState(state) {
  currentState = state;
  const task = state.task || {};
  element("#task-title").textContent = task.title || "Untitled task";
  element("#task-objective").textContent = task.objective || "No objective recorded.";
  element("#task-id").textContent = "task / " + (task.id || "unknown");
  const taskStatus = element("#task-status");
  taskStatus.textContent = String(task.status || "ready").replaceAll("_", " ");
  taskStatus.className = "status-pill " + (
    task.status === "verified" ? "status-teal"
      : task.status === "awaiting_human" ? "status-yellow"
        : task.status === "unchanged" ? "status-coral" : "status-neutral"
  );

  const staged = state.stagedPatch;
  const accepted = state.humanDecision === "accepted";
  const flowPropose = element("#flow-propose");
  const flowVerify = element("#flow-verify");
  flowPropose.className = "flow-step " + (staged ? "flow-active" : accepted ? "flow-complete" : "");
  flowVerify.className = "flow-step " + (
    state.verification?.status === "passed" ? "flow-complete" : ""
  );

  const decision = element("#human-decision");
  decision.textContent = staged
    ? "Awaiting your review"
    : state.humanDecision === "accepted" ? "Accepted"
      : state.humanDecision === "rejected" ? "Rejected · unchanged" : "No proposal";
  element("#accept-button").disabled = !staged;
  element("#reject-button").disabled = !staged;
  element("#verify-staged-button").disabled = !staged;
  element("#verify-accepted-button").disabled = !accepted;
  element("#stage-button").disabled = Boolean(staged) || accepted;

  renderFiles(state.files || []);
  renderDiff(staged, state);
  renderVerification(state.verification);
  renderEvidence(state.evidence || []);
}

function renderFiles(files) {
  const tree = element("#file-tree");
  tree.replaceChildren();
  files.forEach((file) => {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "file-button" + (file.path === selectedPath ? " selected" : "");
    button.addEventListener("click", () => {
      selectedPath = file.path;
      renderFiles(currentState?.files || []);
      runTool("read_file", { path: selectedPath }, "human");
    });
    const icon = document.createElement("span");
    icon.className = "file-icon";
    icon.textContent = file.path.endsWith(".rs") ? "R" : file.path.endsWith(".toml") ? "T" : "·";
    const name = document.createElement("span");
    name.textContent = file.path;
    button.append(icon, name);
    if (file.changed) {
      const mark = document.createElement("span");
      mark.className = "changed-mark";
      mark.title = "Changed in shared state";
      button.append(mark);
    }
    tree.append(button);
  });
}

function renderDiff(staged, state) {
  const heading = element("#diff-heading");
  const diffState = element("#diff-state");
  const meta = element("#diff-meta");
  const view = element("#diff-view");
  if (staged) {
    heading.textContent = staged.path;
    diffState.textContent = "Staged";
    diffState.className = "status-pill status-yellow";
    meta.textContent = staged.linesAdded + " additions · " + staged.linesRemoved
      + " removals · exact precondition · human review required";
    view.className = "diff-view has-diff";
    view.textContent = staged.diff || "No diff content.";
    return;
  }
  if (state.humanDecision === "accepted") {
    heading.textContent = "Accepted change";
    diffState.textContent = "Applied";
    diffState.className = "status-pill status-teal";
    meta.textContent = "The accepted patch is now part of the shared repository view.";
    view.className = "diff-view has-diff";
    view.textContent = "Patch accepted. Read the changed file to inspect the current source.";
    return;
  }
  heading.textContent = "No staged change";
  diffState.textContent = "Waiting";
  diffState.className = "status-pill status-neutral";
  meta.textContent = "Agent proposals stay staged until you choose Accept.";
  view.className = "diff-view is-empty";
  view.textContent = "Use “Stage suggested fix” or ask the browser agent to call propose_patch.";
}

function renderVerification(report) {
  const verification = report || { status: "not_run", summary: "No verification has run for the current view.", checks: [] };
  const dot = element("#verification-dot");
  dot.className = "status-dot " + (
    verification.status === "passed" ? "dot-passed"
      : verification.status === "failed" ? "dot-failed" : "dot-neutral"
  );
  element("#verification-summary").textContent = verification.summary;
  const list = element("#check-list");
  list.replaceChildren();
  (verification.checks || []).forEach((check) => {
    const row = document.createElement("div");
    row.className = "check-row " + (check.passed ? "passed" : "failed");
    const mark = document.createElement("span");
    mark.className = "check-mark";
    mark.textContent = check.passed ? "✓" : "!";
    const copy = document.createElement("div");
    const title = document.createElement("strong");
    title.textContent = check.name;
    const detail = document.createElement("span");
    detail.textContent = check.detail;
    copy.append(title, detail);
    row.append(mark, copy);
    list.append(row);
  });
}

function renderEvidence(evidence) {
  element("#evidence-count").textContent = String(evidence.length);
  const list = element("#evidence-list");
  list.replaceChildren();
  if (evidence.length === 0) {
    const empty = document.createElement("li");
    empty.className = "empty-state";
    empty.textContent = "Evidence will appear as the workspace is inspected.";
    list.append(empty);
    return;
  }
  evidence.slice(0, 12).forEach((item) => {
    const row = document.createElement("li");
    const title = document.createElement("strong");
    title.textContent = item.kind + (item.path ? " · " + item.path : "");
    const detail = document.createElement("span");
    detail.textContent = (item.valid ? "" : "stale · ") + item.detail;
    row.append(title, detail);
    list.append(row);
  });
}

function renderFileResult(result) {
  if (!result?.ok || typeof result.content !== "string") {
    return;
  }
  selectedFileContent = result.content;
  element("#file-heading").textContent = result.path;
  element("#file-meta").textContent = "lines " + result.startLine + "–" + result.endLine;
  element("#file-view").textContent = selectedFileContent;
}

function renderSearchResults(matches) {
  const container = element("#search-results");
  container.replaceChildren();
  container.hidden = matches.length === 0;
  matches.slice(0, MAX_SEARCH_RESULTS).forEach((match) => {
    const button = document.createElement("button");
    button.className = "search-result";
    button.type = "button";
    button.addEventListener("click", () => {
      selectedPath = match.path;
      renderFiles(currentState?.files || []);
      runTool("read_file", { path: match.path, startLine: match.line }, "human");
    });
    const path = document.createElement("strong");
    path.textContent = match.path + ":" + match.line;
    const preview = document.createElement("span");
    preview.textContent = match.preview;
    button.append(path, preview);
    container.append(button);
  });
}

function applyResult(result, name, source) {
  recordTrace(name, source, result);
  if (result?.state) {
    renderState(result.state);
  }
  if (name === "inspect_workspace" && result?.suggestedPatch) {
    suggestedPatch = result.suggestedPatch;
  }
  if (name === "read_file") {
    renderFileResult(result);
  }
  if (name === "search_workspace" && Array.isArray(result?.matches)) {
    renderSearchResults(result.matches);
  }
  return result;
}

function runRaw(operation, input, source) {
  if (!runtime) {
    return { ok: false, error: { code: "runtime_unavailable", message: "browser runtime is still loading" } };
  }
  let result;
  try {
    result = JSON.parse(runtime.invoke(JSON.stringify({ op: operation, ...input })));
  } catch (error) {
    result = { ok: false, error: { code: "runtime_error", message: String(error) } };
  }
  return applyResult(result, operation, source);
}

function runTool(name, input, source) {
  const validationError = validateToolInput(name, input);
  if (validationError) {
    const result = {
      ok: false,
      tool: name,
      error: { code: "invalid_input", message: validationError },
    };
    recordTrace(name, source, result);
    return result;
  }
  return runRaw(name, input, source);
}

async function executeWebMcpTool(name, input, signal) {
  if (signal?.aborted) {
    return { ok: false, tool: name, error: { code: "aborted", message: "tool call was cancelled" } };
  }
  const result = runTool(name, input, "webmcp");
  if (signal?.aborted) {
    return { ok: false, tool: name, error: { code: "aborted", message: "tool call was cancelled" } };
  }
  return result;
}

async function registerWebMcp() {
  if (!("modelContext" in document)
    || !document.modelContext
    || typeof document.modelContext.registerTool !== "function") {
    setStatus("warning", "unavailable");
    showNotice("WebMCP is not available in this browser. The workspace remains usable through its visible controls.");
    return;
  }

  const controller = new AbortController();
  let registered = 0;
  for (const tool of WEBMCP_TOOLS) {
    try {
      await document.modelContext.registerTool({
        name: tool.name,
        title: tool.title,
        description: tool.description,
        inputSchema: tool.inputSchema,
        annotations: tool.annotations,
        execute: (input, options) => executeWebMcpTool(tool.name, input, options?.signal),
      }, { signal: controller.signal });
      registered += 1;
    } catch (error) {
      console.warn("WebMCP registration failed for " + tool.name, error);
    }
  }
  if (registered === WEBMCP_TOOLS.length) {
    setStatus("connected", registered + " tools");
    showNotice("");
  } else if (registered > 0) {
    setStatus("warning", registered + " of " + WEBMCP_TOOLS.length);
    showNotice("WebMCP is partially available: " + registered + " of " + WEBMCP_TOOLS.length + " tools registered.");
  } else {
    setStatus("error", "blocked");
    showNotice("WebMCP is present but tool registration was blocked by the browser. The visible workspace remains usable.");
  }
}

function bindUi() {
  element("#inspect-button").addEventListener("click", () => runTool("inspect_workspace", {}, "human"));
  element("#stage-button").addEventListener("click", () => {
    if (!suggestedPatch) {
      const inspected = runTool("inspect_workspace", {}, "human");
      suggestedPatch = inspected.suggestedPatch;
    }
    if (suggestedPatch) {
      runTool("propose_patch", suggestedPatch, "human");
    }
  });
  element("#verify-staged-button").addEventListener("click", () => {
    runTool("verify_patch", { scope: "staged" }, "human");
  });
  element("#verify-accepted-button").addEventListener("click", () => {
    runTool("verify_patch", { scope: "accepted" }, "human");
  });
  element("#accept-button").addEventListener("click", () => {
    const result = runRaw("accept_patch", {}, "human");
    if (result.ok) {
      selectedPath = result.state?.files?.find((file) => file.changed)?.path || selectedPath;
      runTool("read_file", { path: selectedPath }, "system");
    }
  });
  element("#reject-button").addEventListener("click", () => {
    runRaw("reject_patch", {}, "human");
  });
  element("#search-form").addEventListener("submit", (event) => {
    event.preventDefault();
    const query = element("#search-input").value;
    if (query.trim()) {
      runTool("search_workspace", { query: query.trim(), maxResults: 8 }, "human");
    }
  });
}

async function boot() {
  try {
    await init();
    runtime = new BrowserRuntime();
    bindUi();
    const inspected = runTool("inspect_workspace", {}, "system");
    suggestedPatch = inspected.suggestedPatch || null;
    runTool("read_file", { path: selectedPath }, "system");
    await registerWebMcp();
  } catch (error) {
    setStatus("error", "runtime error");
    showNotice("The browser runtime could not load. Refresh to retry; no repository data was changed.");
    element("#file-view").textContent = String(error);
  }
}

boot();
