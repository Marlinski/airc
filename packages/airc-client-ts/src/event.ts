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
