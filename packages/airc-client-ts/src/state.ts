/**
 * Client-side state tracking.
 *
 * Mirrors the Rust `ClientState` from `airc-client/src/state.rs`.
 * Tracks identity, joined channels, and buffered messages with read cursors.
 *
 * This is a synchronous in-memory store (no async locks needed in JS).
 */

import type { ChannelMessage, ChannelStatus } from "./event.js";

// ---------------------------------------------------------------------------
// ChannelState
// ---------------------------------------------------------------------------

/** State for a single channel. */
export interface ChannelState {
  /** Channel name (preserves original casing). */
  name: string;
  /** Current topic. */
  topic: string | undefined;
  /** Known members (nicks). */
  members: string[];
  /** Buffered messages (bounded ring). */
  messages: ChannelMessage[];
  /** Read cursor — index of the next unread message. */
  readCursor: number;
}

// ---------------------------------------------------------------------------
// ClientState
// ---------------------------------------------------------------------------

export class ClientState {
  /** Our current nick. */
  private _nick: string;
  /** The server name (from RPL_WELCOME). */
  private _serverName: string | undefined;
  /** Channels we've joined, keyed by lowercase name. */
  private _channels: Map<string, ChannelState> = new Map();
  /** Max messages to buffer per channel. */
  private _bufferSize: number;
  /** Whether we've completed registration. */
  private _registered = false;

  constructor(nick: string, bufferSize: number) {
    this._nick = nick;
    this._bufferSize = bufferSize;
  }

  // -- Identity -------------------------------------------------------------

  nick(): string {
    return this._nick;
  }

  setNick(nick: string): void {
    this._nick = nick;
  }

  serverName(): string | undefined {
    return this._serverName;
  }

  setServerName(name: string): void {
    this._serverName = name;
  }

  isRegistered(): boolean {
    return this._registered;
  }

  setRegistered(): void {
    this._registered = true;
  }

  // -- Channel management ---------------------------------------------------

  /** Record that we joined a channel. */
  joinChannel(name: string): void {
    const key = name.toLowerCase();
    if (!this._channels.has(key)) {
      this._channels.set(key, {
        name,
        topic: undefined,
        members: [],
        messages: [],
        readCursor: 0,
      });
    }
  }

  /** Record that we left a channel. */
  partChannel(name: string): void {
    this._channels.delete(name.toLowerCase());
  }

  /** Get the list of channels we're in. */
  channels(): string[] {
    return Array.from(this._channels.values()).map((c) => c.name);
  }

  /** Get the member list for a channel, or an empty array if not joined. */
  members(channel: string): string[] {
    return this._channels.get(channel.toLowerCase())?.members ?? [];
  }

  /** Set the topic for a channel. */
  setTopic(channel: string, topic: string): void {
    const ch = this._channels.get(channel.toLowerCase());
    if (ch) ch.topic = topic;
  }

  /** Set the member list for a channel (from NAMES reply). */
  setMembers(channel: string, members: string[]): void {
    const ch = this._channels.get(channel.toLowerCase());
    if (ch) ch.members = members;
  }

  /** Add a member to a channel. */
  addMember(channel: string, nick: string): void {
    const ch = this._channels.get(channel.toLowerCase());
    if (ch && !ch.members.some((n) => n.toLowerCase() === nick.toLowerCase())) {
      ch.members.push(nick);
    }
  }

  /** Remove a member from a channel. */
  removeMember(channel: string, nick: string): void {
    const ch = this._channels.get(channel.toLowerCase());
    if (ch) {
      ch.members = ch.members.filter((n) => n.toLowerCase() !== nick.toLowerCase());
    }
  }

  /** Remove a member from all channels (e.g., on QUIT). */
  removeMemberAll(nick: string): void {
    const lower = nick.toLowerCase();
    for (const ch of this._channels.values()) {
      ch.members = ch.members.filter((n) => n.toLowerCase() !== lower);
    }
  }

  /** Update nick in member lists when someone changes nick. */
  renameMember(oldNick: string, newNick: string): void {
    const lower = oldNick.toLowerCase();
    for (const ch of this._channels.values()) {
      for (let i = 0; i < ch.members.length; i++) {
        if (ch.members[i].toLowerCase() === lower) {
          ch.members[i] = newNick;
        }
      }
    }
  }

  // -- Message buffering ----------------------------------------------------

  /** Buffer an incoming message for a channel. */
  pushMessage(channel: string, msg: ChannelMessage): void {
    const key = channel.toLowerCase();
    const ch = this._channels.get(key);
    if (!ch) return;

    ch.messages.push(msg);

    // Trim to buffer size.
    while (ch.messages.length > this._bufferSize) {
      ch.messages.shift();
      // Adjust cursor if it pointed to a removed message.
      if (ch.readCursor > 0) {
        ch.readCursor--;
      }
    }
  }

  /**
   * Buffer a private message (not in a channel).
   * Stored under the sender's nick as the "channel" key.
   */
  pushPrivateMessage(msg: ChannelMessage): void {
    const key = msg.from.toLowerCase();
    let ch = this._channels.get(key);
    if (!ch) {
      ch = {
        name: msg.from,
        topic: undefined,
        members: [],
        messages: [],
        readCursor: 0,
      };
      this._channels.set(key, ch);
    }

    ch.messages.push(msg);

    while (ch.messages.length > this._bufferSize) {
      ch.messages.shift();
      if (ch.readCursor > 0) {
        ch.readCursor--;
      }
    }
  }

  // -- Fetch (the key agent UX) ---------------------------------------------

  /** Fetch unread messages for a channel (advances the read cursor). */
  fetch(channel: string): ChannelMessage[] {
    const ch = this._channels.get(channel.toLowerCase());
    if (!ch) return [];
    const unread = ch.messages.slice(ch.readCursor);
    ch.readCursor = ch.messages.length;
    return unread;
  }

  /** Fetch unread messages from ALL channels, sorted by timestamp. */
  fetchAll(): ChannelMessage[] {
    const all: ChannelMessage[] = [];
    for (const ch of this._channels.values()) {
      all.push(...ch.messages.slice(ch.readCursor));
      ch.readCursor = ch.messages.length;
    }
    all.sort((a, b) => a.timestamp - b.timestamp);
    return all;
  }

  /** Fetch the last N messages from a channel and mark all as read. */
  fetchLast(channel: string, n: number): ChannelMessage[] {
    const ch = this._channels.get(channel.toLowerCase());
    if (!ch) return [];
    const start = Math.max(0, ch.messages.length - n);
    const msgs = ch.messages.slice(start);
    ch.readCursor = ch.messages.length;
    return msgs;
  }

  /** Fetch the last N messages from ALL channels and mark all as read. */
  fetchLastAll(n: number): ChannelMessage[] {
    const all: ChannelMessage[] = [];
    for (const ch of this._channels.values()) {
      all.push(...ch.messages);
      ch.readCursor = ch.messages.length;
    }
    all.sort((a, b) => a.timestamp - b.timestamp);
    const start = Math.max(0, all.length - n);
    return all.slice(start);
  }

  // -- Status ---------------------------------------------------------------

  /** Get a summary of all channels: name, unread count, member count. */
  status(): ChannelStatus[] {
    const result: ChannelStatus[] = [];
    for (const ch of this._channels.values()) {
      result.push({
        name: ch.name,
        topic: ch.topic,
        members: ch.members.length,
        totalMessages: ch.messages.length,
        unread: Math.max(0, ch.messages.length - ch.readCursor),
      });
    }
    return result;
  }
}
