/**
 * AircClient — high-level IRC client over WebSocket.
 *
 * Mirrors the Rust `IrcClient` from `airc-client/src/client.rs`.
 *
 * Features:
 * - WebSocket transport (text frames, one IRC message per frame)
 * - Typed event emitter with `.on()` / `.off()`
 * - Auto-reconnect with exponential backoff
 * - Message buffering with read cursors (fetch/fetchAll/fetchLast)
 * - Auto PONG, nick collision handling, MOTD collection
 *
 * Zero runtime dependencies — uses the standard `WebSocket` API.
 */

import type { ClientConfig } from "./config.js";
import { resolveConfig } from "./config.js";
import { IrcMessage } from "./message.js";
import { extractNick } from "./prefix.js";
import { type IrcEvent, type ChannelMessage, type ChannelStatus, MessageKind, newChannelMessage } from "./event.js";
import { ClientState } from "./state.js";
import { Backoff, type ReconnectConfig, resolveReconnectConfig } from "./reconnect.js";

// ---------------------------------------------------------------------------
// Numeric constants (subset we handle in the client)
// ---------------------------------------------------------------------------

const RPL_WELCOME = 1;
const RPL_TOPIC = 332;
const RPL_NAMREPLY = 353;
const RPL_MOTDSTART = 375;
const RPL_MOTD = 372;
const RPL_ENDOFMOTD = 376;
const ERR_NICKNAMEINUSE = 433;

// ---------------------------------------------------------------------------
// Event emitter types
// ---------------------------------------------------------------------------

/** Callback type for event listeners. */
export type EventListener = (event: IrcEvent) => void;

// ---------------------------------------------------------------------------
// AircClient
// ---------------------------------------------------------------------------

export class AircClient {
  private _config: ReturnType<typeof resolveConfig>;
  private _state: ClientState;
  private _ws: WebSocket | null = null;
  private _connected = false;
  private _listeners: EventListener[] = [];
  private _sendQueue: string[] = [];
  private _reconnectConfig: ReturnType<typeof resolveReconnectConfig>;
  private _autoReconnect = true;
  private _destroyed = false;

  constructor(config: ClientConfig, reconnect?: ReconnectConfig) {
    this._config = resolveConfig(config);
    this._state = new ClientState(this._config.nick, this._config.bufferSize);
    this._reconnectConfig = resolveReconnectConfig(reconnect);
  }

  // -- Connection -----------------------------------------------------------

  /**
   * Connect to the IRC server over WebSocket.
   *
   * Returns a promise that resolves once RPL_WELCOME (001) is received
   * (registration complete), or rejects on connection error / timeout.
   *
   * The returned MOTD lines are collected during the registration window.
   */
  connect(): Promise<{ motd: string[] }> {
    if (this._destroyed) {
      return Promise.reject(new Error("client has been destroyed"));
    }
    if (this._ws) {
      return Promise.reject(new Error("already connected"));
    }

    return new Promise((resolve, reject) => {
      let settled = false;
      const motdLines: string[] = [];
      let motdDone = false;
      let registered = false;

      const ws = new WebSocket(this._config.url);
      this._ws = ws;

      // Registration timeout.
      const timeout = setTimeout(() => {
        if (!settled) {
          settled = true;
          ws.close();
          this._ws = null;
          reject(new Error("registration timed out"));
        }
      }, 10_000);

      ws.addEventListener("open", () => {
        // Send registration sequence.
        if (this._config.password) {
          this._sendRaw(IrcMessage.pass(this._config.password).serialize());
        }
        this._sendRaw(IrcMessage.nick(this._config.nick).serialize());
        this._sendRaw(IrcMessage.user(this._config.username, this._config.realname).serialize());
      });

      ws.addEventListener("message", (ev) => {
        const line = typeof ev.data === "string" ? ev.data : "";
        this._handleLine(line, {
          onRegistered: () => {
            registered = true;
          },
          onMotd: (l) => {
            if (!motdDone) motdLines.push(l);
          },
          onMotdEnd: () => {
            motdDone = true;
            if (!settled) {
              settled = true;
              clearTimeout(timeout);
              this._connected = true;
              // Auto-join channels.
              for (const channel of this._config.autoJoin) {
                this._sendRaw(IrcMessage.join(channel).serialize());
              }
              // Flush send queue.
              this._flushQueue();
              resolve({ motd: motdLines });
            }
          },
        });

        // If registered but no MOTD comes within a short window, resolve anyway.
        if (registered && !settled) {
          // Give MOTD 2 seconds after registration.
          setTimeout(() => {
            if (!settled) {
              settled = true;
              clearTimeout(timeout);
              this._connected = true;
              for (const channel of this._config.autoJoin) {
                this._sendRaw(IrcMessage.join(channel).serialize());
              }
              this._flushQueue();
              resolve({ motd: motdLines });
            }
          }, 2000);
        }
      });

      ws.addEventListener("error", () => {
        if (!settled) {
          settled = true;
          clearTimeout(timeout);
          this._ws = null;
          reject(new Error("WebSocket connection failed"));
        }
      });

      ws.addEventListener("close", () => {
        this._connected = false;
        this._ws = null;

        if (!settled) {
          settled = true;
          clearTimeout(timeout);
          reject(new Error("connection closed before registration"));
        } else {
          // Connection lost after successful connect — emit event and
          // start auto-reconnect.
          this._emit({ type: "disconnected", reason: "connection closed" });
          if (this._autoReconnect && !this._destroyed) {
            this._reconnectLoop();
          }
        }
      });
    });
  }

  /**
   * Disconnect from the server. Sends QUIT and closes the WebSocket.
   * Disables auto-reconnect.
   */
  async quit(reason?: string): Promise<void> {
    this._autoReconnect = false;
    if (this._ws) {
      this._sendRaw(IrcMessage.quit(reason).serialize());
      this._ws.close();
      this._ws = null;
    }
    this._connected = false;
  }

  /**
   * Permanently destroy the client. Closes the connection and prevents
   * any further reconnection attempts.
   */
  destroy(): void {
    this._destroyed = true;
    this._autoReconnect = false;
    if (this._ws) {
      this._ws.close();
      this._ws = null;
    }
    this._connected = false;
    this._listeners = [];
  }

  // -- Channel operations ---------------------------------------------------

  /** Join an IRC channel. */
  join(channel: string): void {
    this._send(IrcMessage.join(channel).serialize());
  }

  /** Leave an IRC channel. */
  part(channel: string, reason?: string): void {
    this._send(IrcMessage.part(channel, reason).serialize());
  }

  /** Send a message to a channel or user. */
  say(target: string, text: string): void {
    this._send(IrcMessage.privmsg(target, text).serialize());
  }

  /** Send a notice to a channel or user. */
  notice(target: string, text: string): void {
    this._send(IrcMessage.notice(target, text).serialize());
  }

  // -- Message fetching (the key agent UX) ----------------------------------

  /** Fetch unread messages from a specific channel. */
  fetch(channel: string): ChannelMessage[] {
    return this._state.fetch(channel);
  }

  /** Fetch unread messages from all channels, sorted by timestamp. */
  fetchAll(): ChannelMessage[] {
    return this._state.fetchAll();
  }

  /** Fetch the last N messages from a channel and mark all as read. */
  fetchLast(channel: string, n: number): ChannelMessage[] {
    return this._state.fetchLast(channel, n);
  }

  /** Fetch the last N messages from ALL channels and mark all as read. */
  fetchLastAll(n: number): ChannelMessage[] {
    return this._state.fetchLastAll(n);
  }

  // -- NickServ helpers -----------------------------------------------------

  /** Identify with NickServ. */
  nickservIdentify(password: string): void {
    this.say("NickServ", `IDENTIFY ${password}`);
  }

  /** Register with NickServ. */
  nickservRegister(password: string): void {
    this.say("NickServ", `REGISTER ${password}`);
  }

  // -- Status / queries -----------------------------------------------------

  /** Get our current nickname. */
  nick(): string {
    return this._state.nick();
  }

  /** Get the list of joined channels. */
  channels(): string[] {
    return this._state.channels();
  }

  /** Get status summary: channels, unread counts, member counts. */
  status(): ChannelStatus[] {
    return this._state.status();
  }

  /** Whether we're registered with the server. */
  isRegistered(): boolean {
    return this._state.isRegistered();
  }

  /** Whether the client is currently connected. */
  isConnected(): boolean {
    return this._connected;
  }

  /** Access the underlying state for advanced queries. */
  state(): ClientState {
    return this._state;
  }

  // -- Event emitter --------------------------------------------------------

  /** Subscribe to IRC events. */
  on(listener: EventListener): void {
    this._listeners.push(listener);
  }

  /** Unsubscribe from IRC events. */
  off(listener: EventListener): void {
    const idx = this._listeners.indexOf(listener);
    if (idx >= 0) this._listeners.splice(idx, 1);
  }

  // -- Low-level -------------------------------------------------------------

  /** Send a raw IRC line. If disconnected, the line is queued. */
  sendLine(line: string): void {
    this._send(line);
  }

  // -- Private ---------------------------------------------------------------

  private _emit(event: IrcEvent): void {
    for (const listener of this._listeners) {
      try {
        listener(event);
      } catch {
        // Don't let listener errors crash the client.
      }
    }
  }

  /** Send on the WebSocket, or queue if not connected. */
  private _send(line: string): void {
    if (this._connected && this._ws) {
      this._sendRaw(line);
    } else {
      this._sendQueue.push(line);
    }
  }

  /** Send directly on the WebSocket (no queueing). */
  private _sendRaw(line: string): void {
    if (this._ws && this._ws.readyState === WebSocket.OPEN) {
      this._ws.send(line);
    }
  }

  /** Flush the send queue after reconnect. */
  private _flushQueue(): void {
    while (this._sendQueue.length > 0) {
      const line = this._sendQueue.shift()!;
      this._sendRaw(line);
    }
  }

  /**
   * Handle a single incoming IRC line.
   *
   * The optional `hooks` parameter lets `connect()` intercept registration
   * and MOTD events during the initial connection handshake.
   */
  private _handleLine(
    rawLine: string,
    hooks?: {
      onRegistered?: () => void;
      onMotd?: (line: string) => void;
      onMotdEnd?: () => void;
    },
  ): void {
    const trimmed = rawLine.replace(/\r?\n$/, "");
    if (trimmed.length === 0) return;

    let msg: IrcMessage;
    try {
      msg = IrcMessage.parse(trimmed);
    } catch {
      // Unparseable line — ignore.
      return;
    }

    const { command } = msg;

    // -- Auto PONG --
    if (command.kind === "named" && command.name === "PING") {
      const token = msg.params[0] ?? "";
      this._sendRaw(IrcMessage.pong(token).serialize());
      return;
    }

    // -- Registration complete (001) --
    if (command.kind === "numeric" && command.code === RPL_WELCOME) {
      const nick = msg.params[0] ?? "";
      const server = msg.prefix ?? "";
      const message = msg.params[msg.params.length - 1] ?? "";
      this._state.setNick(nick);
      this._state.setServerName(server);
      this._state.setRegistered();
      const event: IrcEvent = { type: "registered", nick, server, message };
      this._emit(event);
      hooks?.onRegistered?.();
      return;
    }

    // -- Nick in use (433) --
    if (command.kind === "numeric" && command.code === ERR_NICKNAMEINUSE) {
      const current = this._state.nick();
      const newNick = current + "_";
      this._state.setNick(newNick);
      this._sendRaw(IrcMessage.nick(newNick).serialize());
      return;
    }

    // -- Topic (332) --
    if (command.kind === "numeric" && command.code === RPL_TOPIC) {
      if (msg.params.length >= 3) {
        this._state.setTopic(msg.params[1], msg.params[2]);
      }
      return;
    }

    // -- NAMES reply (353) --
    if (command.kind === "numeric" && command.code === RPL_NAMREPLY) {
      if (msg.params.length >= 4) {
        const channel = msg.params[2];
        const namesStr = msg.params[3];
        const members = namesStr
          .split(/\s+/)
          .filter((n) => n.length > 0)
          .map((n) => n.replace(/^[@+%]/, ""));
        this._state.setMembers(channel, members);
      }
      return;
    }

    // -- MOTD (375, 372, 376) --
    if (command.kind === "numeric" && command.code === RPL_MOTDSTART) {
      return; // Nothing to emit.
    }
    if (command.kind === "numeric" && command.code === RPL_MOTD) {
      let line = msg.params[msg.params.length - 1] ?? "";
      if (line.startsWith("- ")) line = line.slice(2);
      this._emit({ type: "motd", line });
      hooks?.onMotd?.(line);
      return;
    }
    if (command.kind === "numeric" && command.code === RPL_ENDOFMOTD) {
      this._emit({ type: "motd_end" });
      hooks?.onMotdEnd?.();
      return;
    }

    // -- JOIN --
    if (command.kind === "named" && command.name === "JOIN") {
      const channel = msg.params[0] ?? "";
      const nick = extractNick(msg.prefix);
      const ourNick = this._state.nick();

      if (nick.toLowerCase() === ourNick.toLowerCase()) {
        this._state.joinChannel(channel);
      } else {
        this._state.addMember(channel, nick);
      }
      this._emit({ type: "join", nick, channel });
      return;
    }

    // -- PART --
    if (command.kind === "named" && command.name === "PART") {
      const channel = msg.params[0] ?? "";
      const reason = msg.params[1];
      const nick = extractNick(msg.prefix);
      const ourNick = this._state.nick();

      if (nick.toLowerCase() === ourNick.toLowerCase()) {
        this._state.partChannel(channel);
      } else {
        this._state.removeMember(channel, nick);
      }
      this._emit({ type: "part", nick, channel, reason });
      return;
    }

    // -- QUIT --
    if (command.kind === "named" && command.name === "QUIT") {
      const nick = extractNick(msg.prefix);
      const reason = msg.params[0];
      this._state.removeMemberAll(nick);
      this._emit({ type: "quit", nick, reason });
      return;
    }

    // -- KICK --
    if (command.kind === "named" && command.name === "KICK") {
      if (msg.params.length >= 2) {
        const channel = msg.params[0];
        const kicked = msg.params[1];
        const reason = msg.params[2];
        const by = extractNick(msg.prefix);
        const ourNick = this._state.nick();

        if (kicked.toLowerCase() === ourNick.toLowerCase()) {
          this._state.partChannel(channel);
        } else {
          this._state.removeMember(channel, kicked);
        }
        this._emit({ type: "kick", channel, nick: kicked, by, reason });
      }
      return;
    }

    // -- NICK change --
    if (command.kind === "named" && command.name === "NICK") {
      const oldNick = extractNick(msg.prefix);
      const newNick = msg.params[0] ?? "";
      const ourNick = this._state.nick();

      if (oldNick.toLowerCase() === ourNick.toLowerCase()) {
        this._state.setNick(newNick);
      }
      this._state.renameMember(oldNick, newNick);
      this._emit({ type: "nick_change", oldNick, newNick });
      return;
    }

    // -- TOPIC change --
    if (command.kind === "named" && command.name === "TOPIC") {
      const channel = msg.params[0] ?? "";
      const topic = msg.params[1] ?? "";
      const setBy = extractNick(msg.prefix);
      this._state.setTopic(channel, topic);
      this._emit({ type: "topic_change", channel, topic, setBy });
      return;
    }

    // -- PRIVMSG --
    if (command.kind === "named" && command.name === "PRIVMSG") {
      if (msg.params.length >= 2) {
        const target = msg.params[0];
        let text = msg.params[1];
        const from = extractNick(msg.prefix);

        // Detect CTCP ACTION.
        let kind = MessageKind.Normal;
        if (text.startsWith("\x01ACTION ") && text.endsWith("\x01")) {
          text = text.slice(8, text.length - 1);
          kind = MessageKind.Action;
        }

        const cm = newChannelMessage(target, from, text, kind);

        if (isChannelName(target)) {
          this._state.pushMessage(target, cm);
        } else {
          this._state.pushPrivateMessage(cm);
        }

        this._emit({ type: "message", message: newChannelMessage(target, from, text, kind) });
      }
      return;
    }

    // -- NOTICE --
    if (command.kind === "named" && command.name === "NOTICE") {
      const target = msg.params[0] ?? "";
      const text = msg.params[1] ?? "";
      const from = msg.prefix ? extractNick(msg.prefix) : undefined;

      // Buffer notices from service bots as messages too.
      if (from) {
        const cm = newChannelMessage(target, from, text, MessageKind.Normal);
        if (isChannelName(target)) {
          this._state.pushMessage(target, cm);
        } else {
          this._state.pushPrivateMessage(cm);
        }
      }

      this._emit({ type: "notice", from, target, text });
      return;
    }

    // -- Everything else: emit as Raw --
    this._emit({ type: "raw", line: msg.serialize() });
  }

  /** Auto-reconnect loop with exponential backoff. */
  private async _reconnectLoop(): Promise<void> {
    const backoff = new Backoff(this._reconnectConfig);

    // Remember which channels we were in.
    const channelsToRejoin = this._state.channels();

    while (!this._destroyed && this._autoReconnect) {
      this._emit({ type: "reconnecting", attempt: backoff.attempt + 1 });
      await backoff.wait();

      try {
        await this._reconnectOnce();

        // Re-join channels.
        for (const ch of channelsToRejoin) {
          this._sendRaw(IrcMessage.join(ch).serialize());
        }

        // Flush the send queue.
        this._flushQueue();

        backoff.reset();
        this._emit({ type: "reconnected" });
        return; // The new WS close handler will trigger another reconnect if needed.
      } catch {
        // Retry on next iteration.
      }
    }
  }

  /** Single reconnect attempt — connects and waits for registration. */
  private _reconnectOnce(): Promise<void> {
    return new Promise((resolve, reject) => {
      let settled = false;

      const ws = new WebSocket(this._config.url);
      this._ws = ws;

      const timeout = setTimeout(() => {
        if (!settled) {
          settled = true;
          ws.close();
          this._ws = null;
          reject(new Error("reconnect registration timed out"));
        }
      }, 10_000);

      ws.addEventListener("open", () => {
        if (this._config.password) {
          ws.send(IrcMessage.pass(this._config.password).serialize());
        }
        ws.send(IrcMessage.nick(this._config.nick).serialize());
        ws.send(IrcMessage.user(this._config.username, this._config.realname).serialize());
      });

      ws.addEventListener("message", (ev) => {
        const line = typeof ev.data === "string" ? ev.data : "";
        this._handleLine(line, {
          onRegistered: () => {
            if (!settled) {
              settled = true;
              clearTimeout(timeout);
              this._connected = true;
              resolve();
            }
          },
        });
      });

      ws.addEventListener("error", () => {
        if (!settled) {
          settled = true;
          clearTimeout(timeout);
          this._ws = null;
          reject(new Error("reconnect failed"));
        }
      });

      ws.addEventListener("close", () => {
        this._connected = false;

        if (!settled) {
          settled = true;
          clearTimeout(timeout);
          this._ws = null;
          reject(new Error("connection closed during reconnect"));
        } else {
          // Post-reconnect close — handle normally.
          this._ws = null;
          this._emit({ type: "disconnected", reason: "connection closed" });
          if (this._autoReconnect && !this._destroyed) {
            this._reconnectLoop();
          }
        }
      });
    });
  }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Check if a target looks like a channel name (starts with #, &, +, or !). */
function isChannelName(target: string): boolean {
  return target.length > 0 && "#&+!".includes(target[0]);
}
