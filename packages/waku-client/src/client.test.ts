import { describe, expect, test } from "bun:test";

import {
  WakuClient,
  WakuConnectionError,
  WakuRpcError,
  daemonUrl,
  type WebSocketLike,
} from "./client";
import { PROTOCOL_VERSION } from "./generated";

class FakeSocket implements WebSocketLike {
  readyState = 0;
  sent: string[] = [];
  private listeners = new Map<string, Array<(...args: any[]) => void>>();

  addEventListener(type: "open", listener: () => void): void;
  addEventListener(type: "message", listener: (event: MessageEvent) => void): void;
  addEventListener(type: "error", listener: () => void): void;
  addEventListener(type: "close", listener: (event: CloseEvent) => void): void;
  addEventListener(type: string, listener: (...args: any[]) => void): void {
    const listeners = this.listeners.get(type) ?? [];
    listeners.push(listener);
    this.listeners.set(type, listeners);
  }

  send(data: string): void {
    this.sent.push(data);
  }

  close(): void {
    this.readyState = 3;
  }

  emitClose(): void {
    this.readyState = 3;
    this.emit("close", { reason: "" });
  }

  fail(reason: string): void {
    this.readyState = 3;
    this.emit("error", {});
    this.emit("close", { reason });
  }

  open(): void {
    this.readyState = 1;
    this.emit("open");
  }

  receive(message: unknown): void {
    this.emit("message", { data: JSON.stringify(message) });
  }

  private emit(type: string, event?: unknown): void {
    for (const listener of this.listeners.get(type) ?? []) listener(event);
  }
}

function fixture() {
  const sockets: FakeSocket[] = [];
  let nextId = 0;
  const client = new WakuClient({
    address: "127.0.0.1:4312",
    token: "secret",
    randomUUID: () => `00000000-0000-4000-8000-${String(++nextId).padStart(12, "0")}`,
    webSocketFactory: () => {
      const socket = new FakeSocket();
      sockets.push(socket);
      return socket;
    },
  });
  return { client, sockets };
}

async function connect(client: WakuClient, sockets: FakeSocket[]): Promise<FakeSocket> {
  const connected = client.connect();
  const socket = sockets.at(-1)!;
  socket.open();
  socket.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
  await connected;
  return socket;
}

describe("WakuClient", () => {
  test("reports connection state changes, including remote closure", async () => {
    const { client, sockets } = fixture();
    const states: string[] = [];
    const unsubscribe = client.subscribeConnectionState((state) => states.push(state));

    const connected = client.connect();
    const socket = sockets[0]!;
    socket.open();
    socket.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
    await connected;
    socket.close();
    socket.emitClose();
    unsubscribe();

    expect(states).toEqual(["disconnected", "connecting", "connected", "disconnected"]);
  });

  test("authenticates and correlates typed responses", async () => {
    const { client, sockets } = fixture();
    const connected = client.connect();
    const socket = sockets[0]!;
    socket.open();
    expect(JSON.parse(socket.sent[0]!)).toEqual({
      type: "hello",
      protocolVersion: PROTOCOL_VERSION,
      token: "secret",
      clientId: "00000000-0000-4000-8000-000000000001",
      resumeFrom: [],
    });
    socket.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
    await connected;

    const response = client.request({ type: "getSettings" });
    const request = JSON.parse(socket.sent[1]!);
    socket.receive({
      type: "response",
      requestId: request.requestId,
      outcome: { status: "ok", payload: { type: "ack" } },
    });
    await expect(response).resolves.toEqual({ type: "ack" });
  });

  test("keeps workspace file responses distinct from task result records", async () => {
    const { client, sockets } = fixture();
    const socket = await connect(client, sockets);
    const response = client.request({
      type: "workspace",
      operation: { type: "readTextFile", root: "/test", relative_path: "result.txt" },
    });
    const request = JSON.parse(socket.sent.at(-1)!);
    socket.receive({
      type: "response", requestId: request.requestId,
      outcome: { status: "ok", payload: { type: "workspace", result: { type: "textFile", content: "retained file" } } },
    });
    const payload = await response;
    if (payload.type !== "workspace" || payload.result.type !== "textFile") {
      throw new Error("expected a workspace text file");
    }
    expect(payload.result.content).toBe("retained file");
    client.disconnect();
  });

  test("surfaces daemon errors", async () => {
    const { client, sockets } = fixture();
    const socket = sockets[0] ?? new FakeSocket();
    const connected = client.connect();
    const active = sockets[0] ?? socket;
    active.open();
    active.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
    await connected;

    const response = client.request({ type: "getSettings" });
    const request = JSON.parse(active.sent[1]!);
    active.receive({
      type: "response",
      requestId: request.requestId,
      outcome: { status: "error", error: { message: "nope" } },
    });
    await expect(response).rejects.toBeInstanceOf(WakuRpcError);
  });

  test("deduplicates events and resumes from the last sequence", async () => {
    const { client, sockets } = fixture();
    const firstConnection = client.connect();
    const first = sockets[0]!;
    first.open();
    first.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
    await firstConnection;

    const received: number[] = [];
    client.subscribe("session", "runtime", (event) => received.push(event.sequence), { epoch: "epoch-one", sequence: 3 });
    const event = {
      type: "event",
      sessionId: "session",
      runtimeId: "runtime",
      epoch: "epoch-one",
      sequence: 4,
      event: { kind: "textDelta", payload: { text: "hi" } },
    };
    first.receive(event);
    first.receive(event);
    expect(received).toEqual([4]);

    client.disconnect();
    const secondConnection = client.connect();
    const second = sockets[1]!;
    second.open();
    expect(JSON.parse(second.sent[0]!).resumeFrom).toEqual([
      { sessionId: "session", runtimeId: "runtime", epoch: "epoch-one", sequence: 4 },
    ]);
    second.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
    await secondConnection;
  });

  test("reconnect finishes an interrupted replay without new provider events", async () => {
    const { client, sockets } = fixture();
    const socket = await connect(client, sockets);
    const received: number[] = [];
    client.subscribe("session", "runtime", (event) => received.push(event.sequence));
    const wire = (sequence: number) => ({ sessionId: "session", runtimeId: "runtime", epoch: "epoch", sequence, event: { kind: "textDelta", payload: "x" } });
    socket.receive({ type: "event", ...wire(5) });
    expect(received).toEqual([]);
    expect(JSON.parse(socket.sent.at(-1)!).command.type).toBe("replayEvents");
    client.disconnect();
    await Promise.resolve();
    const replacement = await connect(client, sockets);
    const request = JSON.parse(replacement.sent.at(-1)!);
    expect(request.command).toEqual({ type: "replayEvents", cursor: { sessionId: "session", runtimeId: "runtime", epoch: "epoch", sequence: 0 } });
    replacement.receive({ type: "response", requestId: request.requestId, outcome: { status: "ok", payload: { type: "eventReplay", events: [1, 2, 3, 4].map(wire) } } });
    await Promise.resolve();
    expect(received).toEqual([1, 2, 3, 4, 5]);
    replacement.receive({ type: "event", ...wire(5) });
    replacement.receive({ type: "event", ...wire(6) });
    expect(received).toEqual([1, 2, 3, 4, 5, 6]);
    client.disconnect();
  });

  test("pruned replay replaces history and skips overlapping live events", async () => {
    const { client, sockets } = fixture();
    const socket = await connect(client, sockets);
    const received: Array<[string, number]> = [];
    client.subscribe("session", "runtime", (event) => received.push([event.event.kind, event.sequence]));
    const wire = (sequence: number) => ({ sessionId: "session", runtimeId: "runtime", epoch: "epoch", sequence, event: { kind: "textDelta", payload: "x" } });
    socket.receive({ type: "event", ...wire(20_005) });
    socket.receive({ type: "event", ...wire(20_006) });
    const request = JSON.parse(socket.sent.at(-1)!);
    socket.receive({ type: "response", requestId: request.requestId, outcome: { status: "ok", payload: {
      type: "historySnapshot",
      session: { id: "session", messages: [{ content: "complete history" }], history_saved_cursor: { runtime_id: "runtime", epoch: "epoch", sequence: 20_006 } },
    } } });
    await Promise.resolve();
    socket.receive({ type: "event", ...wire(20_007) });
    expect(received).toEqual([["historySnapshot", 20_006], ["textDelta", 20_007]]);
    client.disconnect();
  });

  test("snapshot chunks replace history only after a complete ordered transfer", async () => {
    const { client, sockets } = fixture();
    const socket = await connect(client, sockets);
    const cursor = { runtime_id: "runtime", epoch: "epoch", sequence: 20_005 };
    const session = { id: "session", history_saved_cursor: cursor, runtime_event_cursor: cursor, messages: [{ content: "保留历史" }] };
    const serialized = JSON.stringify(session);
    const parts = [serialized.slice(0, 60), serialized.slice(60)];
    let resolved: unknown;
    const pending = client.request({ type: "replayEvents", cursor: { sessionId: "session", runtimeId: "runtime", epoch: "epoch", sequence: 0 } }).then((value) => { resolved = value; });
    const requestId = JSON.parse(socket.sent.at(-1)!).requestId;
    const totalBytes = new TextEncoder().encode(serialized).length;
    socket.receive({ type: "historySnapshotChunk", replay: true, requestId, sessionId: "session", cursor, offset: 0, totalBytes, data: parts[0] });
    await Promise.resolve();
    expect(resolved).toBeUndefined();
    socket.receive({ type: "historySnapshotChunk", replay: true, requestId, sessionId: "session", cursor, offset: new TextEncoder().encode(parts[0]).length, totalBytes, data: parts[1] });
    await Promise.resolve();
    expect(resolved).toEqual({ type: "historySnapshot", session });
    await pending;
    client.disconnect();
  });

  test("snapshot chunks reject gaps and changed transfer identities", async () => {
    for (const change of [{ offset: 2 }, { totalBytes: 11 }, { sessionId: "other" }, { cursor: { runtime_id: "other", epoch: "epoch", sequence: 1 } }]) {
      const { client, sockets } = fixture();
      const socket = await connect(client, sockets);
      const pending = client.request({ type: "replayEvents", cursor: { sessionId: "session", runtimeId: "runtime", epoch: "epoch", sequence: 0 } });
      const requestId = JSON.parse(socket.sent.at(-1)!).requestId;
      const chunk = { type: "historySnapshotChunk", replay: true, requestId, sessionId: "session", cursor: { runtime_id: "runtime", epoch: "epoch", sequence: 1 }, offset: 0, totalBytes: 10, data: "{" };
      socket.receive(chunk);
      socket.receive({ ...chunk, offset: 1, ...change });
      await expect(pending).rejects.toThrow("invalid history snapshot chunk");
      client.disconnect();
    }
  });

  test("pruned terminal event still releases the runtime after snapshot recovery", async () => {
    const { client, sockets } = fixture();
    const socket = await connect(client, sockets);
    const received: string[] = [];
    client.subscribe("session", "runtime", (event) => received.push(event.event.kind));
    socket.receive({ type: "event", sessionId: "session", runtimeId: "runtime", epoch: "epoch", sequence: 20_005, event: { kind: "processExited", payload: null } });
    const request = JSON.parse(socket.sent.at(-1)!);
    socket.receive({ type: "response", requestId: request.requestId, outcome: { status: "ok", payload: { type: "historySnapshot", session: { id: "session", history_saved_cursor: { runtime_id: "runtime", epoch: "epoch", sequence: 20_005 } } } } });
    await Promise.resolve();
    expect(received).toEqual(["historySnapshot", "processExited"]);
    client.disconnect();
  });

  test("saved hot-window pruning restores the complete stream on subscription", async () => {
    const { client, sockets } = fixture();
    const socket = await connect(client, sockets);
    const wire = (sequence: number) => ({ sessionId: "session", runtimeId: "runtime", epoch: "epoch", sequence, event: { kind: "textDelta", payload: "x" } });
    for (let sequence = 1; sequence <= 5_000; sequence++) {
      socket.receive({ type: "event", ...wire(sequence) });
      if (sequence % 100 === 0) socket.receive({ type: "historyPersistence", sessionId: "session", runtimeId: "runtime", epoch: "epoch", sequence, error: null });
    }
    const received: number[] = [];
    client.subscribe("session", "runtime", (event) => { if (event.event.kind !== "historyPersistence") received.push(event.sequence); });
    const answered = new Set<string>();
    for (let pass = 0; pass < 20 && received.length < 5_000; pass++) {
      const request = JSON.parse(socket.sent.at(-1)!);
      if (request.command?.type === "replayEvents" && !answered.has(request.requestId)) {
        answered.add(request.requestId);
        const after = request.command.cursor.sequence;
        socket.receive({ type: "response", requestId: request.requestId, outcome: { status: "ok", payload: { type: "eventReplay", events: Array.from({ length: Math.min(512, 5_000 - after) }, (_, index) => wire(after + index + 1)) } } });
      }
      await Promise.resolve();
    }
    expect(received.length).toBe(5_000);
    expect(received[0]).toBe(1);
    expect(received.at(-1)).toBe(5_000);
    expect(new Set(received).size).toBe(5_000);
    client.disconnect();
  });

  test("disconnect rejects an in-flight handshake and permits reconnecting", async () => {
    const { client, sockets } = fixture();
    const firstConnection = client.connect();
    client.disconnect();
    await expect(firstConnection).rejects.toThrow("Waku client disconnected");

    const secondConnection = client.connect();
    const second = sockets[1]!;
    second.open();
    second.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
    await expect(secondConnection).resolves.toBeUndefined();
  });

  test("surfaces the native socket failure reason", async () => {
    const { client, sockets } = fixture();
    const connection = client.connect();

    sockets[0]!.fail("The network connection was lost");

    await expect(connection).rejects.toThrow("The network connection was lost");
  });

  test("accepts sequence one again when the daemon epoch changes", async () => {
    const { client, sockets } = fixture();
    const socket = await connect(client, sockets);
    const received: Array<[string, number]> = [];
    client.subscribe("session", "runtime", (event) => {
      received.push([event.epoch, event.sequence]);
    }, { epoch: "old", sequence: 8 });

    socket.receive({
      type: "event",
      sessionId: "session",
      runtimeId: "runtime",
      epoch: "old",
      sequence: 9,
      event: { kind: "textDelta", payload: null },
    });
    socket.receive({
      type: "event",
      sessionId: "session",
      runtimeId: "runtime",
      epoch: "new",
      sequence: 1,
      event: { kind: "textDelta", payload: null },
    });

    expect(received).toEqual([
      ["old", 9],
      ["new", 1],
    ]);
  });

  test("buffers replayed events until a refreshed app attaches to the runtime", async () => {
    const { client, sockets } = fixture();
    const socket = await connect(client, sockets);

    socket.receive({
      type: "event",
      sessionId: "session",
      runtimeId: "runtime",
      epoch: "epoch",
      sequence: 1,
      event: { kind: "textDelta", payload: "before attach" },
    });
    socket.receive({
      type: "event",
      sessionId: "session",
      runtimeId: "runtime",
      epoch: "epoch",
      sequence: 2,
      event: { kind: "textDelta", payload: "still before attach" },
    });

    const received: number[] = [];
    client.subscribe("session", "runtime", (event) => received.push(event.sequence));
    expect(received).toEqual([1, 2]);
  });

  test("notifies connected apps when another client changes task state", async () => {
    const { client, sockets } = fixture();
    const socket = await connect(client, sockets);
    const revisions: number[] = [];
    client.subscribeTaskState((revision) => revisions.push(revision));

    socket.receive({ type: "taskStateChanged", revision: 7 });
    expect(revisions).toEqual([7]);
  });

  test("disconnected requests reject instead of throwing synchronously", async () => {
    const { client } = fixture();
    const request = client.request({ type: "getSettings" });
    await expect(request).rejects.toThrow("Waku daemon is disconnected");
  });

  test("disconnected notifications reject instead of throwing synchronously", async () => {
    const { client } = fixture();
    const notification = client.notify({ type: "refreshBackgroundWork" });
    await expect(notification).rejects.toThrow("Waku daemon is disconnected");
  });

  test("notifications use the response-free nil request id", async () => {
    const { client, sockets } = fixture();
    const socket = await connect(client, sockets);

    await client.notify(
      { type: "writeTerminal", data: "bHM=" },
      "terminal",
      "terminal",
    );

    expect(JSON.parse(socket.sent[1]!)).toEqual({
      type: "request",
      requestId: "00000000-0000-0000-0000-000000000000",
      sessionId: "terminal",
      runtimeId: "terminal",
      command: { type: "writeTerminal", data: "bHM=" },
    });
  });
});

test("daemonUrl pins the versioned endpoint", () => {
  expect(daemonUrl("localhost:3030/anything?old=1")).toBe("ws://localhost:3030/v1");
  expect(daemonUrl("wss://waku.example.test")).toBe("wss://waku.example.test/v1");
});

describe("WakuClient connection failures", () => {
  async function failure(setup: (socket: FakeSocket) => void): Promise<WakuConnectionError> {
    const { client, sockets } = fixture();
    const connected = client.connect();
    setup(sockets[0]!);
    const error = await connected.catch((cause: unknown) => cause);
    expect(error).toBeInstanceOf(WakuConnectionError);
    return error as WakuConnectionError;
  }

  test("types handshake failures so callers can tell what needs a person", async () => {
    const rejected = await failure((socket) => {
      socket.open();
      socket.receive({ type: "rejected", message: "authentication failed" });
    });
    expect(rejected.kind).toBe("rejected");
    expect(rejected.retryable).toBe(false);
    expect(rejected.message).toBe("daemon rejected connection: authentication failed");

    const unsupported = await failure((socket) => {
      socket.open();
      socket.receive({ type: "rejected", message: "protocol 3 is unsupported; expected 7" });
    });
    expect(unsupported.kind).toBe("protocol");

    const mismatch = await failure((socket) => {
      socket.open();
      socket.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION + 1, daemonVersion: "x" });
    });
    expect(mismatch.kind).toBe("protocol");
    expect(mismatch.retryable).toBe(false);

    const garbage = await failure((socket) => {
      socket.open();
      socket.receive({ type: "event", sessionId: "x" });
    });
    expect(garbage.kind).toBe("handshake");

    const refused = await failure((socket) => socket.fail("Connection refused"));
    expect(refused.kind).toBe("unreachable");
    expect(refused.retryable).toBe(true);
    expect(refused.message).toBe("Connection refused");
  });

  test("times out a handshake the daemon never answers", async () => {
    const sockets: FakeSocket[] = [];
    const client = new WakuClient({
      address: "127.0.0.1:4312",
      token: "secret",
      connectTimeoutMs: 1,
      randomUUID: () => "00000000-0000-4000-8000-000000000001",
      webSocketFactory: () => {
        const socket = new FakeSocket();
        sockets.push(socket);
        return socket;
      },
    });
    const error = (await client.connect().catch((cause: unknown) => cause)) as WakuConnectionError;
    expect(error.kind).toBe("timeout");
    expect(error.retryable).toBe(true);
    expect(client.connectionState).toBe("disconnected");
    expect(sockets[0]!.readyState).toBe(3);

    // The abandoned socket's late events are ignored and a fresh attempt works.
    sockets[0]!.open();
    const again = client.connect();
    expect(sockets).toHaveLength(2);
    sockets[1]!.open();
    sockets[1]!.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
    await again;
    expect(client.connected).toBe(true);
  });

  test("lets a request shorten its own timeout", async () => {
    const { client, sockets } = fixture();
    await connect(client, sockets);
    await expect(
      client.request({ type: "getSettings" }, undefined, undefined, { timeoutMs: 1 }),
    ).rejects.toThrow("timed out waiting for Waku daemon");
  });

  test("settles requests and clears the socket before listeners hear a remote close", async () => {
    const { client, sockets } = fixture();
    const socket = await connect(client, sockets);
    const request = client.request({ type: "getSettings" });
    let reconnected: Promise<void> | null = null;
    client.subscribeConnectionState((state) => {
      if (state === "disconnected" && !reconnected) reconnected = client.connect();
    });

    socket.fail("Software caused connection abort");
    const error = (await request.catch((cause: unknown) => cause)) as WakuConnectionError;
    expect(error.kind).toBe("closed");
    expect(client.lastDisconnectReason).toBe("Software caused connection abort");
    expect(sockets).toHaveLength(2);

    sockets[1]!.open();
    sockets[1]!.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
    await reconnected!;
    expect(client.connected).toBe(true);
    const next = client.request({ type: "getSettings" });
    expect(sockets[1]!.sent).toHaveLength(2);
    const sent = JSON.parse(sockets[1]!.sent[1]!) as { requestId: string };
    sockets[1]!.receive({
      type: "response",
      requestId: sent.requestId,
      outcome: { status: "ok", payload: { type: "ack" } },
    });
    await expect(next).resolves.toEqual({ type: "ack" });
  });

  test("records when the daemon last spoke", async () => {
    const sockets: FakeSocket[] = [];
    let now = 41;
    const client = new WakuClient({
      address: "127.0.0.1:4312",
      token: "secret",
      now: () => now,
      randomUUID: () => "00000000-0000-4000-8000-000000000001",
      webSocketFactory: () => {
        const socket = new FakeSocket();
        sockets.push(socket);
        return socket;
      },
    });
    expect(client.lastMessageAt).toBe(0);
    const connected = client.connect();
    now = 42;
    sockets[0]!.open();
    sockets[0]!.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
    await connected;
    expect(client.lastMessageAt).toBe(42);
    now = 43;
    sockets[0]!.receive({ type: "taskStateChanged", revision: 1 });
    expect(client.lastMessageAt).toBe(43);
  });
});
