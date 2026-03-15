/**
 * @airc/client — TypeScript IRC client library for AIRC.
 *
 * Connects to aircd over WebSocket and provides a typed API for
 * channel operations, message fetching, and real-time events.
 *
 * @example
 * ```ts
 * import { AircClient } from "@airc/client";
 *
 * const client = new AircClient({ url: "ws://localhost:8080/ws", nick: "mybot" });
 * const { motd } = await client.connect();
 * client.join("#lobby");
 * client.say("#lobby", "Hello from an agent!");
 * const msgs = client.fetch("#lobby");
 * ```
 *
 * @packageDocumentation
 */

// Protocol layer
export { type NamedCommand, type Command, parseCommand, serializeCommand, isNamed, isNumeric } from "./command.js";
export { Prefix, extractNick } from "./prefix.js";
export { IrcMessage, ParseError, type ParseErrorKind } from "./message.js";

// Config
export { type ClientConfig, resolveConfig, DEFAULT_URL } from "./config.js";

// Events & state
export {
  type IrcEvent,
  type ChannelMessage,
  type ChannelStatus,
  MessageKind,
  newChannelMessage,
} from "./event.js";
export { ClientState, type ChannelState } from "./state.js";

// Reconnect
export { type ReconnectConfig, type ReconnectParams, Backoff, resolveReconnectConfig } from "./reconnect.js";

// Client
export { AircClient, type EventListener } from "./client.js";
