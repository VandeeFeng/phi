import {
  type ConnectAuth,
  type ConnectChallenge,
  type ConnectResult,
  type DeviceChallenge,
  decodeFrame,
  type PairingResolution,
  PROTOCOL_VERSION,
  proofPayload,
  type SessionEvent,
  SUBPROTOCOL,
  WS_PATH,
} from "../remote/protocol.js";

const STORAGE_KEY = "phi.remote.device";
const HANDSHAKE_TIMEOUT_MS = 10_000;
const MAX_AUTH_BYTES = 4096;
const MAX_QUEUED_FRAMES = 8;
const MAX_EVENT_BYTES = 8 * 1024 * 1024;
const MAX_EVENT_QUEUE = 256;
const MAX_QUEUED_BYTES = 16 * 1024 * 1024;
const FIRST_WATCH_REQUEST_ID = 3;
const IDENTITY_PATTERN = /^[a-f0-9]{64}$/;

export type EntryOperation = "enroll" | "recover";
export type AuthState = {
  status: "signedOut" | "connecting" | "pending" | "ready" | "error";
  message: string;
};

type DeviceRecord = {
  version: 1;
  publicKey: number[];
  privateKey: JsonWebKey;
  gatewayId?: string;
  credential?: string;
};

function identity(value: unknown): value is string {
  return typeof value === "string" && IDENTITY_PATTERN.test(value);
}

function bytesEqual(left: unknown, right: number[]): boolean {
  return Array.isArray(left) && left.length === right.length && left.every((byte, index) => byte === right[index]);
}

function hexadecimal(bytes: ArrayBuffer): string {
  return [...new Uint8Array(bytes)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

export class BrowserAuth {
  private socket?: WebSocket;
  private timer?: ReturnType<typeof setTimeout>;
  private attempt = 0;
  private activeCredential: string | null = null;
  private nextRequest = FIRST_WATCH_REQUEST_ID;
  private selection?: {
    sessionId: string;
    watchId: string;
    accepted: boolean;
    ready: Promise<void>;
    resolve: () => void;
    reject: (error: Error) => void;
  };

  constructor(
    private readonly notify: (state: AuthState) => void,
    private readonly observe: (event: SessionEvent) => void = () => {},
  ) {}

  watch(sessionId: string): Promise<void> {
    if (!this.activeCredential || !this.socket) return Promise.reject(new Error("Device authentication required"));
    if (this.selection?.sessionId === sessionId) return this.selection.ready;
    this.selection?.reject(new Error("Gateway selection changed"));
    const watchId = String(this.nextRequest++);
    let resolve!: () => void;
    let reject!: (error: Error) => void;
    const ready = new Promise<void>((accepted, rejected) => {
      resolve = accepted;
      reject = rejected;
    });
    this.selection = { sessionId, watchId, accepted: false, ready, resolve, reject };
    clearTimeout(this.timer);
    this.timer = setTimeout(() => this.fail("Gateway watch acknowledgement timed out."), HANDSHAKE_TIMEOUT_MS);
    try {
      this.socket.send(JSON.stringify({ type: "req", id: watchId, method: "session.watch", params: { sessionId } }));
    } catch {
      this.fail("Cannot select the Gateway event stream.");
    }
    return ready;
  }

  get credential(): string | null {
    return this.activeCredential;
  }

  async resume(): Promise<void> {
    await this.begin(undefined, "device");
  }

  async signIn(credential: string, operation: EntryOperation): Promise<void> {
    await this.begin(credential.trim(), operation);
  }

  close(): void {
    this.retire();
    this.notify({ status: "signedOut", message: "Disconnected. Reconnect using the saved device identity." });
  }

  forget(): void {
    this.retire();
    try {
      localStorage.removeItem(STORAGE_KEY);
      this.notify({
        status: "signedOut",
        message:
          "Device identity removed from this browser. Ask the host operator to revoke this device with phi gateway devices revoke <device-id>.",
      });
    } catch {
      this.notify({ status: "error", message: "Cannot remove the saved device identity. Check site storage." });
    }
  }

  private retire(): void {
    this.attempt += 1;
    clearTimeout(this.timer);
    this.timer = undefined;
    const socket = this.socket;
    this.socket = undefined;
    this.activeCredential = null;
    this.selection?.reject(new Error("Gateway event stream retired"));
    this.selection = undefined;
    this.nextRequest = FIRST_WATCH_REQUEST_ID;
    socket?.close();
  }

  private fail(message: string): void {
    this.retire();
    this.notify({ status: "error", message });
  }

  private save(record: DeviceRecord): void {
    try {
      localStorage.setItem(STORAGE_KEY, JSON.stringify(record));
    } catch {
      throw new Error("Browser storage is unavailable. Enable site storage before pairing.");
    }
  }

  private async load(): Promise<DeviceRecord> {
    if (!globalThis.crypto?.subtle) throw new Error("Device pairing requires HTTPS or a loopback browser origin.");
    let stored: string | null;
    try {
      stored = localStorage.getItem(STORAGE_KEY);
    } catch {
      throw new Error("Browser storage is unavailable. Enable site storage before pairing.");
    }
    if (stored) {
      let record: DeviceRecord;
      try {
        if (stored.length > MAX_AUTH_BYTES) throw new Error();
        record = JSON.parse(stored) as DeviceRecord;
        if (
          record.version !== 1 ||
          !Array.isArray(record.publicKey) ||
          record.publicKey.length !== 32 ||
          record.publicKey.some((byte) => !Number.isInteger(byte) || byte < 0 || byte > 255) ||
          record.privateKey.kty !== "OKP" ||
          record.privateKey.crv !== "Ed25519" ||
          typeof record.privateKey.d !== "string" ||
          typeof record.privateKey.x !== "string" ||
          !bytesEqual(
            [...atob(record.privateKey.x.replace(/-/g, "+").replace(/_/g, "/"))].map((c) => c.charCodeAt(0)),
            record.publicKey,
          ) ||
          (record.gatewayId !== undefined && !identity(record.gatewayId)) ||
          (record.credential !== undefined && !identity(record.credential))
        )
          throw new Error();
      } catch {
        throw new Error("Saved device identity is invalid. Clear this site's data and pair again.");
      }
      return record;
    }
    const keys = (await crypto.subtle.generateKey("Ed25519", true, ["sign", "verify"])) as CryptoKeyPair;
    const record: DeviceRecord = {
      version: 1,
      publicKey: [...new Uint8Array(await crypto.subtle.exportKey("raw", keys.publicKey))],
      privateKey: await crypto.subtle.exportKey("jwk", keys.privateKey),
    };
    return record;
  }

  private async begin(entryCredential: string | undefined, operation: "device" | EntryOperation): Promise<void> {
    this.retire();
    const attempt = this.attempt;
    this.notify({ status: "connecting", message: "Verifying device…" });
    try {
      const record = await this.load();
      if (attempt !== this.attempt) return;
      this.save(record);
      const credential = operation === "device" ? record.credential : entryCredential;
      if (!credential && operation === "device") {
        this.notify({
          status: "signedOut",
          message: "Run phi gateway token in a terminal on the host, then enter it here.",
        });
        return;
      }
      if (!identity(credential)) throw new Error("Enter the 64-character pairing token shown by phi gateway token.");
      this.open(record, credential, operation);
    } catch (error) {
      if (attempt === this.attempt) this.fail((error as Error).message);
    }
  }

  private open(
    record: DeviceRecord,
    credential: string,
    operation: Exclude<ConnectAuth["kind"], "local">,
    requestId?: string,
  ): void {
    this.retire();
    const endpoint = new URL(WS_PATH, location.origin);
    endpoint.protocol = location.protocol === "https:" ? "wss:" : "ws:";
    const socket = new WebSocket(endpoint, SUBPROTOCOL);
    this.socket = socket;
    let sequence = 0;
    let queued = 0;
    let queuedBytes = 0;
    let handling = Promise.resolve();
    let challenge: ConnectChallenge;
    let expected: "challenge" | "connect" | "prove" | "pending" | "ready" = "challenge";
    let pending: Extract<ConnectResult, { status: "pending" }> | undefined;
    const current = () => this.socket === socket;
    const deadline = (milliseconds: number) => {
      clearTimeout(this.timer);
      this.timer = setTimeout(() => {
        if (current()) this.fail("Device verification expired. Reconnect or request pairing again.");
      }, milliseconds);
    };
    const send = (id: string, method: "connect" | "connect.prove", params: unknown) => {
      const text = JSON.stringify({ type: "req", id, method, params });
      if (new TextEncoder().encode(text).length > MAX_AUTH_BYTES)
        throw new Error("Authentication exceeds the Gateway limit.");
      socket.send(text);
    };
    const resolve = (resolution: PairingResolution) => {
      if (!pending || resolution.requestId !== pending.requestId || resolution.deviceId !== pending.deviceId) {
        throw new Error("Pairing decision does not match this device request.");
      }
      switch (resolution.status) {
        case "approved":
          this.open(record, credential, "claim", pending.requestId);
          return;
        case "rejected":
          throw new Error("Pairing request rejected. Request again only after checking with the operator.");
        case "expired":
          throw new Error("Pairing request expired. Submit a new request.");
        default:
          throw new Error("Unexpected pairing decision.");
      }
    };
    const receive = async (text: string) => {
      if (new TextEncoder().encode(text).length > (expected === "ready" ? MAX_EVENT_BYTES : MAX_AUTH_BYTES))
        throw new Error("Gateway frame exceeds the limit.");
      const frame = decodeFrame(text);
      if (frame.type === "event") {
        if (frame.seq !== sequence + 1) throw new Error("Gateway authentication event order mismatch.");
        sequence = frame.seq;
        if (expected === "challenge" && frame.event === "connect.challenge") {
          challenge = frame.payload as ConnectChallenge;
          if (
            socket.protocol !== SUBPROTOCOL ||
            !Number.isSafeInteger(challenge.minProtocol) ||
            !Number.isSafeInteger(challenge.maxProtocol) ||
            challenge.minProtocol > PROTOCOL_VERSION ||
            challenge.maxProtocol < PROTOCOL_VERSION ||
            !identity(challenge.gatewayId) ||
            !identity(challenge.bootId) ||
            !identity(challenge.connectionId) ||
            !identity(challenge.nonce) ||
            !Number.isSafeInteger(challenge.expiresAt) ||
            challenge.expiresAt * 1000 <= Date.now() ||
            (record.gatewayId !== undefined && record.gatewayId !== challenge.gatewayId)
          )
            throw new Error("Gateway connection identity or protocol mismatch.");
          expected = "connect";
          send("1", "connect", {
            minProtocol: PROTOCOL_VERSION,
            maxProtocol: PROTOCOL_VERSION,
            auth: { kind: operation, credential, publicKey: record.publicKey, ...(requestId ? { requestId } : {}) },
          });
          return;
        }
        if (expected === "ready" && frame.event === "session.event") {
          const event = frame.payload as SessionEvent;
          if (!event || typeof event.sessionId !== "string" || typeof event.watchId !== "string" || !event.event)
            throw new Error("Invalid Gateway Session event.");
          const selection = this.selection;
          if (!selection || event.watchId !== selection.watchId) return;
          if (!selection.accepted || event.sessionId !== selection.sessionId)
            throw new Error("Gateway event selection mismatch.");
          this.observe(event);
          return;
        }
        if (expected === "pending" && frame.event === "pairing.resolved") {
          resolve(frame.payload as PairingResolution);
          return;
        }
        throw new Error("Unexpected Gateway authentication event.");
      }
      if (expected === "ready" && frame.type === "res") {
        const selection = this.selection;
        if (selection && Number(frame.id) >= FIRST_WATCH_REQUEST_ID && Number(frame.id) < Number(selection.watchId))
          return;
        if (!selection || frame.id !== selection.watchId || selection.accepted)
          throw new Error("Gateway watch response mismatch.");
        if (!frame.ok) throw new Error(`Gateway watch rejected (${frame.error.code}).`);
        const selected = frame.payload as { sessionId: string; watchId: string };
        if (selected.sessionId !== selection.sessionId || selected.watchId !== selection.watchId)
          throw new Error("Gateway watch acknowledgement mismatch.");
        selection.accepted = true;
        clearTimeout(this.timer);
        selection.resolve();
        return;
      }
      if (frame.type !== "res" || frame.id !== (expected === "connect" ? "1" : expected === "prove" ? "2" : "")) {
        throw new Error("Gateway authentication response mismatch.");
      }
      if (!frame.ok) {
        const guidance =
          frame.error.code === "REVOKED"
            ? "Request pairing again and ask the host operator to approve this device."
            : "Check approval or use explicit credential recovery.";
        throw new Error(`Device authentication failed (${frame.error.code}). ${guidance}`);
      }
      const result = frame.payload as ConnectResult;
      if (expected === "connect" && result.status === "proofRequired") {
        const proof: DeviceChallenge = result.challenge;
        const deviceId = hexadecimal(await crypto.subtle.digest("SHA-256", Uint8Array.from(record.publicKey)));
        if (
          proof.gatewayId !== challenge.gatewayId ||
          proof.bootId !== challenge.bootId ||
          proof.connectionId !== challenge.connectionId ||
          proof.audience !== new URL(WS_PATH, location.origin).href ||
          proof.deviceId !== deviceId ||
          !bytesEqual(proof.publicKey, record.publicKey) ||
          proof.operation !== (operation === "device" ? "reconnect" : operation) ||
          proof.requestId !== (requestId ?? null) ||
          !identity(proof.challengeId) ||
          !identity(proof.nonce) ||
          !Number.isSafeInteger(proof.expiresAt) ||
          proof.expiresAt * 1000 <= Date.now() ||
          (proof.authorizationId !== null && !identity(proof.authorizationId))
        )
          throw new Error("Gateway proof does not match this connection and device.");
        const key = await crypto.subtle.importKey("jwk", record.privateKey, "Ed25519", false, ["sign"]);
        const digest = hexadecimal(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(credential)));
        const signature = await crypto.subtle.sign("Ed25519", key, Uint8Array.from(proofPayload(proof, digest)));
        if (!current()) return;
        expected = "prove";
        send("2", "connect.prove", { challengeId: proof.challengeId, signature: [...new Uint8Array(signature)] });
        return;
      }
      if (expected !== "prove") throw new Error("Gateway did not request device proof.");
      if (result.status === "pending" && ["enroll", "claim"].includes(operation)) {
        const deviceId = hexadecimal(await crypto.subtle.digest("SHA-256", Uint8Array.from(record.publicKey)));
        if (!current()) return;
        if (
          !identity(result.requestId) ||
          result.deviceId !== deviceId ||
          !Number.isSafeInteger(result.expiresAt) ||
          result.expiresAt * 1000 <= Date.now()
        ) {
          throw new Error("Invalid Gateway pairing request.");
        }
        pending = result;
        expected = "pending";
        deadline(result.expiresAt * 1000 - Date.now());
        this.notify({
          status: "pending",
          message: `Waiting for host approval. Run: phi gateway devices approve\nRequest ID: ${result.requestId}`,
        });
        return;
      }
      if (
        result.status !== "ready" ||
        operation === "enroll" ||
        result.hello.protocol !== PROTOCOL_VERSION ||
        result.hello.gatewayId !== challenge.gatewayId ||
        result.hello.bootId !== challenge.bootId ||
        result.hello.connectionId !== challenge.connectionId ||
        !result.hello.capabilities.includes("hostedSessions") ||
        result.hello.capabilities.includes("localManagement") ||
        (operation !== "device" && !identity(result.deviceCredential)) ||
        (result.deviceCredential !== undefined && !identity(result.deviceCredential))
      )
        throw new Error("Invalid Gateway device authorization.");
      record.gatewayId = challenge.gatewayId;
      record.credential = result.deviceCredential ?? credential;
      this.save(record);
      clearTimeout(this.timer);
      expected = "ready";
      this.activeCredential = record.credential;
      credential = "";
      this.notify({ status: "ready", message: "Device verified." });
    };
    deadline(HANDSHAKE_TIMEOUT_MS);
    socket.onmessage = (event) => {
      if (!current()) return;
      const bytes = typeof event.data === "string" ? new TextEncoder().encode(event.data).length : MAX_QUEUED_BYTES;
      if (
        typeof event.data !== "string" ||
        ++queued > (expected === "ready" ? MAX_EVENT_QUEUE : MAX_QUEUED_FRAMES) ||
        bytes > MAX_QUEUED_BYTES - queuedBytes
      ) {
        this.fail("Invalid or overloaded Gateway event stream.");
        return;
      }
      queuedBytes += bytes;
      handling = handling
        .then(async () => {
          if (current()) await receive(event.data);
        })
        .catch((error: Error) => {
          if (current()) this.fail(error.message);
        })
        .finally(() => {
          queued -= 1;
          queuedBytes -= bytes;
        });
    };
    socket.onerror = () => {
      if (current()) this.fail("Cannot connect to the Gateway. Check its address and HTTPS configuration.");
    };
    socket.onclose = () => {
      if (current()) this.fail("Gateway disconnected. Reconnect to verify current device access.");
    };
  }
}
