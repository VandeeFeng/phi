export const statuses = Object.freeze({
  working: "Working",
  waiting: "Waiting on agent",
  blocked: "Needs you",
  idle: "Idle",
  error: "Error",
  stopped: "Stopped",
  stopping: "Stopping",
  unavailable: "Unavailable",
});

export interface Agent {
  id: string;
  name: string;
  role: string;
  model: string;
  status: keyof typeof statuses;
  canMessage?: boolean;
  activity: { time: string; text: string }[];
}

export interface PermissionQuestion {
  agentId: string;
  anchorId?: string;
  requestId: string;
  tool: string;
  input: string;
  preview?: string;
  canAllow?: boolean;
}

export const demoPermissions: PermissionQuestion[] = [
  {
    agentId: "researcher",
    requestId: "demo-reference-access",
    tool: "Read",
    input: "Read docs/reference.md",
    preview: "Demo only: simulate reading the design reference. No file will be accessed.",
    canAllow: true,
  },
];

export const demoAgents: Agent[] = [
  {
    id: "main",
    name: "Phi",
    role: "Primary agent",
    model: "Demo model",
    status: "waiting",
    activity: [
      { time: "10:42", text: "Delegated the page layout to Builder." },
      { time: "10:43", text: "Asked Reviewer to check the service boundary." },
      { time: "10:44", text: "Waiting for delegated results." },
    ],
  },
  {
    id: "builder",
    name: "Builder",
    role: "Frontend agent",
    model: "Demo model",
    status: "working",
    activity: [
      { time: "10:42", text: "Received the layout task from Phi." },
      { time: "10:43", text: "Read the reference design." },
      { time: "10:44", text: "Tool: writing the office stylesheet." },
    ],
  },
  {
    id: "reviewer",
    name: "Reviewer",
    role: "Review agent",
    model: "Demo model",
    status: "working",
    activity: [
      { time: "10:43", text: "Received a review request from Phi." },
      { time: "10:44", text: "Tool: reading the runtime design." },
    ],
  },
  {
    id: "researcher",
    name: "Researcher",
    role: "Research agent",
    model: "Demo model",
    status: "blocked",
    activity: [{ time: "10:41", text: "Requested permission to access a reference." }],
  },
  {
    id: "tester",
    name: "Tester",
    role: "Test agent",
    model: "Demo model",
    status: "idle",
    activity: [{ time: "10:40", text: "Finished the previous test run." }],
  },
  {
    id: "writer",
    name: "Writer",
    role: "Documentation agent",
    model: "Demo model",
    status: "stopped",
    activity: [{ time: "10:39", text: "Returned setup notes to Phi and stopped." }],
  },
];

export const demoFrames: { id: string; status: Agent["status"]; text: string }[] = [
  {
    id: "builder",
    status: "idle",
    text: "Returned the office layout to Phi.",
  },
  {
    id: "reviewer",
    status: "error",
    text: "Demo tool failed. No live execution was affected.",
  },
  {
    id: "main",
    status: "working",
    text: "Received Builder’s result and resumed work.",
  },
  {
    id: "main",
    status: "idle",
    text: "Completed the demo turn.",
  },
];
