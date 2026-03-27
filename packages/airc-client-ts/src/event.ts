/**
 * IRC events — typed representations of incoming messages.
 *
 * Mirrors the Rust `IrcEvent` enum from `airc-client/src/event.rs`.
 * Uses a discriminated union on the `type` field.
 */

// ---------------------------------------------------------------------------
// Message types (from proto)
// ---------------------------------------------------------------------------

/** Matches the proto `MessageKind` enum. */
export const enum MessageKind {
  Normal = 0,
  Action = 1,
}

/** A chat message (channel or private) — matches proto `ChannelMessage`. */
export interface ChannelMessage {
  /** Channel name or nick (DM recipient). */
  target: string;
  /** Sender nick. */
  from: string;
  /** Message body. */
  text: string;
  /** Message kind. */
  kind: MessageKind;
  /** Unix timestamp (seconds). */
  timestamp: number;
}

/** Summary of a joined channel's state — matches proto `ChannelStatus`. */
export interface ChannelStatus {
  name: string;
  topic?: string;
  members: number;
  totalMessages: number;
  unread: number;
}

// ---------------------------------------------------------------------------
// IrcEvent discriminated union
// ---------------------------------------------------------------------------

export type IrcEvent =
  | { type: "registered"; nick: string; server: string; message: string }
  | { type: "message"; message: ChannelMessage }
  | { type: "join"; nick: string; channel: string }
  | { type: "part"; nick: string; channel: string; reason?: string }
  | { type: "quit"; nick: string; reason?: string }
  | { type: "kick"; channel: string; nick: string; by: string; reason?: string }
  | { type: "topic_change"; channel: string; topic: string; setBy: string }
  | { type: "nick_change"; oldNick: string; newNick: string }
  | { type: "notice"; from?: string; target: string; text: string }
  | { type: "disconnected"; reason: string }
  | { type: "reconnecting"; attempt: number }
  | { type: "reconnected" }
  | { type: "motd"; line: string }
  | { type: "motd_end" }
  | { type: "sasl_logged_in"; account: string }
  | { type: "sasl_failed"; code: number; reason: string }
  /**
   * A user in a shared channel set or cleared their away status (away-notify).
   * `message` is `undefined` when the user returned from away.
   */
  | { type: "away"; nick: string; message: string | undefined }
  /**
   * A user's NickServ account changed while already connected (account-notify).
   * `account` is `undefined` when the user logged out (`ACCOUNT *`).
   */
  | { type: "account_notify"; nick: string; account: string | undefined }
  /**
   * Initial member list for a channel, from 353 RPL_NAMREPLY.
   * `members` includes mode prefixes (@, +, %, &, ~) if present.
   * Emitted once per 353 line; multiple lines may arrive for large channels.
   */
  | { type: "names"; channel: string; members: string[] }
  /**
   * Topic set at join time, from 332 RPL_TOPIC.
   * Distinct from `topic_change` which is the live TOPIC command mid-session.
   */
  | { type: "topic"; channel: string; topic: string }
  | { type: "raw"; line: string };

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Create a new `ChannelMessage` with the current timestamp. */
export function newChannelMessage(
  target: string,
  from: string,
  text: string,
  kind: MessageKind,
): ChannelMessage {
  return {
    target,
    from,
    text,
    kind,
    timestamp: Math.floor(Date.now() / 1000),
  };
}

/**
 * Create a new `ChannelMessage` using a server-supplied timestamp when
 * available, falling back to the current system time.
 *
 * `serverTs` should be an ISO 8601 string from the `@time=` tag, or
 * `null`/`undefined` if no tag was present.
 */
export function newChannelMessageWithTs(
  target: string,
  from: string,
  text: string,
  kind: MessageKind,
  serverTs: string | null | undefined,
): ChannelMessage {
  let timestamp = Math.floor(Date.now() / 1000);
  if (serverTs) {
    const parsed = Date.parse(serverTs);
    if (!isNaN(parsed)) {
      timestamp = Math.floor(parsed / 1000);
    }
  }
  return { target, from, text, kind, timestamp };
}
