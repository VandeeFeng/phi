import assert from "node:assert/strict";
import { createHash, webcrypto } from "node:crypto";
import { readFile } from "node:fs/promises";
import { type TestContext, test } from "node:test";
import { setImmediate } from "node:timers/promises";
import { type AuthState, BrowserAuth } from "./public/auth.js";
import {
  type Connect,
  type DeviceChallenge,
  decodeConnect,
  decodeFrame,
  negotiate,
  ProtocolError,
  proofPayload,
  RequestOrder,
  type SessionEvent,
} from "./remote/protocol.js";

test("socket-bound proof bytes and Ed25519 signature match Rust", async () => {
  const fixture = JSON.parse(await readFile(new URL("../../../fixtures/proof-v1.json", import.meta.url), "utf8"));
  const digest = createHash("sha256").update(fixture.credential).digest("hex");
  assert.equal(digest, fixture.credentialDigest);
  const payload = proofPayload(fixture.challenge, digest);
  assert.equal(new TextDecoder().decode(payload), fixture.payload);
  const key = await webcrypto.subtle.importKey("raw", new Uint8Array(fixture.challenge.publicKey), "Ed25519", false, [
    "verify",
  ]);
  assert.equal(await webcrypto.subtle.verify("Ed25519", key, new Uint8Array(fixture.signature), payload), true);
});

test("Rust and TypeScript share strict Remote envelopes", async () => {
  const fixtures = JSON.parse(await readFile(new URL("../../../fixtures/remote-v1.json", import.meta.url), "utf8"));
  for (const value of fixtures.valid) {
    assert.deepEqual(decodeFrame(JSON.stringify(value)), value);
  }
  for (const value of fixtures.connectValid) {
    assert.deepEqual(decodeConnect(value), value);
    assert.equal(negotiate(decodeConnect(value)), 1);
  }
  for (const value of fixtures.connectInvalid) {
    assert.throws(() => decodeConnect(value), ProtocolError);
  }
  for (const value of fixtures.invalid) {
    assert.throws(() => decodeFrame(JSON.stringify(value)), ProtocolError);
  }
  assert.throws(() => decodeFrame("not JSON"), { message: "INVALID_REQUEST" });
});

test("authentication fields are strict and negotiation has no legacy fallback", () => {
  const connect = { minProtocol: 1, maxProtocol: 1, auth: { kind: "local", credential: "test-only" } };
  assert.equal(negotiate(decodeConnect(connect)), 1);
  assert.throws(() => negotiate(decodeConnect({ ...connect, minProtocol: 2 })), { code: "PROTOCOL_MISMATCH" });
  assert.throws(
    () => decodeConnect({ ...connect, auth: { ...connect.auth, publicKey: Array(32).fill(0) } }),
    ProtocolError,
  );
});

test("request reservation is connection-local and never moves backwards", () => {
  const order = new RequestOrder();
  order.reserve("1");
  order.reserve("3");
  for (const id of ["1", "2", "3", "01", "9007199254740992"]) {
    assert.throws(() => order.reserve(id), ProtocolError);
  }
  order.reserve("4");
  new RequestOrder().reserve("1");
});

const DEVICE_STORAGE_KEY = "phi.remote.device";
const ENTRY_CREDENTIAL = "a".repeat(64);
const DEVICE_CREDENTIAL = "b".repeat(64);
const GATEWAY_ID = "c".repeat(64);
const BOOT_ID = "d".repeat(64);
const REQUEST_ID = "e".repeat(64);

async function until(condition: () => boolean): Promise<void> {
  const deadline = Date.now() + 3000;
  while (!condition()) {
    assert.ok(Date.now() < deadline, "browser operation did not settle");
    await setImmediate();
  }
}

function installGlobals(context: TestContext, globals: Record<string, unknown>) {
  const descriptors = Object.keys(globals).map(
    (key) => [key, Object.getOwnPropertyDescriptor(globalThis, key)] as const,
  );
  for (const [key, value] of Object.entries(globals)) {
    Object.defineProperty(globalThis, key, { configurable: true, writable: true, value });
  }
  context.after(() => {
    for (const [key, descriptor] of descriptors) {
      if (descriptor) Object.defineProperty(globalThis, key, descriptor);
      else Reflect.deleteProperty(globalThis, key);
    }
  });
}

async function browserFixture(origin = "https://phi.example.com") {
  const keys = (await webcrypto.subtle.generateKey("Ed25519", true, ["sign", "verify"])) as webcrypto.CryptoKeyPair;
  const publicKey = [...new Uint8Array(await webcrypto.subtle.exportKey("raw", keys.publicKey))];
  const stored = {
    version: 1,
    publicKey,
    privateKey: await webcrypto.subtle.exportKey("jwk", keys.privateKey),
    gatewayId: GATEWAY_ID,
  };
  const values = new Map([[DEVICE_STORAGE_KEY, JSON.stringify(stored)]]);
  const sockets: TestSocket[] = [];
  const failures: Error[] = [];
  const control = {
    replies: "respond",
    approval: "pending",
    transform: (frame: unknown): unknown => frame,
    credential: DEVICE_CREDENTIAL,
  };
  class TestSocket {
    onmessage?: (event: { data: string }) => void;
    onerror?: () => void;
    onclose?: () => void;
    closed = false;
    sent: { id: string; method: string; params: Record<string, unknown> }[] = [];
    connectionId: string;
    proof?: DeviceChallenge;
    connect?: Connect;
    deviceId = "";
    sequence = 1;

    constructor(
      readonly url: URL,
      readonly protocol: string,
    ) {
      sockets.push(this);
      this.connectionId = sockets.length.toString(16).padStart(64, "0");
      const endpoint = new URL("/ws", origin);
      endpoint.protocol = endpoint.protocol === "https:" ? "wss:" : "ws:";
      assert.equal(url.href, endpoint.href);
      queueMicrotask(() =>
        this.deliver({
          type: "event",
          event: "connect.challenge",
          seq: 1,
          payload: {
            minProtocol: 1,
            maxProtocol: 1,
            gatewayId: GATEWAY_ID,
            bootId: BOOT_ID,
            connectionId: this.connectionId,
            nonce: "f".repeat(64),
            expiresAt: Math.floor(Date.now() / 1000) + 30,
          },
        }),
      );
    }

    deliver(frame: unknown) {
      this.onmessage?.({ data: JSON.stringify(control.transform(frame)) });
    }

    send(text: string) {
      const request = JSON.parse(text);
      this.sent.push(request);
      if (control.replies === "hold") return;
      void this.respond(request).catch((error: Error) => {
        failures.push(error);
        this.onerror?.();
      });
    }

    async respond(request: { id: string; method: string; params: Record<string, unknown> }) {
      if (request.method === "connect") {
        this.connect = decodeConnect(request.params);
        assert.notEqual(this.connect.auth.kind, "local");
        assert.ok("publicKey" in this.connect.auth);
        const auth = this.connect.auth as Exclude<Connect["auth"], { kind: "local" }>;
        this.deviceId = createHash("sha256").update(Uint8Array.from(auth.publicKey)).digest("hex");
        this.proof = {
          gatewayId: GATEWAY_ID,
          bootId: BOOT_ID,
          connectionId: this.connectionId,
          audience: new URL("/ws", origin).href,
          challengeId: "f".repeat(64),
          nonce: "1".repeat(64),
          deviceId: this.deviceId,
          publicKey: auth.publicKey,
          operation: auth.kind === "device" ? "reconnect" : auth.kind,
          requestId: auth.kind === "claim" ? auth.requestId : null,
          expiresAt: Math.floor(Date.now() / 1000) + 30,
          authorizationId: ["device", "recover", "claim"].includes(auth.kind) ? REQUEST_ID : null,
        };
        this.deliver({
          type: "res",
          id: request.id,
          ok: true,
          payload: { status: "proofRequired", challenge: this.proof },
        });
        return;
      }
      if (request.method === "session.watch") {
        this.deliver({
          type: "res",
          id: request.id,
          ok: true,
          payload: { sessionId: request.params.sessionId, watchId: request.id },
        });
        return;
      }
      assert.equal(request.method, "connect.prove", "browser auth must not send business commands");
      assert.ok(this.proof && this.connect);
      const auth = this.connect.auth as Exclude<Connect["auth"], { kind: "local" }>;
      const key = await webcrypto.subtle.importKey("raw", Uint8Array.from(auth.publicKey), "Ed25519", false, [
        "verify",
      ]);
      const digest = createHash("sha256").update(auth.credential).digest("hex");
      assert.equal(
        await webcrypto.subtle.verify(
          "Ed25519",
          key,
          Uint8Array.from(request.params.signature as number[]),
          Uint8Array.from(proofPayload(this.proof, digest)),
        ),
        true,
      );
      const payload =
        auth.kind === "enroll" || (auth.kind === "claim" && control.approval === "pending")
          ? {
              status: "pending",
              requestId: REQUEST_ID,
              deviceId: this.deviceId,
              expiresAt: Math.floor(Date.now() / 1000) + 600,
            }
          : {
              status: "ready",
              hello: {
                protocol: 1,
                gatewayId: GATEWAY_ID,
                bootId: BOOT_ID,
                connectionId: this.connectionId,
                capabilities: ["hostedSessions"],
                limits: {},
              },
              ...(auth.kind !== "device" ? { deviceCredential: control.credential } : {}),
            };
      this.deliver({ type: "res", id: request.id, ok: true, payload });
    }

    decision(status: "approved" | "rejected" | "expired") {
      control.approval = status;
      this.deliver({
        type: "event",
        event: "pairing.resolved",
        seq: ++this.sequence,
        payload: { requestId: REQUEST_ID, deviceId: this.deviceId, status },
      });
    }

    sessionEvent(event: SessionEvent["event"], selection?: { sessionId: string; watchId: string }) {
      const watch = [...this.sent].reverse().find((request) => request.method === "session.watch")!;
      this.deliver({
        type: "event",
        event: "session.event",
        seq: ++this.sequence,
        payload: { sessionId: watch.params.sessionId, watchId: watch.id, ...selection, event },
      });
    }

    close() {
      this.closed = true;
    }
    remoteClose() {
      this.closed = true;
      this.onclose?.();
    }
  }
  return {
    sockets,
    values,
    failures,
    control,
    remember() {
      values.set(DEVICE_STORAGE_KEY, JSON.stringify({ ...stored, credential: DEVICE_CREDENTIAL }));
    },
    globals: {
      crypto: webcrypto,
      localStorage: {
        getItem: (key: string) => values.get(key) ?? null,
        setItem: (key: string, value: string) => values.set(key, value),
        removeItem: (key: string) => values.delete(key),
      },
      WebSocket: TestSocket,
      location: new URL("/#session=hosted-session", origin),
    },
  };
}

function browserAuth(context: TestContext, fixture: Awaited<ReturnType<typeof browserFixture>>) {
  const states: AuthState[] = [];
  installGlobals(context, fixture.globals);
  const auth = new BrowserAuth((state) => states.push(state));
  context.after(() => auth.close());
  return { auth, states };
}

test("browser watches only the selected root and retires incomplete streams without replay", async (context) => {
  const fixture = await browserFixture();
  fixture.remember();
  installGlobals(context, fixture.globals);
  const states: AuthState[] = [];
  const observed: SessionEvent[] = [];
  const auth = new BrowserAuth(
    (state) => states.push(state),
    (event) => observed.push(event),
  );
  context.after(() => auth.close());
  await auth.resume();
  await until(() => auth.credential !== null);
  await auth.watch("first");
  const socket = fixture.sockets[0];
  socket.sessionEvent({ kind: "boundary", id: "other-client-marker" });
  await until(() => observed.length === 1);
  await auth.watch("second");
  socket.sessionEvent({ kind: "refresh", reason: "state" }, { sessionId: "first", watchId: "3" });
  socket.sessionEvent({
    kind: "phi",
    id: "event",
    emittedAt: "now",
    sessionId: "second",
    turnId: null,
    messageId: null,
    agentId: null,
    parentAgentId: null,
    parentToolCallId: null,
    event: { type: "assistant_delta", display: "x".repeat(5000) },
  });
  await until(() => observed.length === 2);
  assert.equal(observed[1].sessionId, "second");
  socket.sessionEvent({ kind: "refresh", reason: "state" }, { sessionId: "foreign", watchId: "4" });
  await until(() => states.at(-1)?.status === "error");
  assert.equal(auth.credential, null);
  assert.equal(socket.closed, true);
  await auth.resume();
  await until(() => auth.credential !== null);
  socket.sessionEvent({ kind: "refresh", reason: "state" });
  await setImmediate();
  assert.equal(observed.length, 2);
  assert.deepEqual(
    fixture.sockets[1].sent.map((request) => request.method),
    ["connect", "connect.prove"],
  );
  assert.deepEqual(fixture.failures, []);
});

test("superseded watch replies cannot retire or change the current selection", async (context) => {
  const fixture = await browserFixture();
  fixture.remember();
  installGlobals(context, fixture.globals);
  const observed: SessionEvent[] = [];
  const auth = new BrowserAuth(
    () => {},
    (event) => observed.push(event),
  );
  context.after(() => auth.close());
  for (const oldFirst of [true, false]) {
    for (const oldSucceeded of [true, false]) {
      fixture.control.replies = "respond";
      await auth.resume();
      await until(() => auth.credential !== null);
      fixture.control.replies = "hold";
      const superseded = assert.rejects(auth.watch("first"), /selection changed/);
      const selected = assert.doesNotReject(auth.watch("second"));
      await superseded;
      const socket = fixture.sockets.at(-1)!;
      const [first, second] = socket.sent.filter((request) => request.method === "session.watch");
      const oldReply = () =>
        socket.deliver(
          oldSucceeded
            ? { type: "res", id: first.id, ok: true, payload: { sessionId: "first", watchId: first.id } }
            : { type: "res", id: first.id, ok: false, error: { code: "UNAVAILABLE", message: "superseded" } },
        );
      if (oldFirst) oldReply();
      await socket.respond(second);
      if (!oldFirst) oldReply();
      await selected;
      socket.sessionEvent({ kind: "refresh", reason: "state" }, { sessionId: "first", watchId: first.id });
      socket.sessionEvent({ kind: "refresh", reason: "state" });
      await until(() => observed.length > 0 || auth.credential === null);
      assert.equal(auth.credential, DEVICE_CREDENTIAL);
      assert.equal(socket.closed, false);
      assert.deepEqual(
        observed.splice(0).map((event) => event.sessionId),
        ["second"],
      );
    }
  }
  assert.deepEqual(fixture.failures, []);
});

test("browser bounds watch queues and rejects missing acknowledgements and sequence gaps", async (context) => {
  context.mock.timers.enable({ apis: ["setTimeout"] });
  const fixture = await browserFixture();
  fixture.remember();
  const { auth, states } = browserAuth(context, fixture);
  await auth.resume();
  await until(() => auth.credential !== null);
  fixture.control.replies = "hold";
  const pending = auth.watch("first");
  const rejected = assert.rejects(pending, /retired/);
  context.mock.timers.tick(10_001);
  await rejected;
  assert.match(states.at(-1)!.message, /acknowledgement timed out/);
  fixture.control.replies = "respond";
  await auth.resume();
  await until(() => auth.credential !== null);
  await auth.watch("first");
  const socket = fixture.sockets.at(-1)!;
  for (let index = 0; index <= 256; index++) socket.sessionEvent({ kind: "refresh", reason: "state" });
  assert.equal(auth.credential, null);
  assert.equal(socket.closed, true);
  await auth.resume();
  await until(() => auth.credential !== null);
  await auth.watch("first");
  const latest = fixture.sockets.at(-1)!;
  latest.sequence += 1;
  latest.sessionEvent({ kind: "refresh", reason: "state" });
  await until(() => states.at(-1)?.status === "error");
  assert.match(states.at(-1)!.message, /order mismatch/);
  assert.deepEqual(fixture.failures, []);
});

for (const origin of ["http://127.0.0.1:3141", "https://phi.example.com"]) {
  test(`browser pairs, claims, remembers and recovers at ${origin}`, async (context) => {
    const fixture = await browserFixture(origin);
    const { auth, states } = browserAuth(context, fixture);
    await auth.signIn(ENTRY_CREDENTIAL, "enroll");
    await until(() => states.at(-1)?.status === "pending");
    assert.equal(auth.credential, null);
    assert.match(states.at(-1)!.message, new RegExp(`phi gateway devices approve\nRequest ID: ${REQUEST_ID}`));
    assert.ok(!fixture.values.get(DEVICE_STORAGE_KEY)!.includes(ENTRY_CREDENTIAL));
    fixture.sockets[0].decision("approved");
    await until(() => auth.credential !== null);
    assert.equal(auth.credential, DEVICE_CREDENTIAL);
    assert.equal(fixture.sockets.length, 2);
    assert.equal(fixture.sockets[0].closed, true);
    assert.equal(fixture.sockets[1].connect?.auth.kind, "claim");
    const identityBefore = JSON.parse(fixture.values.get(DEVICE_STORAGE_KEY)!);
    auth.close();
    assert.equal(fixture.sockets[1].closed, true);
    assert.equal(auth.credential, null);
    assert.deepEqual(JSON.parse(fixture.values.get(DEVICE_STORAGE_KEY)!), identityBefore);
    await auth.resume();
    await until(() => states.at(-1)?.status === "ready");
    assert.equal(fixture.sockets[2].connect?.auth.kind, "device");
    assert.equal(fixture.sockets[2].connect?.auth.credential, DEVICE_CREDENTIAL);
    fixture.control.credential = "2".repeat(64);
    await auth.signIn(ENTRY_CREDENTIAL, "recover");
    await until(() => auth.credential === fixture.control.credential);
    const identityAfter = JSON.parse(fixture.values.get(DEVICE_STORAGE_KEY)!);
    assert.deepEqual(identityBefore.publicKey, identityAfter.publicKey);
    assert.deepEqual(identityBefore.privateKey, identityAfter.privateKey);
    auth.forget();
    assert.equal(auth.credential, null);
    assert.equal(fixture.values.has(DEVICE_STORAGE_KEY), false);
    assert.equal(fixture.sockets.at(-1)!.closed, true);
    assert.match(states.at(-1)!.message, /revoke.*phi gateway devices revoke/i);
    await auth.resume();
    assert.equal(states.at(-1)!.status, "signedOut");
    const replacement = JSON.parse(fixture.values.get(DEVICE_STORAGE_KEY)!);
    assert.notDeepEqual(replacement.publicKey, identityBefore.publicKey);
    assert.notDeepEqual(replacement.privateKey, identityBefore.privateKey);
    assert.equal(replacement.credential, undefined);
    assert.deepEqual(fixture.failures, []);
  });
}

for (const decision of ["rejected", "expired"] as const) {
  test(`browser stops on ${decision} pairing without automatic enrollment or mutation replay`, async (context) => {
    const fixture = await browserFixture();
    const { auth, states } = browserAuth(context, fixture);
    await auth.signIn(ENTRY_CREDENTIAL, "enroll");
    await until(() => states.at(-1)?.status === "pending");
    fixture.sockets[0].decision(decision);
    await until(() => states.at(-1)?.status === "error");
    assert.equal(auth.credential, null);
    assert.equal(fixture.sockets.length, 1);
    assert.equal(fixture.sockets[0].closed, true);
    assert.ok(!fixture.values.get(DEVICE_STORAGE_KEY)!.includes(ENTRY_CREDENTIAL));
  });
}

test("browser rejects foreign proof identities before signing and never falls back to entry auth", async (context) => {
  const fixture = await browserFixture();
  fixture.remember();
  const { auth, states } = browserAuth(context, fixture);
  for (const field of [
    "gatewayId",
    "bootId",
    "connectionId",
    "audience",
    "deviceId",
    "operation",
    "requestId",
    "expiresAt",
    "publicKey",
  ]) {
    fixture.control.transform = (frame) => {
      const value = structuredClone(frame) as { payload?: { status?: string; challenge?: Record<string, unknown> } };
      if (value.payload?.status === "proofRequired")
        value.payload.challenge![field] = field === "requestId" ? REQUEST_ID : null;
      return value;
    };
    await auth.resume();
    await until(() => states.at(-1)?.status === "error");
    assert.equal(fixture.sockets.at(-1)!.sent.length, 1, field);
    assert.equal(auth.credential, null);
  }
  assert.deepEqual(fixture.failures, []);
});

test("browser authentication cancellation, timeout and revoked responses retire only their own socket", async (context) => {
  context.mock.timers.enable({ apis: ["setTimeout"] });
  const fixture = await browserFixture();
  fixture.remember();
  fixture.control.replies = "hold";
  const { auth, states } = browserAuth(context, fixture);
  await auth.resume();
  await until(() => fixture.sockets[0]?.sent.length === 1);
  const obsolete = fixture.sockets[0];
  auth.close();
  await auth.resume();
  await until(() => fixture.sockets[1]?.sent.length === 1);
  obsolete.remoteClose();
  assert.equal(states.at(-1)?.status, "connecting");
  context.mock.timers.tick(10_000);
  assert.equal(states.at(-1)?.status, "error");
  assert.equal(fixture.sockets[1].closed, true);
  await auth.resume();
  await until(() => fixture.sockets[2]?.sent.length === 1);
  fixture.sockets[2].deliver({ type: "res", id: "1", ok: false, error: { code: "REVOKED", message: "revoked" } });
  await until(() => states.at(-1)?.status === "error");
  assert.match(states.at(-1)!.message, /REVOKED.*Request pairing again/);
  assert.equal(fixture.sockets.length, 3);
});

test("invalid or blocked browser storage cannot silently replace the device or authenticate", async (context) => {
  const fixture = await browserFixture();
  fixture.values.set(DEVICE_STORAGE_KEY, "invalid record");
  const { auth, states } = browserAuth(context, fixture);
  await auth.resume();
  assert.equal(states.at(-1)?.status, "error");
  assert.equal(fixture.values.get(DEVICE_STORAGE_KEY), "invalid record");
  assert.equal(fixture.sockets.length, 0);
  fixture.values.clear();
  fixture.globals.localStorage.setItem = () => {
    throw new Error("storage denied");
  };
  await auth.signIn(ENTRY_CREDENTIAL, "enroll");
  assert.equal(states.at(-1)?.status, "error");
  assert.equal(fixture.sockets.length, 0);
  assert.equal(fixture.values.size, 0);
});

test("forget reports blocked deletion without leaving the connection active", async (context) => {
  const fixture = await browserFixture();
  fixture.remember();
  const { auth, states } = browserAuth(context, fixture);
  await auth.resume();
  await until(() => auth.credential !== null);
  const saved = fixture.values.get(DEVICE_STORAGE_KEY);
  fixture.globals.localStorage.removeItem = () => {
    throw new Error("storage denied");
  };
  auth.forget();
  assert.equal(auth.credential, null);
  assert.equal(fixture.sockets.at(-1)!.closed, true);
  assert.equal(fixture.values.get(DEVICE_STORAGE_KEY), saved);
  assert.equal(states.at(-1)!.status, "error");
  assert.match(states.at(-1)!.message, /Cannot remove the saved device identity/);
});

test("cancelled identity creation cannot overwrite a newer paired device", async (context) => {
  const fixture = await browserFixture();
  fixture.values.clear();
  let release: (() => void) | undefined;
  const held = new Promise<void>((resolve) => {
    release = resolve;
  });
  let generations = 0;
  const subtle = new Proxy(webcrypto.subtle, {
    get(target, property) {
      if (property === "generateKey")
        return async (...args: Parameters<typeof target.generateKey>) => {
          const generation = ++generations;
          const keys = await target.generateKey(...args);
          if (generation === 1) await held;
          return keys;
        };
      const method = Reflect.get(target, property);
      return typeof method === "function" ? method.bind(target) : method;
    },
  });
  Object.defineProperty(fixture.globals, "crypto", { value: { subtle } });
  const { auth, states } = browserAuth(context, fixture);
  const obsolete = auth.resume();
  await until(() => generations === 1);
  auth.close();
  await auth.signIn(ENTRY_CREDENTIAL, "enroll");
  await until(() => states.at(-1)?.status === "pending");
  fixture.sockets[0].decision("approved");
  await until(() => auth.credential !== null);
  const paired = fixture.values.get(DEVICE_STORAGE_KEY);
  release!();
  await obsolete;
  assert.equal(fixture.values.get(DEVICE_STORAGE_KEY), paired);
});

type Listener = (event: { target?: TestElement; preventDefault?: () => void }) => unknown;

class TestElement {
  dataset: Record<string, string> = {};
  children: TestElement[] = [];
  listeners = new Map<string, Listener>();
  className = "";
  textContent = "";
  value = "";
  disabled = false;
  hidden = false;
  open = false;
  scrollHeight = 100;
  clientHeight = 100;
  scrollTop = 0;
  parent?: TestElement;

  constructor(readonly tag = "div") {}

  append(...children: TestElement[]) {
    if (this.tag === "button") {
      assert.ok(
        children.every((child) => !child.all().some((node) => node.tag === "button")),
        "nested buttons",
      );
    }
    for (const child of children) child.parent = this;
    this.children.push(...children);
  }

  replaceChildren(...children: TestElement[]) {
    this.children = [];
    this.append(...children);
  }

  all(): TestElement[] {
    return [this, ...this.children.flatMap((child) => child.all())];
  }

  querySelectorAll(selector: string) {
    return this.all().filter((node) => selector === "details[open]" && node.tag === "details" && node.open);
  }

  addEventListener(name: string, listener: Listener) {
    this.listeners.set(name, listener);
  }

  setAttribute(name: string, value: string) {
    if (name === "role") this.dataset.role = value;
  }

  closest(selector: string): TestElement | null {
    if (this.tag === "button" && selector.includes("data-decision") && this.dataset.decision) return this;
    if (this.tag === "button" && selector.includes("data-agent-id") && this.dataset.agentId) return this;
    return this.parent?.closest(selector) ?? null;
  }

  focus() {}
}

test("demo interaction never authenticates or invokes the Gateway", async (context) => {
  context.mock.timers.enable({ apis: ["setTimeout"] });
  const elements = new Map<string, TestElement>();
  const element = (selector: string) => {
    if (!elements.has(selector)) elements.set(selector, new TestElement());
    return elements.get(selector)!;
  };
  const windowEvents = new Map<string, () => void>();
  const body = new TestElement("body");
  body.dataset.mode = "demo";
  let requests = 0;
  const unexpectedRequest = () => {
    requests += 1;
    throw new Error("demo must not access the Gateway");
  };
  installGlobals(context, {
    document: {
      body,
      activeElement: null,
      querySelector: element,
      getElementById: (id: string) => element(`#${id}`),
      createElement: (tag: string) => new TestElement(tag),
    },
    location: { hash: "" },
    window: { addEventListener: (name: string, listener: () => void) => windowEvents.set(name, listener) },
    fetch: unexpectedRequest,
    WebSocket: class {
      constructor() {
        unexpectedRequest();
      }
    },
  });
  context.after(() => windowEvents.get("pagehide")?.());
  await import(`${new URL("./public/app.js", import.meta.url).href}?demo`);
  const desks = element("#desks");
  const question = desks.all().find((node) => node.tag === "details" && node.dataset.requestId);
  assert.ok(question);
  const deny = question.all().find((node) => node.dataset.decision === "deny");
  assert.ok(deny);
  desks.listeners.get("click")!({ target: deny });
  await setImmediate();
  assert.match(element("#view-notice").textContent, /No command was executed/);
  assert.equal(element("#terminal-hint").hidden, true);
  element("#demo-toggle").listeners.get("click")!({});
  assert.ok(desks.all().some((node) => node.tag === "details" && node.dataset.requestId));
  assert.equal(requests, 0);
});

for (const state of ["enrollment", "empty", "conversation", "permission", "permission-error"] as const) {
  test(`live page renders ${state} and its hosted session command`, async (context) => {
    context.mock.timers.enable({ apis: ["setTimeout"] });
    const elements = new Map<string, TestElement>();
    const element = (selector: string) => {
      if (!elements.has(selector)) elements.set(selector, new TestElement());
      return elements.get(selector)!;
    };
    const windowEvents = new Map<string, () => void>();
    const body = new TestElement("body");
    let finishSubmission: ((receipt: unknown) => void) | undefined;
    let submissionError: Error | undefined;
    let viewGate: Promise<void> | undefined;
    let canonicalReply = "\n\nHello!\n\nNext paragraph.";
    let canonicalLiveText = "\n\nStreaming reply";
    let canonicalTool = "Read · failed";
    let childPermission = false;
    const fixture = await browserFixture();
    if (state !== "enrollment") fixture.remember();
    let requests = 0;
    const cleanedUrls: string[] = [];
    const globals = {
      ...fixture.globals,
      history: { replaceState: (_state: unknown, _unused: string, url: string) => cleanedUrls.push(url) },
      document: {
        body,
        activeElement: null,
        querySelector: element,
        getElementById: (id: string) => element(`#${id}`),
        createElement: (tag: string) => new TestElement(tag),
      },
      location: new URL("https://phi.example.com/?token=old-query#token=old-fragment&session=hosted-session"),
      window: { addEventListener: (name: string, listener: () => void) => windowEvents.set(name, listener) },
      fetch: async (path: string, options?: RequestInit) => {
        requests += 1;
        assert.equal((options?.headers as Record<string, string>).Authorization, `Bearer ${DEVICE_CREDENTIAL}`);
        if (path === "/api/sessions/hosted-session/permission") {
          assert.deepEqual(JSON.parse(options?.body as string), {
            agentId: childPermission ? "child-agent" : "hosted-session",
            requestId: childPermission ? "child-approval" : "approval",
            decision: "allow_always",
          });
          childPermission = false;
          if (state === "permission-error") {
            return { ok: false, json: async () => ({ error: "Permission no longer pending" }) };
          }
          return {
            ok: true,
            json: async () => ({ status: "answered", persistenceWarning: "Failed to persist authorization" }),
          };
        }
        if (path === "/api/sessions/hosted-session/message") {
          if (submissionError) throw submissionError;
          return {
            ok: true,
            json: () =>
              new Promise((resolve) => {
                finishSubmission = resolve;
              }),
          };
        }
        assert.equal(path, "/api/sessions/hosted-session/view");
        await viewGate;
        const pendingPermissions: Record<string, string | boolean>[] = [];
        if (childPermission) {
          pendingPermissions.push({
            agentId: "child-agent",
            anchorId: "hosted-session",
            requestId: "child-approval",
            tool: "Bash",
            input: "child-only command",
          });
        } else if (state.startsWith("permission")) {
          pendingPermissions.push(
            { agentId: "hosted-session", requestId: "approval", tool: "Bash", input: "git status" },
            {
              agentId: "hosted-session",
              requestId: "unsupported",
              tool: "Bash",
              input: "too large",
              canAllow: false,
            },
          );
        }
        return {
          ok: true,
          json: async () => ({
            agents:
              state === "empty"
                ? []
                : [
                    {
                      id: "hosted-session",
                      name: "Phi",
                      role: "Agent",
                      model: "test",
                      status: "working",
                      canMessage: true,
                      activity: [],
                    },
                  ],
            sessionId: state === "empty" ? "" : "hosted-session",
            permissions: pendingPermissions,
            conversation: {
              messages: [
                { id: "reply", role: "assistant", text: canonicalReply },
                { id: "tool", role: "tool", text: canonicalTool },
                { id: "failure", role: "error", text: "connection lost" },
              ],
              liveText: canonicalLiveText,
            },
          }),
        };
      },
    };
    const descriptors = Object.keys(globals).map(
      (key) => [key, Object.getOwnPropertyDescriptor(globalThis, key)] as const,
    );
    for (const [key, value] of Object.entries(globals)) {
      Object.defineProperty(globalThis, key, { configurable: true, writable: true, value });
    }
    context.after(() => {
      windowEvents.get("pagehide")?.();
      for (const [key, descriptor] of descriptors) {
        if (descriptor) Object.defineProperty(globalThis, key, descriptor);
        else Reflect.deleteProperty(globalThis, key);
      }
    });
    await import(`${new URL("./public/app.js", import.meta.url).href}?live=${state}`);
    if (state === "enrollment") {
      await until(() => element("#auth-status").textContent.includes("Run phi gateway token"));
      assert.equal(requests, 0);
      assert.equal(fixture.sockets.length, 0);
      assert.equal(element("#auth-label").textContent, "Pair device");
      assert.equal(element("#device-auth").dataset.state, "signedOut");
      element("#device-auth").open = true;
      element("#pairing-token").value = ENTRY_CREDENTIAL;
      element("#auth-form").listeners.get("submit")!({ preventDefault() {} });
      await until(() => element("#auth-status").textContent.includes("Waiting for host approval"));
      assert.equal(element("#pairing-token").value, "");
      assert.equal(element("#auth-label").textContent, "Awaiting approval");
      assert.equal(element("#device-auth").dataset.state, "pending");
      assert.equal(element("#device-auth").open, true);
      assert.equal(requests, 0);
      assert.equal(fixture.sockets[0].connect?.auth.credential, ENTRY_CREDENTIAL);
      fixture.sockets[0].decision("approved");
    }
    await until(() => body.dataset.connection === "live");
    assert.deepEqual(cleanedUrls, ["/#session=hosted-session"]);
    assert.equal(element("#auth-form").hidden, true);
    assert.equal(element("#auth-label").textContent, "Paired");
    assert.equal(element("#device-auth").dataset.state, "ready");
    assert.equal(element("#device-auth").open, false);
    assert.equal(fixture.sockets[0].connect?.auth.kind, state === "enrollment" ? "enroll" : "device");
    assert.equal(element("#agent-detail").dataset.empty, String(state === "empty"));
    if (state === "empty") {
      assert.equal(element("#agent-name").textContent, "");
      assert.equal(element("#agent-role").textContent, "");
      assert.equal(element("#agent-status").textContent, "");
    } else {
      const paragraphs = element("#activity")
        .all()
        .filter((node) => node.tag === "p");
      assert.deepEqual(
        paragraphs.map((node) => node.textContent),
        ["Hello!\n\nNext paragraph.", "Read · failed", "connection lost", "Streaming reply"],
      );
      assert.deepEqual(
        element("#activity")
          .all()
          .filter((node) => node.tag === "time")
          .map((node) => node.textContent),
        ["Phi", "Tool", "Error", "Phi · live"],
      );
    }
    assert.equal(element("#terminal-hint").hidden, false);
    assert.equal(element("#terminal-command").textContent, "phi --gateway --session hosted-session");
    if (state.startsWith("permission")) {
      const desks = element("#desks");
      const question = desks.all().find((node) => node.tag === "details" && node.dataset.requestId === "approval");
      assert.ok(question);
      const always = question.all().find((node) => node.dataset.decision === "allow_always");
      assert.ok(always);
      assert.equal(always.textContent, "Always");
      assert.equal(always.disabled, false);
      desks.listeners.get("click")!({ target: always });
      await setImmediate();
      if (state === "permission-error") {
        assert.match(
          element("#message-status").textContent,
          /Permission no longer pending.*Check the current permission state/,
        );
        assert.equal(element("#view-notice").textContent, "");
        assert.ok(desks.all().some((node) => node.tag === "details" && node.dataset.requestId === "approval"));
        assert.ok(!desks.all().some((node) => node.className.includes("permission-feedback")));
        return;
      }
      const blocked = desks.all().find((node) => node.tag === "details" && node.dataset.requestId === "unsupported");
      assert.ok(blocked);
      for (const decision of ["allow_once", "allow_always", "deny"]) {
        const button: TestElement | undefined = blocked
          .all()
          .find((node) => node.tag === "button" && node.dataset.decision === decision);
        assert.ok(button);
        assert.equal(button.disabled, decision !== "deny");
      }
      assert.equal(
        element("#message-status").textContent,
        "permission allowed, but failed to persist allowlist: Failed to persist authorization",
      );
      assert.equal(element("#view-notice").textContent, "");
      assert.ok(!desks.all().some((node) => node.tag === "details" && node.dataset.requestId === "approval"));
      assert.ok(
        desks
          .all()
          .some((node) => node.dataset.decision === "allow_always" && node.className.includes("permission-feedback")),
      );
      await setImmediate();
      const beforeChild = requests;
      const socketCount = fixture.sockets.length;
      childPermission = true;
      context.mock.timers.tick(2000);
      await until(() =>
        desks.all().some((node) => node.tag === "details" && node.dataset.requestId === "child-approval"),
      );
      assert.equal(requests, beforeChild + 1, "child questions must refresh without root WS events");
      assert.equal(fixture.sockets.length, socketCount);
      assert.equal(element("#session-id").textContent, "hosted-session");
      const childQuestion = desks
        .all()
        .find((node) => node.tag === "details" && node.dataset.requestId === "child-approval")!;
      const childAlways = childQuestion
        .all()
        .find((node) => node.tag === "button" && node.dataset.decision === "allow_always")!;
      desks.listeners.get("click")!({ target: childAlways });
      await until(() => !childPermission);
      await setImmediate();
      assert.ok(!desks.all().some((node) => node.dataset.requestId === "child-approval"));
      assert.match(element("#message-status").textContent, /failed to persist allowlist/);
    }
    if (state === "conversation") {
      const socket = fixture.sockets.at(-1)!;
      const phi = (event: { type: string; [key: string]: unknown }) =>
        socket.sessionEvent({
          kind: "phi",
          id: "event",
          emittedAt: "now",
          sessionId: "hosted-session",
          turnId: "turn",
          messageId: "reply",
          agentId: "hosted-session",
          parentAgentId: null,
          parentToolCallId: null,
          event,
        });
      const activity = () =>
        element("#activity")
          .all()
          .filter((node) => node.tag === "p")
          .map((node) => node.textContent);
      phi({ type: "assistant_started", id: "reply" });
      phi({ type: "assistant_delta", display: "Live " });
      phi({ type: "assistant_delta", display: "reply" });
      phi({ type: "tool_call_started", id: "tool", name: "Read", args: { secret: "not a browser summary" } });
      await until(() => activity().includes("Live reply") && activity().includes("Read · running"));
      assert.ok(!activity().join().includes("not a browser summary"));
      phi({ type: "tool_call_finished", id: "tool", is_error: false, output: "private full output" });
      await until(() => activity().includes("Read · completed"));
      assert.ok(!activity().join().includes("private full output"));
      let finishView!: () => void;
      viewGate = new Promise((resolve) => {
        finishView = resolve;
      });
      const beforeRefresh = requests;
      phi({ type: "assistant_finished" });
      await until(() => requests > beforeRefresh);
      socket.sessionEvent({ kind: "refresh", reason: "interactions" });
      socket.sessionEvent({ kind: "refresh", reason: "history" });
      phi({ type: "assistant_delta", display: "covered by the overlapping read" });
      await setImmediate();
      assert.equal(requests, beforeRefresh + 1, "only one selected-view read may be in flight");
      canonicalReply = "Live reply";
      canonicalLiveText = "";
      canonicalTool = "Read · finished";
      finishView();
      viewGate = undefined;
      await until(() => activity()[0] === "Live reply");
      assert.deepEqual(activity(), ["Live reply", "Read · finished", "connection lost"]);
      context.mock.timers.tick(0);
      await until(() => requests === beforeRefresh + 2);
      await setImmediate();
      const input = element("#message");
      const notification = element("#message-status");
      const submit = element("#message-form").listeners.get("submit")!;
      input.value = "hello";
      const pending = submit({ preventDefault() {} });
      await setImmediate();
      assert.equal(input.disabled, true);
      assert.equal(notification.textContent, "");
      assert.ok(finishSubmission);
      finishSubmission({ kind: "submission", status: "started", notices: ["Hook notice"] });
      await pending;
      assert.equal(input.value, "");
      assert.equal(input.disabled, false);
      assert.equal(notification.textContent, "Hook notice");
      await submit({ preventDefault() {} });
      assert.equal(notification.textContent, "Enter a message up to 12 KiB.");
      submissionError = new Error("Connection lost");
      input.value = "retained draft";
      await submit({ preventDefault() {} });
      assert.match(notification.textContent, /Connection lost.*Check Phi before resending/);
      assert.equal(input.value, "retained draft");
      assert.equal(input.disabled, false);
    }
  });
}
