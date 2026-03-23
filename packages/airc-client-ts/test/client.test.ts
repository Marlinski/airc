/**
 * Tests for AircClient — integration tests with mock WebSocket.
 *
 * Uses a minimal WebSocket mock to test the client's message handling,
 * state updates, and event emission without a real server.
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { AircClient } from "../src/client.js";
import type { IrcEvent } from "../src/event.js";

// ===========================================================================
// Mock WebSocket
// ===========================================================================

type WSListener = (ev: { data: string }) => void;
type WSVoidListener = () => void;

class MockWebSocket {
  static CONNECTING = 0;
  static OPEN = 1;
  static CLOSING = 2;
  static CLOSED = 3;

  readyState = MockWebSocket.CONNECTING;
  url: string;
  sent: string[] = [];

  private _messageListeners: WSListener[] = [];
  private _openListeners: WSVoidListener[] = [];
  private _closeListeners: WSVoidListener[] = [];
  private _errorListeners: WSVoidListener[] = [];

  constructor(url: string) {
    this.url = url;
    // Auto-open in microtask.
    queueMicrotask(() => {
      this.readyState = MockWebSocket.OPEN;
      for (const fn of this._openListeners) fn();
    });
  }

  send(data: string): void {
    this.sent.push(data);
  }

  close(): void {
    this.readyState = MockWebSocket.CLOSED;
    for (const fn of this._closeListeners) fn();
  }

  addEventListener(type: string, listener: (...args: any[]) => void): void {
    switch (type) {
      case "message":
        this._messageListeners.push(listener);
        break;
      case "open":
        this._openListeners.push(listener);
        break;
      case "close":
        this._closeListeners.push(listener);
        break;
      case "error":
        this._errorListeners.push(listener);
        break;
    }
  }

  // Test helpers — inject server messages.
  _injectMessage(line: string): void {
    for (const fn of this._messageListeners) {
      fn({ data: line });
    }
  }

  _injectError(): void {
    for (const fn of this._errorListeners) fn();
  }
}

// Install mock globally.
let lastWs: MockWebSocket | null = null;

beforeEach(() => {
  lastWs = null;
  (globalThis as any).WebSocket = class extends MockWebSocket {
    constructor(url: string) {
      super(url);
      lastWs = this;
    }
  };
  // Also expose static constants.
  (globalThis as any).WebSocket.OPEN = MockWebSocket.OPEN;
  (globalThis as any).WebSocket.CONNECTING = MockWebSocket.CONNECTING;
  (globalThis as any).WebSocket.CLOSING = MockWebSocket.CLOSING;
  (globalThis as any).WebSocket.CLOSED = MockWebSocket.CLOSED;
});

afterEach(() => {
  delete (globalThis as any).WebSocket;
});

/** Helper to wait for macrotasks/microtasks. */
function tick(ms = 10): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

// ===========================================================================
// Tests
// ===========================================================================

describe("AircClient", () => {
  it("connects, registers, and collects MOTD", async () => {
    const client = new AircClient({
      url: "ws://localhost:8080/ws",
      nick: "testbot",
    });

    const connectPromise = client.connect();

    // Wait for WS to open and registration to be sent.
    await tick();

    expect(lastWs).not.toBeNull();
    expect(lastWs!.sent).toContain("NICK testbot");
    expect(lastWs!.sent.some((s) => s.startsWith("USER testbot"))).toBe(true);

    // Server sends registration reply + MOTD.
    lastWs!._injectMessage(":irc.example.com 001 testbot :Welcome to IRC");
    lastWs!._injectMessage(":irc.example.com 375 testbot :- irc.example.com MOTD -");
    lastWs!._injectMessage(":irc.example.com 372 testbot :- Welcome to AIRC!");
    lastWs!._injectMessage(":irc.example.com 372 testbot :- Enjoy your stay.");
    lastWs!._injectMessage(":irc.example.com 376 testbot :End of MOTD");

    const result = await connectPromise;
    expect(result.motd).toEqual(["Welcome to AIRC!", "Enjoy your stay."]);
    expect(client.nick()).toBe("testbot");
    expect(client.isConnected()).toBe(true);
    expect(client.isRegistered()).toBe(true);

    client.destroy();
  });

  it("sends CAP LS before NICK/USER when password is configured", async () => {
    const client = new AircClient({
      url: "ws://localhost:8080/ws",
      nick: "testbot",
      password: "secret",
    });

    const connectPromise = client.connect();
    await tick();

    // CAP LS 302 must be the first thing sent (before NICK/USER).
    expect(lastWs!.sent[0]).toBe("CAP LS 302");
    expect(lastWs!.sent).toContain("NICK testbot");
    // PASS is not sent — authentication uses SASL or NickServ IDENTIFY.
    expect(lastWs!.sent).not.toContain("PASS secret");

    lastWs!._injectMessage(":irc.example.com 001 testbot :Welcome");
    lastWs!._injectMessage(":irc.example.com 376 testbot :End of MOTD");

    await connectPromise;
    client.destroy();
  });

  it("auto-joins channels after registration", async () => {
    const client = new AircClient({
      url: "ws://localhost:8080/ws",
      nick: "testbot",
      autoJoin: ["#lobby", "#dev"],
    });

    const connectPromise = client.connect();
    await tick();

    lastWs!._injectMessage(":irc.example.com 001 testbot :Welcome");
    lastWs!._injectMessage(":irc.example.com 376 testbot :End of MOTD");

    await connectPromise;

    expect(lastWs!.sent).toContain("JOIN #lobby");
    expect(lastWs!.sent).toContain("JOIN #dev");

    client.destroy();
  });

  it("handles PING/PONG automatically", async () => {
    const client = new AircClient({
      url: "ws://localhost:8080/ws",
      nick: "testbot",
    });

    const connectPromise = client.connect();
    await tick();

    lastWs!._injectMessage(":irc.example.com 001 testbot :Welcome");
    lastWs!._injectMessage(":irc.example.com 376 testbot :End of MOTD");
    await connectPromise;

    lastWs!._injectMessage("PING :token123");
    expect(lastWs!.sent).toContain("PONG token123");

    client.destroy();
  });

  it("handles nick collision (433)", async () => {
    const client = new AircClient({
      url: "ws://localhost:8080/ws",
      nick: "testbot",
    });

    const connectPromise = client.connect();
    await tick();

    // Server says nick is in use.
    lastWs!._injectMessage(":irc.example.com 433 * testbot :Nickname is already in use");

    // Client should try testbot_.
    expect(lastWs!.sent).toContain("NICK testbot_");

    // Now server accepts.
    lastWs!._injectMessage(":irc.example.com 001 testbot_ :Welcome");
    lastWs!._injectMessage(":irc.example.com 376 testbot_ :End of MOTD");

    await connectPromise;
    expect(client.nick()).toBe("testbot_");

    client.destroy();
  });

  it("emits events to listeners", async () => {
    const client = new AircClient({
      url: "ws://localhost:8080/ws",
      nick: "testbot",
    });

    const events: IrcEvent[] = [];
    client.on((ev) => events.push(ev));

    const connectPromise = client.connect();
    await tick();

    lastWs!._injectMessage(":irc.example.com 001 testbot :Welcome");
    lastWs!._injectMessage(":irc.example.com 376 testbot :End of MOTD");
    await connectPromise;

    // Simulate someone joining.
    lastWs!._injectMessage(":alice!user@host JOIN #lobby");
    expect(events.some((e) => e.type === "join" && e.nick === "alice")).toBe(true);

    // Simulate a message.
    lastWs!._injectMessage(":bob!user@host PRIVMSG #lobby :hello world");
    const msgEvent = events.find((e) => e.type === "message");
    expect(msgEvent).toBeDefined();
    if (msgEvent?.type === "message") {
      expect(msgEvent.message.from).toBe("bob");
      expect(msgEvent.message.text).toBe("hello world");
    }

    client.destroy();
  });

  it("tracks channel state from JOIN/PART", async () => {
    const client = new AircClient({
      url: "ws://localhost:8080/ws",
      nick: "testbot",
    });

    const connectPromise = client.connect();
    await tick();

    lastWs!._injectMessage(":irc.example.com 001 testbot :Welcome");
    lastWs!._injectMessage(":irc.example.com 376 testbot :End of MOTD");
    await connectPromise;

    // Our JOIN.
    lastWs!._injectMessage(":testbot!user@host JOIN #lobby");
    expect(client.channels()).toContain("#lobby");

    // Someone else joins.
    lastWs!._injectMessage(":alice!user@host JOIN #lobby");

    // NAMES reply.
    lastWs!._injectMessage(":irc.example.com 353 testbot = #lobby :testbot @alice");
    const s = client.status();
    const lobby = s.find((c) => c.name === "#lobby");
    expect(lobby).toBeDefined();
    expect(lobby!.members).toBe(2);

    // Our PART.
    lastWs!._injectMessage(":testbot!user@host PART #lobby :bye");
    expect(client.channels()).not.toContain("#lobby");

    client.destroy();
  });

  it("buffers messages and supports fetch", async () => {
    const client = new AircClient({
      url: "ws://localhost:8080/ws",
      nick: "testbot",
    });

    const connectPromise = client.connect();
    await tick();

    lastWs!._injectMessage(":irc.example.com 001 testbot :Welcome");
    lastWs!._injectMessage(":irc.example.com 376 testbot :End of MOTD");
    await connectPromise;

    // Join channel and receive messages.
    lastWs!._injectMessage(":testbot!user@host JOIN #lobby");
    lastWs!._injectMessage(":alice!u@h PRIVMSG #lobby :msg1");
    lastWs!._injectMessage(":bob!u@h PRIVMSG #lobby :msg2");

    const msgs = client.fetch("#lobby");
    expect(msgs).toHaveLength(2);
    expect(msgs[0].text).toBe("msg1");
    expect(msgs[1].text).toBe("msg2");

    // Second fetch returns nothing.
    expect(client.fetch("#lobby")).toHaveLength(0);

    client.destroy();
  });

  it("handles TOPIC change", async () => {
    const client = new AircClient({
      url: "ws://localhost:8080/ws",
      nick: "testbot",
    });

    const events: IrcEvent[] = [];
    client.on((ev) => events.push(ev));

    const connectPromise = client.connect();
    await tick();

    lastWs!._injectMessage(":irc.example.com 001 testbot :Welcome");
    lastWs!._injectMessage(":irc.example.com 376 testbot :End of MOTD");
    await connectPromise;

    lastWs!._injectMessage(":testbot!user@host JOIN #lobby");
    lastWs!._injectMessage(":admin!u@h TOPIC #lobby :New topic!");

    const topicEvent = events.find((e) => e.type === "topic_change");
    expect(topicEvent).toBeDefined();
    if (topicEvent?.type === "topic_change") {
      expect(topicEvent.topic).toBe("New topic!");
      expect(topicEvent.setBy).toBe("admin");
    }

    // Topic reflected in status.
    const lobby = client.status().find((c) => c.name === "#lobby");
    expect(lobby?.topic).toBe("New topic!");

    client.destroy();
  });

  it("handles NICK change", async () => {
    const client = new AircClient({
      url: "ws://localhost:8080/ws",
      nick: "testbot",
    });

    const connectPromise = client.connect();
    await tick();

    lastWs!._injectMessage(":irc.example.com 001 testbot :Welcome");
    lastWs!._injectMessage(":irc.example.com 376 testbot :End of MOTD");
    await connectPromise;

    // Our nick changes.
    lastWs!._injectMessage(":testbot!u@h NICK newbot");
    expect(client.nick()).toBe("newbot");

    client.destroy();
  });

  it("handles KICK", async () => {
    const client = new AircClient({
      url: "ws://localhost:8080/ws",
      nick: "testbot",
    });

    const events: IrcEvent[] = [];
    client.on((ev) => events.push(ev));

    const connectPromise = client.connect();
    await tick();

    lastWs!._injectMessage(":irc.example.com 001 testbot :Welcome");
    lastWs!._injectMessage(":irc.example.com 376 testbot :End of MOTD");
    await connectPromise;

    lastWs!._injectMessage(":testbot!user@host JOIN #lobby");
    // We get kicked.
    lastWs!._injectMessage(":admin!u@h KICK #lobby testbot :Bad behavior");

    expect(client.channels()).not.toContain("#lobby");
    const kickEvent = events.find((e) => e.type === "kick");
    expect(kickEvent).toBeDefined();
    if (kickEvent?.type === "kick") {
      expect(kickEvent.nick).toBe("testbot");
      expect(kickEvent.by).toBe("admin");
      expect(kickEvent.reason).toBe("Bad behavior");
    }

    client.destroy();
  });

  it("handles QUIT from other users", async () => {
    const client = new AircClient({
      url: "ws://localhost:8080/ws",
      nick: "testbot",
    });

    const events: IrcEvent[] = [];
    client.on((ev) => events.push(ev));

    const connectPromise = client.connect();
    await tick();

    lastWs!._injectMessage(":irc.example.com 001 testbot :Welcome");
    lastWs!._injectMessage(":irc.example.com 376 testbot :End of MOTD");
    await connectPromise;

    lastWs!._injectMessage(":testbot!user@host JOIN #lobby");
    lastWs!._injectMessage(":irc.example.com 353 testbot = #lobby :testbot alice");
    lastWs!._injectMessage(":alice!u@h QUIT :Leaving");

    const quitEvent = events.find((e) => e.type === "quit");
    expect(quitEvent).toBeDefined();
    if (quitEvent?.type === "quit") {
      expect(quitEvent.nick).toBe("alice");
      expect(quitEvent.reason).toBe("Leaving");
    }

    // alice removed from members.
    const lobby = client.status().find((c) => c.name === "#lobby");
    expect(lobby?.members).toBe(1);

    client.destroy();
  });

  it("detects CTCP ACTION in PRIVMSG", async () => {
    const client = new AircClient({
      url: "ws://localhost:8080/ws",
      nick: "testbot",
    });

    const events: IrcEvent[] = [];
    client.on((ev) => events.push(ev));

    const connectPromise = client.connect();
    await tick();

    lastWs!._injectMessage(":irc.example.com 001 testbot :Welcome");
    lastWs!._injectMessage(":irc.example.com 376 testbot :End of MOTD");
    await connectPromise;

    lastWs!._injectMessage(":testbot!user@host JOIN #lobby");
    lastWs!._injectMessage(":alice!u@h PRIVMSG #lobby :\x01ACTION waves\x01");

    const msgEvent = events.find((e) => e.type === "message");
    expect(msgEvent).toBeDefined();
    if (msgEvent?.type === "message") {
      expect(msgEvent.message.text).toBe("waves");
      expect(msgEvent.message.kind).toBe(1); // Action
    }

    client.destroy();
  });

  it("queues messages when disconnected", async () => {
    const client = new AircClient({
      url: "ws://localhost:8080/ws",
      nick: "testbot",
    });

    // Send before connecting — should be queued.
    client.say("#lobby", "queued message");

    const connectPromise = client.connect();
    await tick();

    lastWs!._injectMessage(":irc.example.com 001 testbot :Welcome");
    lastWs!._injectMessage(":irc.example.com 376 testbot :End of MOTD");
    await connectPromise;

    // The queued message should have been flushed.
    expect(lastWs!.sent).toContain("PRIVMSG #lobby :queued message");

    client.destroy();
  });

  it("sendLine sends raw IRC line", async () => {
    const client = new AircClient({
      url: "ws://localhost:8080/ws",
      nick: "testbot",
    });

    const connectPromise = client.connect();
    await tick();

    lastWs!._injectMessage(":irc.example.com 001 testbot :Welcome");
    lastWs!._injectMessage(":irc.example.com 376 testbot :End of MOTD");
    await connectPromise;

    client.sendLine("WHOIS alice");
    expect(lastWs!.sent).toContain("WHOIS alice");

    client.destroy();
  });

  it("off removes listener", async () => {
    const client = new AircClient({
      url: "ws://localhost:8080/ws",
      nick: "testbot",
    });

    const events: IrcEvent[] = [];
    const listener = (ev: IrcEvent) => events.push(ev);
    client.on(listener);

    const connectPromise = client.connect();
    await tick();

    lastWs!._injectMessage(":irc.example.com 001 testbot :Welcome");
    lastWs!._injectMessage(":irc.example.com 376 testbot :End of MOTD");
    await connectPromise;

    client.off(listener);
    lastWs!._injectMessage(":alice!u@h JOIN #lobby");

    // Should not see the JOIN event (only registration events before off).
    expect(events.every((e) => e.type !== "join")).toBe(true);

    client.destroy();
  });
});

// ===========================================================================
// IRCv3 capability tests
// ===========================================================================

describe("IRCv3 capabilities", () => {
  async function connectedClient(opts: { password?: string } = {}): Promise<AircClient> {
    const client = new AircClient({ url: "ws://localhost:8080/ws", nick: "testbot", ...opts });
    const p = client.connect();
    await tick();
    // Minimal welcome to resolve connect().
    lastWs!._injectMessage(":irc.example.com 001 testbot :Welcome");
    lastWs!._injectMessage(":irc.example.com 376 testbot :End of MOTD");
    await p;
    return client;
  }

  it("requests optional caps from CAP LS advertisement", async () => {
    const client = new AircClient({ url: "ws://localhost:8080/ws", nick: "testbot" });
    const p = client.connect();
    await tick();

    // Server advertises several caps.
    lastWs!._injectMessage(
      ":irc.example.com CAP testbot LS :message-tags server-time echo-message away-notify multi-prefix extended-join account-notify",
    );
    await tick();

    // Client should have sent a CAP REQ for the intersection.
    const reqs = lastWs!.sent.filter((s) => s.startsWith("CAP REQ"));
    expect(reqs.length).toBeGreaterThanOrEqual(1);
    const allReqs = reqs.join(" ");
    expect(allReqs).toContain("message-tags");
    expect(allReqs).toContain("server-time");
    expect(allReqs).toContain("away-notify");
    expect(allReqs).toContain("account-notify");

    client.destroy();
  });

  it("only requests advertised optional caps (intersection)", async () => {
    const client = new AircClient({ url: "ws://localhost:8080/ws", nick: "testbot" });
    const p = client.connect();
    await tick();

    // Server only advertises two caps.
    lastWs!._injectMessage(":irc.example.com CAP testbot LS :server-time away-notify");
    await tick();

    const reqs = lastWs!.sent.filter((s) => s.startsWith("CAP REQ"));
    const allReqs = reqs.join(" ");
    expect(allReqs).toContain("server-time");
    expect(allReqs).toContain("away-notify");
    expect(allReqs).not.toContain("echo-message");

    client.destroy();
  });

  it("sends CAP END when server advertises no optional caps", async () => {
    const client = new AircClient({ url: "ws://localhost:8080/ws", nick: "testbot" });
    const p = client.connect();
    await tick();

    lastWs!._injectMessage(":irc.example.com CAP testbot LS :"); // empty
    await tick();

    expect(lastWs!.sent).toContain("CAP END");

    client.destroy();
  });

  it("records negotiated caps from CAP ACK", async () => {
    const client = new AircClient({ url: "ws://localhost:8080/ws", nick: "testbot" });
    const p = client.connect();
    await tick();

    lastWs!._injectMessage(":irc.example.com CAP testbot LS :message-tags server-time");
    await tick();
    lastWs!._injectMessage(":irc.example.com CAP testbot ACK :message-tags server-time");
    await tick();

    // Should send CAP END after ACK (no SASL in flight).
    expect(lastWs!.sent).toContain("CAP END");

    client.destroy();
  });

  it("uses @time= tag for PRIVMSG timestamp", async () => {
    const client = await connectedClient();
    const events: IrcEvent[] = [];
    client.on((e) => events.push(e));

    lastWs!._injectMessage(
      "@time=2026-01-01T12:00:00.000Z :alice!a@host PRIVMSG #lobby :hello",
    );
    await tick();

    const msg = events.find((e) => e.type === "message");
    expect(msg?.type).toBe("message");
    if (msg?.type === "message") {
      expect(msg.message.timestamp).toBe(1767268800); // 2026-01-01T12:00:00Z
    }

    client.destroy();
  });

  it("falls back to Date.now() when no @time= tag", async () => {
    const before = Math.floor(Date.now() / 1000);
    const client = await connectedClient();
    const events: IrcEvent[] = [];
    client.on((e) => events.push(e));

    lastWs!._injectMessage(":alice!a@host PRIVMSG #lobby :hello");
    await tick();

    const msg = events.find((e) => e.type === "message");
    expect(msg?.type).toBe("message");
    if (msg?.type === "message") {
      const after = Math.floor(Date.now() / 1000);
      expect(msg.message.timestamp).toBeGreaterThanOrEqual(before);
      expect(msg.message.timestamp).toBeLessThanOrEqual(after + 1);
    }

    client.destroy();
  });

  it("emits away event on AWAY message from server", async () => {
    const client = await connectedClient();
    const events: IrcEvent[] = [];
    client.on((e) => events.push(e));

    lastWs!._injectMessage(":alice!a@host AWAY :Gone for lunch");
    await tick();

    const ev = events.find((e) => e.type === "away");
    expect(ev?.type).toBe("away");
    if (ev?.type === "away") {
      expect(ev.nick).toBe("alice");
      expect(ev.message).toBe("Gone for lunch");
    }

    client.destroy();
  });

  it("emits away event with undefined message when user returns", async () => {
    const client = await connectedClient();
    const events: IrcEvent[] = [];
    client.on((e) => events.push(e));

    lastWs!._injectMessage(":alice!a@host AWAY");
    await tick();

    const ev = events.find((e) => e.type === "away");
    expect(ev?.type).toBe("away");
    if (ev?.type === "away") {
      expect(ev.nick).toBe("alice");
      expect(ev.message).toBeUndefined();
    }

    client.destroy();
  });

  it("emits account_notify event on ACCOUNT message", async () => {
    const client = await connectedClient();
    const events: IrcEvent[] = [];
    client.on((e) => events.push(e));

    lastWs!._injectMessage(":alice!a@host ACCOUNT alice_account");
    await tick();

    const ev = events.find((e) => e.type === "account_notify");
    expect(ev?.type).toBe("account_notify");
    if (ev?.type === "account_notify") {
      expect(ev.nick).toBe("alice");
      expect(ev.account).toBe("alice_account");
    }

    client.destroy();
  });

  it("emits account_notify with undefined account on ACCOUNT *", async () => {
    const client = await connectedClient();
    const events: IrcEvent[] = [];
    client.on((e) => events.push(e));

    lastWs!._injectMessage(":alice!a@host ACCOUNT *");
    await tick();

    const ev = events.find((e) => e.type === "account_notify");
    expect(ev?.type).toBe("account_notify");
    if (ev?.type === "account_notify") {
      expect(ev.nick).toBe("alice");
      expect(ev.account).toBeUndefined();
    }

    client.destroy();
  });

  it("strips all leading prefix chars in NAMES (multi-prefix)", async () => {
    const client = await connectedClient();
    const events: IrcEvent[] = [];
    client.on((e) => events.push(e));

    lastWs!._injectMessage(":irc.example.com 353 testbot = #lobby :@+alice ~bob &carol +dave testbot");
    await tick();

    const members = client.state().members("#lobby");
    expect(members).toContain("alice");
    expect(members).toContain("bob");
    expect(members).toContain("carol");
    expect(members).toContain("dave");
    expect(members).toContain("testbot");
    // None should have prefix chars remaining.
    for (const m of members) {
      expect(m).not.toMatch(/^[@+%~&]/);
    }

    client.destroy();
  });

  it("handles extended-join gracefully (extra params ignored)", async () => {
    const client = await connectedClient();
    const events: IrcEvent[] = [];
    client.on((e) => events.push(e));

    // extended-join: params[0]=channel, params[1]=account, params[2]=realname
    lastWs!._injectMessage(":alice!a@host JOIN #lobby alice_ns :Alice Real Name");
    await tick();

    const ev = events.find((e) => e.type === "join");
    expect(ev?.type).toBe("join");
    if (ev?.type === "join") {
      expect(ev.channel).toBe("#lobby");
      expect(ev.nick).toBe("alice");
    }

    client.destroy();
  });
});
