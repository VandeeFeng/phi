// Wire contracts only; socket, authority and rendering have separate owners.
export const PROTOCOL_VERSION = 1;
export const WS_PATH = "/ws";
export const SUBPROTOCOL = "phi.remote.v1";

const methods = ["connect", "connect.prove", "pairing.status", "session.watch", "session.unwatch"] as const;

const errorCodes = [
  "INVALID_REQUEST",
  "PROTOCOL_MISMATCH",
  "UNAUTHENTICATED",
  "PAIRING_PENDING",
  "PAIRING_REJECTED",
  "PAIRING_EXPIRED",
  "REVOKED",
  "STALE_CREDENTIAL",
  "FORBIDDEN",
  "NOT_FOUND",
  "BUSY",
  "LIMIT_EXCEEDED",
  "UNAVAILABLE",
  "INTERNAL",
] as const;

const events = ["connect.challenge", "pairing.resolved", "session.changed", "session.event"] as const;
export type Method = (typeof methods)[number];
export type ErrorCode = (typeof errorCodes)[number];
export type EventName = (typeof events)[number];
export type RemoteError = { code: ErrorCode; message: string };
export type Request<P = unknown> = { type: "req"; id: string; method: Method; params: P };
export type Response<P = unknown> =
  | { type: "res"; id: string; ok: true; payload: P }
  | { type: "res"; id: string; ok: false; error: RemoteError };
export type Event<P = unknown> = { type: "event"; event: EventName; seq: number; payload: P };
export type Frame = Request | Response | Event;

export type ConnectAuth =
  | { kind: "local"; credential: string }
  | { kind: "device" | "enroll" | "recover"; credential: string; publicKey: number[] }
  | { kind: "claim"; credential: string; publicKey: number[]; requestId: string };
export type Connect = { minProtocol: number; maxProtocol: number; auth: ConnectAuth };
export type SessionParams = { sessionId: string };
export type SessionWatch = SessionParams & { watchId: string };
export type SessionEvent = SessionWatch & {
  event:
    | { kind: "boundary"; id: string }
    | { kind: "refresh"; reason: "history" | "runtime" | "interactions" | "cancelled" | "failed" | "stopped" | "state" }
    | {
        kind: "phi";
        id: string;
        emittedAt: string;
        sessionId: string;
        turnId: string | null;
        messageId: string | null;
        agentId: string | null;
        parentAgentId: string | null;
        parentToolCallId: string | null;
        event: { type: string; [field: string]: unknown };
      }
    | {
        kind: "progress";
        turnId: string;
        status: "running" | "completed" | "failed" | "cancelled";
        cancellationReason: string | null;
        startedAt: string;
        durationMs: number;
        usage: { input_tokens: number; output_tokens: number; cache_read: number; cache_write: number; cost: number };
        toolUses: number;
        currentTools: { id: string; name: string }[];
        lastTool: { id: string; name: string } | null;
      };
};
export type SavedParams = { query?: string };
export type ResumeParams = { id: string };
export type SubmissionInput = {
  text: string;
  delivery: "steer" | "follow_up";
  images?: ({ type: "url"; url: string; mime_type?: string } | { type: "base64"; data: string; mime_type: string })[];
  fileContext?: string[];
};
export type MessageInput = { agentId?: string; message: string };
export type PermissionInput = {
  agentId: string;
  requestId: string;
  decision: "allow_once" | "allow_always" | "deny";
};
export type ConnectChallenge = {
  minProtocol: number;
  maxProtocol: number;
  gatewayId: string;
  bootId: string;
  connectionId: string;
  nonce: string;
  expiresAt: number;
};

export type DeviceChallenge = {
  gatewayId: string;
  bootId: string;
  connectionId: string;
  audience: string;
  challengeId: string;
  nonce: string;
  deviceId: string;
  publicKey: number[];
  operation: "enroll" | "claim" | "reconnect" | "recover";
  requestId: string | null;
  expiresAt: number;
  authorizationId: string | null;
};
export type DeviceProof = { challengeId: string; signature: number[] };
export type Limits = {
  authBytes: number;
  requestBytes: number;
  serverBytes: number;
  queueMessages: number;
  queueBytes: number;
  ordinaryRequests: number;
  controlRequests: number;
};
export type Hello = {
  protocol: number;
  gatewayId: string;
  bootId: string;
  connectionId: string;
  capabilities: ("hostedSessions" | "localManagement")[];
  limits: Limits;
};
export type ConnectResult =
  | { status: "proofRequired"; challenge: DeviceChallenge }
  | { status: "ready"; hello: Hello; deviceCredential?: string }
  | { status: "pending"; requestId: string; deviceId: string; expiresAt: number };
export type PairingResolution = {
  requestId: string;
  deviceId: string;
  status: "pending" | "approved" | "rejected" | "expired";
};

export function proofPayload(challenge: DeviceChallenge, credentialDigest: string): Uint8Array {
  const authKind = challenge.operation === "reconnect" ? "device" : challenge.operation;
  return new TextEncoder().encode(
    JSON.stringify([
      "phi.gateway.device-proof",
      PROTOCOL_VERSION,
      authKind,
      challenge.gatewayId,
      challenge.bootId,
      challenge.connectionId,
      challenge.audience,
      challenge.challengeId,
      challenge.nonce,
      challenge.deviceId,
      challenge.publicKey,
      challenge.operation,
      challenge.requestId,
      challenge.expiresAt,
      challenge.authorizationId,
      credentialDigest,
    ]),
  );
}

export class ProtocolError extends Error {
  constructor(readonly code: ErrorCode) {
    super(code);
  }
}

function invalid(): never {
  throw new ProtocolError("INVALID_REQUEST");
}

function object(value: unknown): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return invalid();
  return value as Record<string, unknown>;
}

function exact(value: Record<string, unknown>, keys: string[]): void {
  const actual = Object.keys(value);
  if (actual.length !== keys.length || actual.some((key) => !keys.includes(key))) invalid();
}

function member<T extends string>(value: unknown, values: readonly T[]): value is T {
  return typeof value === "string" && values.includes(value as T);
}

function positiveInteger(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value > 0;
}

export function requestId(value: unknown): string {
  if (typeof value !== "string" || !/^[1-9][0-9]*$/.test(value)) return invalid();
  if (!positiveInteger(Number(value))) return invalid();
  return value;
}

export function decodeFrame(text: string): Frame {
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    return invalid();
  }
  const value = object(parsed);
  switch (value.type) {
    case "req":
      exact(value, ["type", "id", "method", "params"]);
      requestId(value.id);
      if (!member(value.method, methods)) invalid();
      break;
    case "res":
      requestId(value.id);
      if (value.ok === true) {
        exact(value, ["type", "id", "ok", "payload"]);
      } else if (value.ok === false) {
        exact(value, ["type", "id", "ok", "error"]);
        const error = object(value.error);
        exact(error, ["code", "message"]);
        if (!member(error.code, errorCodes) || typeof error.message !== "string") invalid();
      } else {
        invalid();
      }
      break;
    case "event":
      exact(value, ["type", "event", "seq", "payload"]);
      if (!member(value.event, events) || !positiveInteger(value.seq)) invalid();
      break;
    default:
      invalid();
  }
  return value as Frame;
}

export function decodeConnect(params: unknown): Connect {
  const value = object(params);
  exact(value, ["minProtocol", "maxProtocol", "auth"]);
  const maxProtocolNumber = 0xffff_ffff;
  for (const version of [value.minProtocol, value.maxProtocol]) {
    if (typeof version !== "number" || !Number.isInteger(version) || version < 0 || version > maxProtocolNumber)
      invalid();
  }
  const auth = object(value.auth);
  if (typeof auth.credential !== "string") invalid();
  switch (auth.kind) {
    case "local":
      exact(auth, ["kind", "credential"]);
      break;
    case "device":
    case "enroll":
    case "recover":
    case "claim": {
      const fields = ["kind", "credential", "publicKey"];
      if (auth.kind === "claim") {
        fields.push("requestId");
        if (typeof auth.requestId !== "string") invalid();
      }
      exact(auth, fields);
      const publicKeyBytes = 32;
      const maxByte = 255;
      if (
        !Array.isArray(auth.publicKey) ||
        auth.publicKey.length !== publicKeyBytes ||
        auth.publicKey.some(
          (byte: unknown) => typeof byte !== "number" || !Number.isInteger(byte) || byte < 0 || byte > maxByte,
        )
      )
        invalid();
      break;
    }
    default:
      invalid();
  }
  return value as Connect;
}

export function negotiate(connect: Connect): number {
  if (connect.minProtocol > PROTOCOL_VERSION || connect.maxProtocol < PROTOCOL_VERSION) {
    throw new ProtocolError("PROTOCOL_MISMATCH");
  }
  return PROTOCOL_VERSION;
}

export class RequestOrder {
  private highWater = 0;

  reserve(id: string): void {
    const next = Number(requestId(id));
    if (next <= this.highWater) invalid();
    this.highWater = next;
  }
}
