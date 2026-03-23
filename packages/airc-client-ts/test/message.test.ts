/**
 * Tests for IRC message parsing and serialization.
 *
 * Ports the 40+ Rust tests from `airc-shared/src/message.rs` plus
 * additional prefix tests from `airc-shared/src/prefix.rs` and
 * command tests.
 */

import { describe, it, expect } from "vitest";
import { IrcMessage, ParseError } from "../src/message.js";
import { parseCommand, serializeCommand, isNamed, isNumeric, type Command } from "../src/command.js";
import { Prefix, extractNick } from "../src/prefix.js";

// ===========================================================================
// Command tests
// ===========================================================================

describe("Command", () => {
  it("parses named commands", () => {
    const cmd = parseCommand("NICK");
    expect(cmd).toEqual({ kind: "named", name: "NICK" });
  });

  it("parses case-insensitive commands", () => {
    const cmd = parseCommand("privmsg");
    expect(cmd).toEqual({ kind: "named", name: "PRIVMSG" });
  });

  it("parses numeric commands", () => {
    const cmd = parseCommand("001");
    expect(cmd).toEqual({ kind: "numeric", code: 1 });
  });

  it("parses 3-digit numeric with leading zeros", () => {
    const cmd = parseCommand("042");
    expect(cmd).toEqual({ kind: "numeric", code: 42 });
  });

  it("parses unknown commands", () => {
    const cmd = parseCommand("FOOBAR");
    expect(cmd).toEqual({ kind: "unknown", raw: "FOOBAR" });
  });

  it("serializes named commands", () => {
    expect(serializeCommand({ kind: "named", name: "NICK" })).toBe("NICK");
  });

  it("serializes numeric commands with zero-padding", () => {
    expect(serializeCommand({ kind: "numeric", code: 1 })).toBe("001");
    expect(serializeCommand({ kind: "numeric", code: 42 })).toBe("042");
    expect(serializeCommand({ kind: "numeric", code: 433 })).toBe("433");
  });

  it("serializes unknown commands", () => {
    expect(serializeCommand({ kind: "unknown", raw: "FOO" })).toBe("FOO");
  });

  it("isNamed helper works", () => {
    const cmd: Command = { kind: "named", name: "NICK" };
    expect(isNamed(cmd, "NICK")).toBe(true);
    expect(isNamed(cmd, "USER")).toBe(false);
  });

  it("isNumeric helper works", () => {
    const cmd: Command = { kind: "numeric", code: 433 };
    expect(isNumeric(cmd, 433)).toBe(true);
    expect(isNumeric(cmd, 1)).toBe(false);
  });
});

// ===========================================================================
// Prefix tests
// ===========================================================================

describe("Prefix", () => {
  it("parses full user prefix", () => {
    const p = Prefix.parse("nick!user@host.com");
    expect(p.nick()).toBe("nick");
    expect(p.user()).toBe("user");
    expect(p.host()).toBe("host.com");
    expect(p.isUser()).toBe(true);
    expect(p.isServer()).toBe(false);
  });

  it("parses server prefix", () => {
    const p = Prefix.parse("irc.server.com");
    expect(p.nick()).toBe("irc.server.com");
    expect(p.user()).toBeUndefined();
    expect(p.host()).toBeUndefined();
    expect(p.isServer()).toBe(true);
    expect(p.isUser()).toBe(false);
  });

  it("parses nick@host without user", () => {
    const p = Prefix.parse("nick@host.com");
    expect(p.nick()).toBe("nick");
    expect(p.user()).toBeUndefined();
    expect(p.host()).toBe("host.com");
  });

  it("display roundtrip", () => {
    const raw = "nick!user@host.com";
    const p = Prefix.parse(raw);
    expect(p.toString()).toBe(raw);
  });

  it("userPrefix builder", () => {
    const p = Prefix.userPrefix("alice", "asmith", "example.com");
    expect(p.nick()).toBe("alice");
    expect(p.user()).toBe("asmith");
    expect(p.host()).toBe("example.com");
    expect(p.toString()).toBe("alice!asmith@example.com");
  });

  it("server builder", () => {
    const p = Prefix.server("irc.example.com");
    expect(p.nick()).toBe("irc.example.com");
    expect(p.isServer()).toBe(true);
  });

  it("extractNick from prefix string", () => {
    expect(extractNick("nick!user@host")).toBe("nick");
    expect(extractNick("server.com")).toBe("server.com");
    expect(extractNick(undefined)).toBe("");
  });
});

// ===========================================================================
// IrcMessage — Parsing tests
// ===========================================================================

describe("IrcMessage.parse", () => {
  it("parses simple command", () => {
    const msg = IrcMessage.parse("QUIT");
    expect(msg.prefix).toBeUndefined();
    expect(msg.command).toEqual({ kind: "named", name: "QUIT" });
    expect(msg.params).toEqual([]);
  });

  it("parses command with params", () => {
    const msg = IrcMessage.parse("NICK alice");
    expect(msg.command).toEqual({ kind: "named", name: "NICK" });
    expect(msg.params).toEqual(["alice"]);
  });

  it("parses prefix and trailing", () => {
    const msg = IrcMessage.parse(":nick!user@host PRIVMSG #chan :hello world");
    expect(msg.prefix).toBe("nick!user@host");
    expect(msg.command).toEqual({ kind: "named", name: "PRIVMSG" });
    expect(msg.params).toEqual(["#chan", "hello world"]);
  });

  it("parses numeric reply", () => {
    const msg = IrcMessage.parse(":server 001 nick :Welcome to the IRC Network");
    expect(msg.prefix).toBe("server");
    expect(msg.command).toEqual({ kind: "numeric", code: 1 });
    expect(msg.params).toEqual(["nick", "Welcome to the IRC Network"]);
  });

  it("parses case-insensitive command", () => {
    const msg = IrcMessage.parse("privmsg #test :hi");
    expect(msg.command).toEqual({ kind: "named", name: "PRIVMSG" });
  });

  it("parses no params", () => {
    const msg = IrcMessage.parse(":server PING");
    expect(msg.prefix).toBe("server");
    expect(msg.command).toEqual({ kind: "named", name: "PING" });
    expect(msg.params).toEqual([]);
  });

  it("parses only trailing", () => {
    const msg = IrcMessage.parse("QUIT :Gone for lunch");
    expect(msg.command).toEqual({ kind: "named", name: "QUIT" });
    expect(msg.params).toEqual(["Gone for lunch"]);
  });

  it("parses USER command", () => {
    const msg = IrcMessage.parse("USER alice 0 * :Alice Smith");
    expect(msg.command).toEqual({ kind: "named", name: "USER" });
    expect(msg.params).toEqual(["alice", "0", "*", "Alice Smith"]);
  });

  it("parses empty trailing", () => {
    const msg = IrcMessage.parse("TOPIC #chan :");
    expect(msg.command).toEqual({ kind: "named", name: "TOPIC" });
    expect(msg.params).toEqual(["#chan", ""]);
  });

  it("parses unknown command", () => {
    const msg = IrcMessage.parse("FOOBAR arg1 arg2");
    expect(msg.command).toEqual({ kind: "unknown", raw: "FOOBAR" });
    expect(msg.params).toEqual(["arg1", "arg2"]);
  });

  it("strips crlf", () => {
    const msg = IrcMessage.parse("PING server\r\n");
    expect(msg.command).toEqual({ kind: "named", name: "PING" });
    expect(msg.params).toEqual(["server"]);
  });

  it("rejects empty line", () => {
    expect(() => IrcMessage.parse("")).toThrow(ParseError);
    try {
      IrcMessage.parse("");
    } catch (e) {
      expect((e as ParseError).kind).toBe("empty");
    }
  });

  it("rejects empty prefix", () => {
    expect(() => IrcMessage.parse(": NICK alice")).toThrow(ParseError);
    try {
      IrcMessage.parse(": NICK alice");
    } catch (e) {
      expect((e as ParseError).kind).toBe("empty_prefix");
    }
  });

  it("rejects prefix only", () => {
    expect(() => IrcMessage.parse(":server")).toThrow(ParseError);
    try {
      IrcMessage.parse(":server");
    } catch (e) {
      expect((e as ParseError).kind).toBe("missing_command");
    }
  });

  it("rejects prefix with trailing space, no command", () => {
    expect(() => IrcMessage.parse(":server ")).toThrow(ParseError);
    try {
      IrcMessage.parse(":server ");
    } catch (e) {
      expect((e as ParseError).kind).toBe("missing_command");
    }
  });

  it("parses JOIN multiple channels", () => {
    const msg = IrcMessage.parse("JOIN #a,#b,#c");
    expect(msg.command).toEqual({ kind: "named", name: "JOIN" });
    expect(msg.params).toEqual(["#a,#b,#c"]);
  });

  it("parses KICK with reason", () => {
    const msg = IrcMessage.parse(":op!u@h KICK #chan victim :You have been kicked");
    expect(msg.command).toEqual({ kind: "named", name: "KICK" });
    expect(msg.params).toEqual(["#chan", "victim", "You have been kicked"]);
  });

  it("parses MODE command", () => {
    const msg = IrcMessage.parse("MODE #chan +o alice");
    expect(msg.command).toEqual({ kind: "named", name: "MODE" });
    expect(msg.params).toEqual(["#chan", "+o", "alice"]);
  });

  it("is lenient with multiple spaces", () => {
    const msg = IrcMessage.parse("NICK   alice");
    expect(msg.command).toEqual({ kind: "named", name: "NICK" });
    expect(msg.params).toEqual(["alice"]);
  });
});

// ===========================================================================
// IrcMessage — Serialization tests
// ===========================================================================

describe("IrcMessage.serialize", () => {
  it("serializes simple command", () => {
    const msg = new IrcMessage({ kind: "named", name: "QUIT" });
    expect(msg.serialize()).toBe("QUIT");
  });

  it("serializes with prefix", () => {
    const msg = IrcMessage.privmsg("#chan", "hello world").withPrefix("nick!user@host");
    expect(msg.serialize()).toBe(":nick!user@host PRIVMSG #chan :hello world");
  });

  it("serializes without trailing when not needed", () => {
    const msg = IrcMessage.nick("alice");
    expect(msg.serialize()).toBe("NICK alice");
  });

  it("serializes numeric with prefix", () => {
    const msg = IrcMessage.numeric(1, "nick", ["Welcome to IRC"]).withPrefix("server");
    expect(msg.serialize()).toBe(":server 001 nick :Welcome to IRC");
  });

  it("serializes numeric zero-padded", () => {
    const msg = new IrcMessage({ kind: "numeric", code: 42 });
    expect(msg.serialize()).toBe("042");
  });

  it("serializes empty trailing", () => {
    const msg = new IrcMessage({ kind: "named", name: "TOPIC" }, ["#chan", ""]);
    expect(msg.serialize()).toBe("TOPIC #chan :");
  });

  it("serializes trailing starting with colon", () => {
    const msg = new IrcMessage({ kind: "named", name: "PRIVMSG" }, ["#chan", ":)"]);
    expect(msg.serialize()).toBe("PRIVMSG #chan ::)");
  });
});

// ===========================================================================
// IrcMessage — Round-trip tests
// ===========================================================================

describe("IrcMessage round-trip", () => {
  const roundtripCases = [
    ":nick!user@host PRIVMSG #channel :hello world",
    ":irc.server.com 433 * alice :Nickname is already in use",
    "QUIT",
    "QUIT :Gone for lunch",
    "USER alice 0 * :Alice Smith",
  ];

  for (const raw of roundtripCases) {
    it(`round-trips: ${raw}`, () => {
      const msg = IrcMessage.parse(raw);
      expect(msg.serialize()).toBe(raw);
    });
  }
});

// ===========================================================================
// IrcMessage — Builder / convenience tests
// ===========================================================================

describe("IrcMessage builders", () => {
  it("privmsg builder", () => {
    const msg = IrcMessage.privmsg("#test", "hi there");
    expect(msg.command).toEqual({ kind: "named", name: "PRIVMSG" });
    expect(msg.params).toEqual(["#test", "hi there"]);
    expect(msg.prefix).toBeUndefined();
  });

  it("withPrefix builder", () => {
    const msg = IrcMessage.ping("token").withPrefix("server.example.com");
    expect(msg.prefix).toBe("server.example.com");
    expect(msg.command).toEqual({ kind: "named", name: "PING" });
  });

  it("numeric builder", () => {
    const msg = IrcMessage.numeric(353, "nick", ["= #chan", "alice bob"]);
    expect(msg.command).toEqual({ kind: "numeric", code: 353 });
    expect(msg.params).toEqual(["nick", "= #chan", "alice bob"]);
  });

  it("part with reason", () => {
    const msg = IrcMessage.part("#chan", "Leaving");
    expect(msg.params).toEqual(["#chan", "Leaving"]);
  });

  it("part without reason", () => {
    const msg = IrcMessage.part("#chan");
    expect(msg.params).toEqual(["#chan"]);
  });

  it("quit with reason", () => {
    const msg = IrcMessage.quit("bye");
    expect(msg.params).toEqual(["bye"]);
  });

  it("quit without reason", () => {
    const msg = IrcMessage.quit();
    expect(msg.params).toEqual([]);
  });

  it("user builder", () => {
    const msg = IrcMessage.user("alice", "Alice Smith");
    expect(msg.serialize()).toBe("USER alice 0 * :Alice Smith");
  });

  it("pass builder", () => {
    const msg = IrcMessage.pass("secret");
    expect(msg.serialize()).toBe("PASS secret");
  });

  it("mode builder", () => {
    const msg = IrcMessage.mode("#chan", "+o");
    expect(msg.params).toEqual(["#chan", "+o"]);
  });

  it("mode builder without modes", () => {
    const msg = IrcMessage.mode("#chan");
    expect(msg.params).toEqual(["#chan"]);
  });

  it("join builder", () => {
    const msg = IrcMessage.join("#lobby");
    expect(msg.serialize()).toBe("JOIN #lobby");
  });

  it("pong builder", () => {
    const msg = IrcMessage.pong("token123");
    expect(msg.serialize()).toBe("PONG token123");
  });
});

// ===========================================================================
// IRCv3 message-tags tests
// ===========================================================================

describe("IrcMessage tags", () => {
  it("parses bare tag (no value)", () => {
    const msg = IrcMessage.parse("@draft/msgid :nick!u@h PRIVMSG #chan :hi");
    expect(msg.tags).toEqual([["draft/msgid", undefined]]);
    expect(msg.command).toEqual({ kind: "named", name: "PRIVMSG" });
  });

  it("parses key=value tag", () => {
    const msg = IrcMessage.parse("@time=2026-01-01T12:00:00.000Z :nick!u@h PRIVMSG #chan :hi");
    expect(msg.tags).toEqual([["time", "2026-01-01T12:00:00.000Z"]]);
  });

  it("parses multiple tags", () => {
    const msg = IrcMessage.parse("@time=2026-01-01T12:00:00.000Z;msgid=abc123 :nick!u@h PRIVMSG #chan :hi");
    expect(msg.tags).toHaveLength(2);
    expect(msg.tags[0]).toEqual(["time", "2026-01-01T12:00:00.000Z"]);
    expect(msg.tags[1]).toEqual(["msgid", "abc123"]);
  });

  it("parses tags with no prefix", () => {
    const msg = IrcMessage.parse("@foo=bar PING token");
    expect(msg.tags).toEqual([["foo", "bar"]]);
    expect(msg.command).toEqual({ kind: "named", name: "PING" });
  });

  it("unescapes \\: to semicolon", () => {
    const msg = IrcMessage.parse("@foo=a\\:b :n!u@h PRIVMSG #c :x");
    expect(msg.tags[0]).toEqual(["foo", "a;b"]);
  });

  it("unescapes \\s to space", () => {
    const msg = IrcMessage.parse("@foo=a\\sb :n!u@h PRIVMSG #c :x");
    expect(msg.tags[0]).toEqual(["foo", "a b"]);
  });

  it("unescapes \\\\ to backslash", () => {
    const msg = IrcMessage.parse("@foo=a\\\\b :n!u@h PRIVMSG #c :x");
    expect(msg.tags[0]).toEqual(["foo", "a\\b"]);
  });

  it("passes through unknown escape", () => {
    const msg = IrcMessage.parse("@foo=a\\xb :n!u@h PRIVMSG #c :x");
    expect(msg.tags[0]).toEqual(["foo", "axb"]);
  });

  it("empty tags array when no tag block", () => {
    const msg = IrcMessage.parse(":nick!u@h PRIVMSG #chan :hi");
    expect(msg.tags).toEqual([]);
  });

  it("tag() lookup returns value for present key", () => {
    const msg = IrcMessage.parse("@time=2026-01-01T12:00:00.000Z :n!u@h PRIVMSG #c :x");
    expect(msg.tag("time")).toBe("2026-01-01T12:00:00.000Z");
  });

  it("tag() lookup returns null for absent key", () => {
    const msg = IrcMessage.parse("@time=2026-01-01T12:00:00.000Z :n!u@h PRIVMSG #c :x");
    expect(msg.tag("msgid")).toBeNull();
  });

  it("tag() lookup returns undefined for bare key", () => {
    const msg = IrcMessage.parse("@draft/labeled-response :n!u@h PRIVMSG #c :x");
    expect(msg.tag("draft/labeled-response")).toBeUndefined();
  });

  it("withTag() adds a tag to a copy", () => {
    const base = IrcMessage.parse(":n!u@h PRIVMSG #c :x");
    const tagged = base.withTag("time", "2026-01-01T12:00:00.000Z");
    expect(base.tags).toHaveLength(0);
    expect(tagged.tags).toEqual([["time", "2026-01-01T12:00:00.000Z"]]);
  });

  it("withTag() bare tag (no value)", () => {
    const base = IrcMessage.parse(":n!u@h PRIVMSG #c :x");
    const tagged = base.withTag("echo-message");
    expect(tagged.tags).toEqual([["echo-message", undefined]]);
  });

  it("parses tagged message with correct params", () => {
    const msg = IrcMessage.parse("@time=2026-01-01T00:00:00.000Z :alice!a@host PRIVMSG #lobby :hello world");
    expect(msg.prefix).toBe("alice!a@host");
    expect(msg.params).toEqual(["#lobby", "hello world"]);
  });

  it("multi-line LS with * marker", () => {
    // A plain (non-tagged) message is not affected by tag parsing.
    const msg = IrcMessage.parse(":server CAP * LS * :sasl message-tags");
    expect(msg.tags).toEqual([]);
    expect(msg.params[2]).toBe("*");
  });
});
