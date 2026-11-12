import type { SessionEvent } from "../remote/protocol.js";
import { type AuthState, BrowserAuth, type EntryOperation } from "./auth.js";
import { type Agent, demoAgents, demoFrames, demoPermissions, type PermissionQuestion, statuses } from "./demo.js";

const DEMO_INTERVAL_MS = 2800;
const DEMO_NOTICE = "Preview data only. No live agents or commands.";
const POLL_INTERVAL_MS = 2000;
const PERMISSION_FEEDBACK_MS = 3000;
const REQUEST_TIMEOUT_MS = 35_000;
const MAX_MESSAGE_BYTES = 12 * 1024;
const MAX_LIVE_TEXT = 12 * 1024;
const MAX_LIVE_TOOLS = 16;
const MAX_TOOL_NAME = 128;
const CONVERSATION_ROLES = { user: "You", assistant: "Phi", tool: "Tool", error: "Error" } as const;
const isDemo = document.body.dataset.mode === "demo";
const connectionParams = new URLSearchParams(location.hash.slice(1));
if (!isDemo) {
  const address = new URL(location.href);
  const fragment = new URLSearchParams(address.hash.slice(1));
  let changed = false;
  for (const params of [address.searchParams, fragment]) {
    for (const secret of ["token", "password"]) {
      if (params.has(secret)) {
        params.delete(secret);
        changed = true;
      }
    }
  }
  if (changed) {
    address.hash = fragment.toString();
    history.replaceState(null, "", `${address.pathname}${address.search}${address.hash}`);
  }
}
let sessionId = connectionParams.get("session");
const agents = isDemo ? structuredClone(demoAgents) : [];
const main = document.querySelector("main")!;
const desks = document.querySelector<HTMLElement>("#desks")!;
const demoToggle = document.querySelector<HTMLButtonElement>("#demo-toggle")!;
const connection = document.querySelector<HTMLElement>(".connection")!;
const notice = document.querySelector<HTMLElement>("#view-notice")!;
const detailToggle = document.querySelector<HTMLButtonElement>("#detail-toggle")!;
const detail = document.querySelector<HTMLElement>("#agent-detail")!;
const message = document.querySelector<HTMLTextAreaElement>("#message")!;
const messageStatus = document.querySelector<HTMLElement>("#message-status")!;
let selectedId: string | undefined = agents[0]?.id;
let connected = false;
let sending = false;
let demoTimer: ReturnType<typeof setInterval> | undefined;
let pollTimer: ReturnType<typeof setTimeout> | undefined;
let pageActive = true;
let polling = false;
let frameIndex = 0;
let deskKey: string | undefined;
let rootId: string | undefined;
let conversation: Conversation | undefined;
let liveText: string | undefined;
const liveTools = new Map<string, { name: string; status: string }>();
let refreshPending = false;
let permissions: PermissionQuestion[] = isDemo ? structuredClone(demoPermissions) : [];
type PermissionDecision = "allow_once" | "allow_always" | "deny";
const answering = new Set<string>();
const answered = new Set<string>();
const permissionFeedback = new Map<string, { decision: PermissionDecision; text: string }>();
const CHAT_SCROLL_MARGIN = 32;
let activityKey: string | undefined;
const AUTH_LABELS: Record<AuthState["status"], string> = {
  signedOut: "Pair device",
  connecting: "Verifying…",
  pending: "Awaiting approval",
  ready: "Paired",
  error: "Verification failed",
};
const authPanel = document.querySelector<HTMLDetailsElement>("#device-auth")!;
const authForm = document.querySelector<HTMLFormElement>("#auth-form")!;
const pairingToken = document.querySelector<HTMLInputElement>("#pairing-token")!;
let authGeneration = 0;
const auth = new BrowserAuth((state) => {
  authGeneration += 1;
  connected = false;
  liveText = undefined;
  liveTools.clear();
  clearTimeout(pollTimer);
  document.body.dataset.connection = state.status === "ready" ? "connecting" : "disconnected";
  connection.textContent = state.status === "pending" ? "PAIRING PENDING" : "NOT CONNECTED";
  document.querySelector("#auth-status")!.textContent = state.message;
  document.querySelector("#auth-label")!.textContent = AUTH_LABELS[state.status];
  authPanel.dataset.state = state.status;
  if (state.status === "ready") authPanel.open = false;
  const busy = state.status === "connecting" || state.status === "pending";
  authForm.hidden = state.status === "ready";
  pairingToken.disabled = busy;
  document.querySelector<HTMLButtonElement>("#auth-connect")!.disabled = busy;
  document.querySelector<HTMLButtonElement>("#auth-recover")!.disabled = busy;
  document.querySelector<HTMLButtonElement>("#auth-cancel")!.hidden = !busy;
  document.querySelector<HTMLButtonElement>("#auth-reconnect")!.hidden = busy || state.status === "ready";
  document.querySelector<HTMLButtonElement>("#auth-disconnect")!.hidden = state.status !== "ready";
  document.querySelector<HTMLButtonElement>("#auth-forget")!.hidden = busy;
  render();
  if (state.status === "ready" && pageActive) void poll();
}, observe);

function requestRefresh() {
  if (polling) refreshPending = true;
  else void poll();
}

function observe({ sessionId: observedSession, event }: SessionEvent) {
  if (!pageActive || observedSession !== sessionId) return;
  if (event.kind === "boundary") return;
  if (event.kind === "refresh" || (event.kind === "progress" && event.status !== "running")) {
    requestRefresh();
    return;
  }
  if (event.kind !== "phi") return;
  const display = event.event;
  if (["assistant_finished", "turn_started", "turn_finished", "turn_failed"].includes(display.type)) {
    liveText = undefined;
    liveTools.clear();
    requestRefresh();
    return;
  }
  // OfficeView has no atomic boundary; never merge overlapping reads and deltas.
  if (polling) return;
  if (display.type === "assistant_started") liveText = "";
  else if (display.type === "assistant_delta" && typeof display.display === "string")
    liveText = `${liveText ?? ""}${display.display.slice(-MAX_LIVE_TEXT)}`.slice(-MAX_LIVE_TEXT);
  else if (display.type === "tool_call_started" && typeof display.id === "string" && typeof display.name === "string") {
    if (liveTools.size >= MAX_LIVE_TOOLS) liveTools.delete(liveTools.keys().next().value!);
    liveTools.set(display.id, { name: display.name.slice(0, MAX_TOOL_NAME), status: "running" });
  } else if (display.type === "tool_call_finished" && typeof display.id === "string") {
    const tool = liveTools.get(display.id);
    if (tool) tool.status = display.is_error ? "failed" : "completed";
  }
  renderDetail();
}
const authenticate = (operation: EntryOperation) => {
  const credential = pairingToken.value;
  pairingToken.value = "";
  void auth.signIn(credential, operation);
};
authForm.addEventListener("submit", (event) => {
  event.preventDefault();
  authenticate("enroll");
});
document.querySelector("#auth-recover")!.addEventListener("click", () => authenticate("recover"));
document.querySelector("#auth-reconnect")!.addEventListener("click", () => void auth.resume());
document.querySelector("#auth-cancel")!.addEventListener("click", () => auth.close());
document.querySelector("#auth-disconnect")!.addEventListener("click", () => auth.close());
document.querySelector("#auth-forget")!.addEventListener("click", () => auth.forget());
window.addEventListener("click", (event) => {
  if (event.target instanceof Node && !authPanel.contains(event.target)) authPanel.open = false;
});
window.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && authPanel.open) {
    authPanel.open = false;
    authPanel.querySelector("summary")!.focus();
  }
});

detailToggle.addEventListener("click", () => {
  const collapsed = main.classList.toggle("details-collapsed");
  detailToggle.setAttribute("aria-expanded", String(!collapsed));
  const label = collapsed ? "Show details" : "Hide details";
  detailToggle.setAttribute("aria-label", label);
  detailToggle.title = label;
  detail.inert = collapsed;
  detail.setAttribute("aria-hidden", String(collapsed));
});

function element<Tag extends keyof HTMLElementTagNameMap>(tag: Tag, className: string, text?: string) {
  const node = document.createElement(tag);
  node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function renderDesks() {
  const key = JSON.stringify({
    agents: agents.map(({ id, name, role, status }) => ({ id, name, role, status, selected: id === selectedId })),
    permissions,
    connected,
    feedback: [...permissionFeedback],
    answering: [...answering],
  });
  if (deskKey === key) return;
  deskKey = key;
  const focused = (document.activeElement as HTMLElement | null)?.dataset.agentId;
  const opened = new Set(
    [...desks.querySelectorAll<HTMLDetailsElement>("details[open]")].map((node) => node.dataset.requestId),
  );
  const buttons = agents.map((agent, index) => {
    const button = element("button", "desk");
    button.type = "button";
    button.dataset.agentId = agent.id;
    button.dataset.status = agent.status;
    button.setAttribute("aria-pressed", String(agent.id === selectedId));
    button.setAttribute("aria-label", `${agent.name}: ${statuses[agent.status]}`);
    const scene = element("span", "desk-scene");
    const furniture = element("img", "furniture");
    furniture.src = "/office.svg#desk";
    furniture.alt = "";
    const person = element("img", "person");
    person.src = `/office.svg#person-${index % demoAgents.length}`;
    person.alt = "";
    scene.append(furniture, person);
    button.append(
      element("span", "desk-bubble", statuses[agent.status]),
      scene,
      element("span", "desk-name", agent.name),
      element("span", "desk-role", agent.role),
    );
    return button;
  });
  desks.replaceChildren(
    ...buttons.map((button) => {
      const slot = element("div", "desk-slot");
      slot.append(button);
      const questions = permissions.filter(
        (question) => (question.anchorId ?? question.agentId) === button.dataset.agentId,
      );
      const feedback = permissionFeedback.get(button.dataset.agentId!);
      if (questions.length) {
        const question = questions[0];
        const bubble = element("details", "permission-bubble");
        bubble.dataset.requestId = question.requestId;
        bubble.open = opened.has(question.requestId);
        bubble.append(
          element(
            "summary",
            "",
            `${question.tool} needs permission${questions.length > 1 ? ` (${questions.length})` : ""}`,
          ),
        );
        const content = element("div", "permission-content");
        if (feedback) {
          const result = element("p", "permission-feedback", feedback.text);
          result.dataset.decision = feedback.decision;
          content.append(result);
        }
        content.append(element("pre", "", question.input));
        if (question.preview) content.append(element("pre", "", question.preview));
        if (question.canAllow === false)
          content.append(
            element(
              "p",
              "",
              "Approval unavailable: details exceed the display limit or this interaction is unsupported.",
            ),
          );
        for (const [decision, label] of [
          ["allow_once", "Allow once"],
          ["allow_always", "Always"],
          ["deny", "Deny"],
        ] as const) {
          const answer = element("button", "", label);
          answer.type = "button";
          answer.dataset.requestId = question.requestId;
          answer.dataset.decision = decision;
          answer.disabled =
            (!isDemo && !connected) ||
            answering.has(question.requestId) ||
            (decision !== "deny" && question.canAllow === false);
          content.append(answer);
        }
        bubble.append(content);
        slot.append(bubble);
      } else if (feedback) {
        const result = element("div", "permission-bubble permission-feedback", feedback.text);
        result.dataset.decision = feedback.decision;
        result.setAttribute("role", "status");
        slot.append(result);
      }
      return slot;
    }),
  );
  if (focused) buttons.find((button) => button.dataset.agentId === focused)?.focus();
  const working = agents.filter((agent) => agent.status === "working").length;
  document.querySelector("#office-summary")!.textContent = `${agents.length} agents · ${working} working`;
}

desks.addEventListener("click", (event) => {
  const answer = (event.target as Element).closest<HTMLButtonElement>("button[data-decision]");
  if (answer) {
    const question = permissions.find((question) => question.requestId === answer.dataset.requestId);
    if (question) void respondPermission(question, answer.dataset.decision as PermissionDecision);
    return;
  }
  const button = (event.target as Element).closest<HTMLButtonElement>("button[data-agent-id]");
  if (!button) return;
  selectedId = button.dataset.agentId;
  render();
});

function selectedAgent() {
  return agents.find((agent) => agent.id === selectedId);
}

function updateComposer() {
  const agent = selectedAgent();
  const enabled = !isDemo && connected && (agents.length === 0 || agent?.canMessage);
  message.disabled = !enabled || sending;
  if (!enabled) message.placeholder = "Messaging unavailable";
  else message.placeholder = "Enter: send · Alt+Enter: newline";
}

function renderDetail() {
  const agent = selectedAgent();
  detail.dataset.empty = String(!agent);
  if (!agent) {
    document.querySelector("#agent-name")!.textContent = "";
    document.querySelector("#agent-role")!.textContent = "";
    document.querySelector("#agent-model")!.textContent = "";
    document.querySelector("#agent-status")!.textContent = "";
    document.querySelector("#activity")!.replaceChildren();
    updateComposer();
    return;
  }
  const fields = {
    "agent-name": agent.name,
    "agent-role": agent.role,
    "agent-model": agent.model,
    "agent-status": statuses[agent.status],
  };
  for (const [id, text] of Object.entries(fields)) document.getElementById(id)!.textContent = text;
  document.querySelector<HTMLElement>("#agent-status")!.dataset.status = agent.status;
  document.querySelector<HTMLImageElement>("#portrait")!.src =
    `/office.svg#portrait-${agents.indexOf(agent) % demoAgents.length}`;
  const chatting = agent.id === rootId && conversation !== undefined;
  document.querySelector("#activity-title")!.textContent = chatting ? "CONVERSATION" : "RECENT ACTIVITY";
  const entries = chatting
    ? [
        ...(conversation!.truncated
          ? [{ time: "Notice", text: "Only recent conversation text is shown; earlier or long content is omitted." }]
          : []),
        ...conversation!.messages.map((entry) => ({ time: CONVERSATION_ROLES[entry.role], text: entry.text })),
        ...((liveText ?? conversation!.liveText)
          ? [{ time: "Phi · live", text: liveText ?? conversation!.liveText }]
          : []),
        ...[...liveTools.values()].map((tool) => ({ time: "Tool · live", text: `${tool.name} · ${tool.status}` })),
      ]
    : agent.activity;
  const key = JSON.stringify({ selectedId, entries });
  if (activityKey === key) {
    updateComposer();
    return;
  }
  activityKey = key;
  const activity = document.querySelector(".activity-section")!;
  const atBottom = activity.scrollHeight - activity.clientHeight - activity.scrollTop <= CHAT_SCROLL_MARGIN;
  document.querySelector("#activity")!.replaceChildren(
    ...entries.map((entry) => {
      const item = element("li", "activity-item");
      const timestamp = Date.parse(entry.time);
      const time = Number.isNaN(timestamp) ? entry.time : new Date(timestamp).toLocaleTimeString();
      item.append(element("time", "", time), element("p", "", entry.text.replace(/^(?:[ \t]*\r?\n)+/, "")));
      return item;
    }),
  );
  if (chatting && atBottom) activity.scrollTop = activity.scrollHeight;
  updateComposer();
}

function render() {
  const terminalHint = document.querySelector<HTMLElement>("#terminal-hint")!;
  const hostedSession = !isDemo && sessionId && /^[a-zA-Z0-9_-]+$/.test(sessionId) ? sessionId : null;
  terminalHint.hidden = !hostedSession;
  document.querySelector("#terminal-command")!.textContent = hostedSession
    ? `phi --gateway --session ${hostedSession}`
    : "";
  renderDesks();
  renderDetail();
}

interface Receipt {
  notices?: string[];
  kind: string;
  status: string;
  delivery?: { state: string; reason?: string };
}

interface Conversation {
  messages: { id: string; role: keyof typeof CONVERSATION_ROLES; text: string }[];
  liveText: string;
  truncated: boolean;
}

interface View {
  agents: Agent[];
  conversation?: Conversation;
  permissions?: PermissionQuestion[];
  workspacePath?: string;
  sessionName?: string;
  sessionId?: string;
}

function validateReceipt(receipt: Receipt) {
  if (receipt.kind === "submission" && ["started", "queued", "handled"].includes(receipt.status)) return;
  if (receipt.kind === "agent_message") {
    const { state, reason } = receipt.delivery ?? {};
    if (["queued", "admitted", "committed"].includes(state!)) return;
    if (state === "rejected") throw new Error(reason || "Recipient rejected the message");
  }
  throw new Error("Unrecognized message receipt; check Phi before resending");
}

async function api<Result>(path: string, options: RequestInit = {}): Promise<Result> {
  if (path !== "/api/sessions") {
    if (!sessionId) {
      const sessions = await api<{ id: string }[]>("/api/sessions");
      sessionId = sessions[0]?.id ?? null;
    }
    if (!sessionId) throw new Error("No gateway Session is available");
    path = `/api/sessions/${encodeURIComponent(sessionId)}/${path.slice("/api/".length)}`;
  }
  const credential = auth.credential;
  const generation = authGeneration;
  if (!credential) throw new Error("Device authentication required");
  const response = await fetch(path, {
    ...options,
    headers: { Authorization: `Bearer ${credential}`, ...options.headers },
    signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
    cache: "no-store",
  });
  const value = await response.json();
  if (generation !== authGeneration)
    throw new Error("Device access changed; discard this response and check current state");
  if (response.status === 401 || response.status === 403) auth.close();
  if (!response.ok) throw new Error(value.error || `Request failed (${response.status})`);
  return value;
}

function showPermissionFeedback(agentId: string, decision: PermissionDecision, text: string) {
  const feedback = { decision, text };
  permissionFeedback.set(agentId, feedback);
  setTimeout(() => {
    if (permissionFeedback.get(agentId) !== feedback) return;
    permissionFeedback.delete(agentId);
    renderDesks();
  }, PERMISSION_FEEDBACK_MS);
}

async function respondPermission(question: PermissionQuestion, decision: PermissionDecision) {
  if ((!isDemo && !connected) || answering.has(question.requestId)) return;
  const feedback = {
    allow_once: `${question.tool} allowed once`,
    allow_always: `${question.tool} always allowed`,
    deny: `${question.tool} denied`,
  }[decision];
  if (isDemo) {
    const agent = agents.find((agent) => agent.id === question.agentId)!;
    agent.status = decision !== "deny" ? "working" : "idle";
    const activity =
      decision !== "deny"
        ? "Demo permission allowed once. Simulated reference access resumed."
        : "Demo permission denied. Simulated reference access skipped.";
    agent.activity.push({ time: "Demo", text: activity });
    permissions = permissions.filter((entry) => entry.requestId !== question.requestId);
    showPermissionFeedback(question.anchorId ?? question.agentId, decision, `Demo: ${feedback}`);
    selectedId = agent.id;
    notice.textContent = `${agent.name}: ${activity} No command was executed.`;
    render();
    return;
  }
  answering.add(question.requestId);
  renderDesks();
  try {
    const receipt = await api<{ status: string; persistenceWarning?: string }>("/api/permission", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ agentId: question.agentId, requestId: question.requestId, decision }),
    });
    if (receipt.status !== "answered") throw new Error("Unrecognized permission receipt");
    answered.add(question.requestId);
    showPermissionFeedback(question.anchorId ?? question.agentId, decision, feedback);
    permissions = permissions.filter((entry) => entry.requestId !== question.requestId);
    messageStatus.textContent = receipt.persistenceWarning
      ? `permission allowed, but failed to persist allowlist: ${receipt.persistenceWarning}`
      : "";
  } catch (error) {
    messageStatus.textContent = `${(error as Error).message}. Check the current permission state before retrying.`;
  } finally {
    answering.delete(question.requestId);
    renderDesks();
    void poll();
  }
}

async function poll() {
  if (polling || !pageActive || !auth.credential) return;
  clearTimeout(pollTimer);
  pollTimer = undefined;
  polling = true;
  refreshPending = false;
  liveText = undefined;
  liveTools.clear();
  try {
    if (!sessionId) {
      const sessions = await api<{ id: string }[]>("/api/sessions");
      sessionId = sessions[0]?.id ?? null;
    }
    if (!sessionId) throw new Error("No gateway Session is available");
    await auth.watch(sessionId);
    const view = await api<View>("/api/view");
    if (!Array.isArray(view.agents)) throw new Error("No agent observation returned");
    for (const agent of view.agents) {
      if (!Object.hasOwn(statuses, agent.status) || !Array.isArray(agent.activity))
        throw new Error("Unsupported agent observation");
    }
    document.querySelector("#workspace-path")!.textContent = view.workspacePath ?? "Unavailable";
    document.querySelector("#session-name")!.textContent = view.sessionName?.trim() || "Untitled session";
    document.querySelector("#session-id")!.textContent = view.sessionId ?? "Unavailable";
    agents.splice(0, agents.length, ...view.agents);
    rootId = view.sessionId || undefined;
    conversation = view.conversation;
    permissions = (view.permissions ?? []).filter((question) => !answered.has(question.requestId));
    if (!agents.some((agent) => agent.id === selectedId)) selectedId = agents[0]?.id;
    connected = true;
    document.body.dataset.connection = "live";
    connection.textContent = "CONNECTED · PHI";
    notice.textContent = "";
    render();
  } catch (error) {
    connected = false;
    document.body.dataset.connection = "disconnected";
    connection.textContent = "DISCONNECTED · STALE VIEW";
    notice.textContent = (error as Error).message;
    renderDesks();
    updateComposer();
  } finally {
    polling = false;
    if (pageActive && auth.credential) pollTimer = setTimeout(poll, refreshPending ? 0 : POLL_INTERVAL_MS);
  }
}

function stopDemo() {
  clearInterval(demoTimer);
  demoTimer = undefined;
  demoToggle.textContent = "Replay demo";
  demoToggle.setAttribute("aria-pressed", "false");
}

demoToggle.addEventListener("click", () => {
  if (!isDemo) return;
  if (demoTimer !== undefined) {
    stopDemo();
    return;
  }
  agents.splice(0, agents.length, ...structuredClone(demoAgents));
  permissions = structuredClone(demoPermissions);
  permissionFeedback.clear();
  notice.textContent = DEMO_NOTICE;
  frameIndex = 0;
  render();
  demoToggle.textContent = "Pause demo";
  demoToggle.setAttribute("aria-pressed", "true");
  demoTimer = setInterval(() => {
    const frame = demoFrames[frameIndex++];
    const agent = agents.find((candidate) => candidate.id === frame.id)!;
    agent.status = frame.status;
    agent.activity.push({ time: "Demo", text: frame.text });
    render();
    if (frameIndex === demoFrames.length) stopDemo();
  }, DEMO_INTERVAL_MS);
});

message.addEventListener("keydown", (event) => {
  if (event.key !== "Enter" || event.shiftKey || event.isComposing) return;
  event.preventDefault();
  if (event.altKey) {
    message.setRangeText("\n", message.selectionStart, message.selectionEnd, "end");
    message.dispatchEvent(new Event("input", { bubbles: true }));
    return;
  }
  message.form!.requestSubmit();
});

document.querySelector("#message-form")!.addEventListener("submit", async (event) => {
  event.preventDefault();
  const target = selectedAgent();
  if (isDemo || !connected || sending || (agents.length > 0 && !target?.canMessage)) return;
  const draft = message.value;
  if (!draft.trim() || new TextEncoder().encode(draft).length > MAX_MESSAGE_BYTES) {
    messageStatus.textContent = "Enter a message up to 12 KiB.";
    return;
  }
  sending = true;
  updateComposer();
  messageStatus.textContent = "";
  try {
    const receipt = await api<Receipt>("/api/message", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ agentId: target?.id, message: draft }),
    });
    validateReceipt(receipt);
    messageStatus.textContent = receipt.notices?.join("\n") ?? "";
    message.value = "";
    void poll();
  } catch (error) {
    messageStatus.textContent = `${(error as Error).message}. Check Phi before resending.`;
  } finally {
    sending = false;
    updateComposer();
  }
});

window.addEventListener("pagehide", () => {
  pageActive = false;
  stopDemo();
  clearTimeout(pollTimer);
  if (!isDemo) auth.close();
});
window.addEventListener("pageshow", (event) => {
  pageActive = true;
  if (event.persisted && !isDemo) void auth.resume();
});
render();
authPanel.hidden = isDemo;
if (isDemo) {
  document.querySelector("#session-name")!.textContent = "Demo";
  document.querySelector("#session-id")!.textContent = "not connected";
  connection.textContent = "DEMO · NOT CONNECTED";
  notice.textContent = DEMO_NOTICE;
  messageStatus.textContent = "Unavailable in demo";
} else {
  demoToggle.hidden = true;
  messageStatus.textContent = "";
  void auth.resume();
}
