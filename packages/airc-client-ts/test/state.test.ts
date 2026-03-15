/**
 * Tests for ClientState — buffer, cursor, and fetch logic.
 *
 * Mirrors the Rust state tests and covers the key agent UX: fetch,
 * fetchAll, fetchLast, fetchLastAll.
 */

import { describe, it, expect } from "vitest";
import { ClientState } from "../src/state.js";
import { type ChannelMessage, MessageKind } from "../src/event.js";

/** Helper to create a ChannelMessage with a given timestamp. */
function msg(target: string, from: string, text: string, timestamp: number): ChannelMessage {
  return { target, from, text, kind: MessageKind.Normal, timestamp };
}

describe("ClientState", () => {
  // -- Identity -----------------------------------------------------------

  it("tracks nick", () => {
    const state = new ClientState("alice", 100);
    expect(state.nick()).toBe("alice");
    state.setNick("bob");
    expect(state.nick()).toBe("bob");
  });

  it("tracks server name", () => {
    const state = new ClientState("alice", 100);
    expect(state.serverName()).toBeUndefined();
    state.setServerName("irc.example.com");
    expect(state.serverName()).toBe("irc.example.com");
  });

  it("tracks registration", () => {
    const state = new ClientState("alice", 100);
    expect(state.isRegistered()).toBe(false);
    state.setRegistered();
    expect(state.isRegistered()).toBe(true);
  });

  // -- Channel management -------------------------------------------------

  it("joins and parts channels", () => {
    const state = new ClientState("alice", 100);
    state.joinChannel("#lobby");
    expect(state.channels()).toEqual(["#lobby"]);
    state.joinChannel("#dev");
    expect(state.channels().sort()).toEqual(["#dev", "#lobby"]);
    state.partChannel("#lobby");
    expect(state.channels()).toEqual(["#dev"]);
  });

  it("join is case-insensitive (no duplicates)", () => {
    const state = new ClientState("alice", 100);
    state.joinChannel("#Lobby");
    state.joinChannel("#lobby");
    expect(state.channels()).toEqual(["#Lobby"]);
  });

  it("sets topic", () => {
    const state = new ClientState("alice", 100);
    state.joinChannel("#lobby");
    state.setTopic("#lobby", "Welcome!");
    const s = state.status();
    expect(s[0].topic).toBe("Welcome!");
  });

  // -- Members ------------------------------------------------------------

  it("manages members", () => {
    const state = new ClientState("alice", 100);
    state.joinChannel("#lobby");
    state.setMembers("#lobby", ["alice", "bob"]);
    const s1 = state.status();
    expect(s1[0].members).toBe(2);

    state.addMember("#lobby", "charlie");
    const s2 = state.status();
    expect(s2[0].members).toBe(3);

    // No duplicate.
    state.addMember("#lobby", "Bob");
    expect(state.status()[0].members).toBe(3);

    state.removeMember("#lobby", "bob");
    expect(state.status()[0].members).toBe(2);
  });

  it("removeMemberAll removes from all channels", () => {
    const state = new ClientState("alice", 100);
    state.joinChannel("#a");
    state.joinChannel("#b");
    state.setMembers("#a", ["alice", "bob"]);
    state.setMembers("#b", ["bob", "charlie"]);
    state.removeMemberAll("bob");
    expect(state.status().find((s) => s.name === "#a")!.members).toBe(1);
    expect(state.status().find((s) => s.name === "#b")!.members).toBe(1);
  });

  it("renameMember updates all channels", () => {
    const state = new ClientState("alice", 100);
    state.joinChannel("#a");
    state.setMembers("#a", ["alice", "bob"]);
    state.renameMember("bob", "robert");
    // Check via status — member count unchanged, but name updated.
    expect(state.status()[0].members).toBe(2);
  });

  // -- Message buffering --------------------------------------------------

  it("pushes and fetches messages", () => {
    const state = new ClientState("alice", 100);
    state.joinChannel("#lobby");
    state.pushMessage("#lobby", msg("#lobby", "bob", "hello", 1));
    state.pushMessage("#lobby", msg("#lobby", "charlie", "world", 2));

    const fetched = state.fetch("#lobby");
    expect(fetched).toHaveLength(2);
    expect(fetched[0].text).toBe("hello");
    expect(fetched[1].text).toBe("world");

    // Second fetch returns nothing (cursor advanced).
    expect(state.fetch("#lobby")).toHaveLength(0);
  });

  it("fetchAll returns from all channels sorted by timestamp", () => {
    const state = new ClientState("alice", 100);
    state.joinChannel("#a");
    state.joinChannel("#b");
    state.pushMessage("#a", msg("#a", "bob", "a1", 3));
    state.pushMessage("#b", msg("#b", "charlie", "b1", 1));
    state.pushMessage("#a", msg("#a", "bob", "a2", 5));
    state.pushMessage("#b", msg("#b", "charlie", "b2", 2));

    const all = state.fetchAll();
    expect(all).toHaveLength(4);
    expect(all.map((m) => m.text)).toEqual(["b1", "b2", "a1", "a2"]);

    // Second fetchAll returns nothing.
    expect(state.fetchAll()).toHaveLength(0);
  });

  it("fetchLast returns last N and marks all as read", () => {
    const state = new ClientState("alice", 100);
    state.joinChannel("#lobby");
    for (let i = 0; i < 10; i++) {
      state.pushMessage("#lobby", msg("#lobby", "bob", `msg${i}`, i));
    }

    const last3 = state.fetchLast("#lobby", 3);
    expect(last3).toHaveLength(3);
    expect(last3.map((m) => m.text)).toEqual(["msg7", "msg8", "msg9"]);

    // All marked as read.
    expect(state.fetch("#lobby")).toHaveLength(0);
  });

  it("fetchLastAll returns last N across all channels", () => {
    const state = new ClientState("alice", 100);
    state.joinChannel("#a");
    state.joinChannel("#b");
    state.pushMessage("#a", msg("#a", "bob", "a1", 1));
    state.pushMessage("#b", msg("#b", "charlie", "b1", 2));
    state.pushMessage("#a", msg("#a", "bob", "a2", 3));
    state.pushMessage("#b", msg("#b", "charlie", "b2", 4));
    state.pushMessage("#a", msg("#a", "bob", "a3", 5));

    const last3 = state.fetchLastAll(3);
    expect(last3).toHaveLength(3);
    expect(last3.map((m) => m.text)).toEqual(["a2", "b2", "a3"]);

    // All marked as read.
    expect(state.fetchAll()).toHaveLength(0);
  });

  it("fetch from unknown channel returns empty", () => {
    const state = new ClientState("alice", 100);
    expect(state.fetch("#unknown")).toEqual([]);
  });

  it("fetchLast from unknown channel returns empty", () => {
    const state = new ClientState("alice", 100);
    expect(state.fetchLast("#unknown", 5)).toEqual([]);
  });

  // -- Buffer size enforcement --------------------------------------------

  it("trims messages to buffer size", () => {
    const state = new ClientState("alice", 5);
    state.joinChannel("#lobby");

    for (let i = 0; i < 10; i++) {
      state.pushMessage("#lobby", msg("#lobby", "bob", `msg${i}`, i));
    }

    const s = state.status();
    expect(s[0].totalMessages).toBe(5);
    // The last 5 messages should remain.
    const all = state.fetchLast("#lobby", 10);
    expect(all.map((m) => m.text)).toEqual(["msg5", "msg6", "msg7", "msg8", "msg9"]);
  });

  it("adjusts read cursor when trimming", () => {
    const state = new ClientState("alice", 5);
    state.joinChannel("#lobby");

    // Push 3 messages and read them.
    for (let i = 0; i < 3; i++) {
      state.pushMessage("#lobby", msg("#lobby", "bob", `msg${i}`, i));
    }
    state.fetch("#lobby"); // cursor = 3

    // Push 5 more — total 8, trims to 5, cursor adjusts.
    for (let i = 3; i < 8; i++) {
      state.pushMessage("#lobby", msg("#lobby", "bob", `msg${i}`, i));
    }

    // Unread should be the messages after the adjusted cursor.
    const unread = state.fetch("#lobby");
    expect(unread.length).toBeGreaterThan(0);
  });

  // -- Private messages ---------------------------------------------------

  it("buffers private messages under sender nick", () => {
    const state = new ClientState("alice", 100);
    const pm = msg("alice", "bob", "hey there", 1);
    state.pushPrivateMessage(pm);

    // Should be fetchable under "bob".
    const fetched = state.fetch("bob");
    expect(fetched).toHaveLength(1);
    expect(fetched[0].text).toBe("hey there");
  });

  it("private messages appear in fetchAll", () => {
    const state = new ClientState("alice", 100);
    state.joinChannel("#lobby");
    state.pushMessage("#lobby", msg("#lobby", "charlie", "channel msg", 1));
    state.pushPrivateMessage(msg("alice", "bob", "pm", 2));

    const all = state.fetchAll();
    expect(all).toHaveLength(2);
    expect(all[0].text).toBe("channel msg");
    expect(all[1].text).toBe("pm");
  });

  // -- Status -------------------------------------------------------------

  it("status reports correct unread counts", () => {
    const state = new ClientState("alice", 100);
    state.joinChannel("#lobby");
    state.pushMessage("#lobby", msg("#lobby", "bob", "m1", 1));
    state.pushMessage("#lobby", msg("#lobby", "bob", "m2", 2));

    const s1 = state.status();
    expect(s1[0].unread).toBe(2);
    expect(s1[0].totalMessages).toBe(2);

    state.fetch("#lobby");
    const s2 = state.status();
    expect(s2[0].unread).toBe(0);
    expect(s2[0].totalMessages).toBe(2);
  });
});
