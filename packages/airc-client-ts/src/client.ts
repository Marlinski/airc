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
import { type IrcEvent, type ChannelMessage, type ChannelStatus, MessageKind, newChannelMessage, newChannelMessageWithTs } from "./event.js";
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
const RPL_LOGGEDIN = 900;
const RPL_SASLSUCCESS = 903;
const ERR_SASLFAIL = 904;
const ERR_SASLABORTED = 906;

// ---------------------------------------------------------------------------
// IRCv3 optional capabilities we request (server-advertised intersection)
// ---------------------------------------------------------------------------

const OPTIONAL_CAPS: readonly string[] = [
  "message-tags",
  "server-time",
  "echo-message",
  "away-notify",
  "multi-prefix",
  "extended-join",
  "account-notify",
];

// ---------------------------------------------------------------------------
// SASL state
// ---------------------------------------------------------------------------

interface SaslState {
  account: string;
  password: string;
  mechanism: "PLAIN" | null;
  step: "awaiting_cap_ack" | "awaiting_challenge" | "awaiting_success" | "done";
  loggedIn: boolean;
}

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
  private _saslState: SaslState | null = null;
  private _negotiatedCaps: Set<string> = new Set();

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

      // Initialise SASL state for this connection attempt.
      this._negotiatedCaps = new Set();
      this._saslState = this._config.password
        ? {
            account: this._config.saslAccount ?? this._config.nick,
            password: this._config.password,
            mechanism: null,
            step: "awaiting_cap_ack",
            loggedIn: false,
          }
        : null;

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
        // Send CAP negotiation before NICK/USER so the server holds
        // registration until we send CAP END.
        this._sendRaw("CAP LS 302");
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
          if (!this._destroyed) {
            reject(new Error("connection closed before registration"));
          }
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

  /** The set of IRCv3 capabilities successfully negotiated with the server. */
  negotiatedCaps(): Set<string> {
    return new Set(this._negotiatedCaps);
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

    // -- CAP (capability negotiation) --
    if (command.kind === "named" && command.name === "CAP") {
      const subcommand = (msg.params[1] ?? "").toUpperCase();

      if (subcommand === "LS") {
        // params: [target, "LS", ["*",] caplist]
        // Multi-line LS (302) uses "*" as the third param; wait for the final
        // line (no "*"). We process each line as it arrives — requesting caps
        // on the final line only (no "*" marker).
        const isMultiLine = msg.params.length >= 4 && msg.params[2] === "*";
        if (isMultiLine) return; // wait for the final LS line

        const capList = msg.params[msg.params.length - 1] ?? "";
        const advertised = new Set(
          capList.split(/\s+/).filter((c) => c.length > 0).map((c) => c.split("=")[0].toLowerCase()),
        );

        // Build the intersection of optional caps the server advertises,
        // minus any caps the caller has opted out of via disableCaps.
        const disabled = new Set(this._config.disableCaps.map((c) => c.toLowerCase()));
        const optionalToRequest = OPTIONAL_CAPS.filter(
          (c) => advertised.has(c) && !disabled.has(c),
        );

        const hasSasl = advertised.has("sasl");

        if (this._config.password && hasSasl && this._saslState) {
          // Request SASL first; optional caps in a second REQ.
          this._saslState.mechanism = "PLAIN";
          this._sendRaw("CAP REQ :sasl");
          if (optionalToRequest.length > 0) {
            this._sendRaw(`CAP REQ :${optionalToRequest.join(" ")}`);
          }
        } else if (optionalToRequest.length > 0) {
          // No SASL — request optional caps and then end.
          this._sendRaw(`CAP REQ :${optionalToRequest.join(" ")}`);
        } else {
          // Nothing to negotiate.
          this._sendRaw("CAP END");
        }
        return;
      }

      if (subcommand === "ACK") {
        const acked = (msg.params[msg.params.length - 1] ?? "").toLowerCase().split(/\s+/);
        for (const cap of acked) {
          if (cap.length > 0) this._negotiatedCaps.add(cap);
        }

        if (acked.includes("sasl") && this._saslState) {
          this._saslState.step = "awaiting_challenge";
          this._sendRaw("AUTHENTICATE PLAIN");
        } else if (!this._negotiatedCaps.has("sasl") || this._saslState?.step === "done") {
          // All non-SASL REQs acknowledged — end negotiation.
          // (SASL flow will send CAP END via 903 handler.)
          if (!this._saslState || this._saslState.step === "done") {
            this._sendRaw("CAP END");
          }
        }
        return;
      }

      if (subcommand === "NAK") {
        // NAK on optional caps is silently ignored — the server just won't
        // send those features. Only emit sasl_failed if SASL itself was NAK'd.
        const nakked = (msg.params[msg.params.length - 1] ?? "").toLowerCase();
        if (nakked.includes("sasl") && this._saslState) {
          const reason = msg.params[msg.params.length - 1] ?? "capability rejected";
          this._emit({ type: "sasl_failed", code: 0, reason });
          this._sendRaw("CAP END");
        }
        // For optional caps NAK: if there's no in-flight SASL, we may need to
        // end negotiation — but only if we have nothing else pending.
        if (!this._saslState || this._saslState.step === "done") {
          this._sendRaw("CAP END");
        }
        return;
      }

      return;
    }

    // -- AUTHENTICATE --
    if (command.kind === "named" && command.name === "AUTHENTICATE") {
      const payload = msg.params[0] ?? "";
      if (payload === "+" && this._saslState && this._saslState.step === "awaiting_challenge") {
        const { account, password } = this._saslState;
        // SASL PLAIN: \0<authcid>\0<passwd>
        const authStr = `\0${account}\0${password}`;
        const bytes = new Uint8Array(authStr.length);
        for (let i = 0; i < authStr.length; i++) {
          bytes[i] = authStr.charCodeAt(i);
        }
        const encoded = btoa(String.fromCharCode(...Array.from(bytes)));
        this._sendRaw(`AUTHENTICATE ${encoded}`);
        this._saslState.step = "awaiting_success";
      }
      return;
    }

    // -- RPL_LOGGEDIN (900) --
    if (command.kind === "numeric" && command.code === RPL_LOGGEDIN) {
      const account = msg.params[2] ?? msg.params[1] ?? "";
      if (this._saslState) {
        this._saslState.loggedIn = true;
      }
      this._emit({ type: "sasl_logged_in", account });
      return;
    }

    // -- RPL_SASLSUCCESS (903) --
    if (command.kind === "numeric" && command.code === RPL_SASLSUCCESS) {
      if (this._saslState) {
        this._saslState.step = "done";
      }
      this._sendRaw("CAP END");
      return;
    }

    // -- ERR_SASLFAIL (904) / ERR_SASLABORTED (906) --
    if (
      command.kind === "numeric" &&
      (command.code === ERR_SASLFAIL || command.code === ERR_SASLABORTED)
    ) {
      const reason = msg.params[msg.params.length - 1] ?? "SASL authentication failed";
      this._emit({ type: "sasl_failed", code: command.code, reason });
      this._sendRaw("CAP END");
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

      // NickServ fallback: send IDENTIFY if password is set but SASL either
      // wasn't used or didn't log us in.
      if (this._config.password && (this._saslState === null || !this._saslState.loggedIn)) {
        this._sendRaw(IrcMessage.privmsg("NickServ", `IDENTIFY ${this._config.password}`).serialize());
      }
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
        const channel = msg.params[1];
        const topic = msg.params[2];
        this._state.setTopic(channel, topic);
        this._emit({ type: "topic", channel, topic });
      }
      return;
    }

    // -- NAMES reply (353) --
    if (command.kind === "numeric" && command.code === RPL_NAMREPLY) {
      if (msg.params.length >= 4) {
        const channel = msg.params[2];
        const namesStr = msg.params[3];
        // Strip all leading prefix chars (multi-prefix: @+nick, ~&nick, etc.)
        const members = namesStr
          .split(/\s+/)
          .filter((n) => n.length > 0)
          .map((n) => n.replace(/^[@+%~&]+/, ""));
        // Ensure channel entry exists — NAMES can arrive before our JOIN is processed.
        this._state.joinChannel(channel);
        this._state.setMembers(channel, members);
        // Emit structured event so consumers don't need to parse raw lines.
        this._emit({ type: "names", channel, members });
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
      // params[0] = channel; extended-join adds params[1]=account and
      // params[2]=realname — we read only the channel and ignore the rest.
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

        // Use server-time tag when available (requires server-time / message-tags cap).
        const serverTs = msg.tag("time") ?? undefined;
        const cm = newChannelMessageWithTs(target, from, text, kind, serverTs);

        if (isChannelName(target)) {
          this._state.pushMessage(target, cm);
        } else {
          this._state.pushPrivateMessage(cm);
        }

        this._emit({ type: "message", message: cm });
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
        const serverTs = msg.tag("time") ?? undefined;
        const cm = newChannelMessageWithTs(target, from, text, MessageKind.Normal, serverTs);
        if (isChannelName(target)) {
          this._state.pushMessage(target, cm);
        } else {
          this._state.pushPrivateMessage(cm);
        }
      }

      this._emit({ type: "notice", from, target, text });
      return;
    }

    // -- AWAY (away-notify) --
    // Sent by the server to channel members when a peer sets or clears away.
    // `params[0]` is the away message; absent means the user returned from away.
    if (command.kind === "named" && command.name === "AWAY") {
      const nick = extractNick(msg.prefix);
      const message = msg.params[0]; // undefined = back from away
      this._emit({ type: "away", nick, message });
      return;
    }

    // -- ACCOUNT (account-notify) --
    // Sent by the server when a user's NickServ account changes mid-session.
    // `params[0] === "*"` means the user logged out.
    if (command.kind === "named" && command.name === "ACCOUNT") {
      const nick = extractNick(msg.prefix);
      const raw = msg.params[0] ?? "*";
      const account = raw === "*" ? undefined : raw;
      this._emit({ type: "account_notify", nick, account });
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

      // Initialise SASL state for this reconnect attempt.
      this._negotiatedCaps = new Set();
      this._saslState = this._config.password
        ? {
            account: this._config.saslAccount ?? this._config.nick,
            password: this._config.password,
            mechanism: null,
            step: "awaiting_cap_ack",
            loggedIn: false,
          }
        : null;

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
        // Send CAP negotiation before NICK/USER.
        ws.send("CAP LS 302");
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
