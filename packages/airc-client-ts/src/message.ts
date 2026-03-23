/**
 * IRC message parsing and serialization per RFC 2812 + IRCv3 message-tags.
 *
 * Mirrors the Rust `IrcMessage` from `airc-shared/src/message.rs`.
 *
 * Wire format: `[@tags] [:prefix] COMMAND [params...] [:trailing]\r\n`
 *
 * IRCv3 message tags (https://ircv3.net/specs/extensions/message-tags) occupy
 * an optional `@key=value;bare_key;...` block at the very start of the line,
 * before the optional `:prefix`. Values use a custom escape sequence:
 *   `\:` → `;`   `\s` → ` `   `\\` → `\`   `\r` → CR   `\n` → LF
 *
 * This module handles parsing raw lines (without the trailing `\r\n`) into
 * structured `IrcMessage` values, and serializing them back to wire format.
 */

import { type Command, parseCommand, serializeCommand } from "./command.js";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/** Errors that can occur when parsing a raw IRC line. */
export type ParseErrorKind = "empty" | "empty_prefix" | "missing_command";

export class ParseError extends Error {
  readonly kind: ParseErrorKind;
  constructor(kind: ParseErrorKind, message: string) {
    super(message);
    this.name = "ParseError";
    this.kind = kind;
  }
}

// ---------------------------------------------------------------------------
// IrcMessage
// ---------------------------------------------------------------------------

/**
 * A parsed IRC protocol message.
 *
 * @example
 * ```ts
 * const msg = IrcMessage.parse(":server 001 nick :Welcome to IRC");
 * msg.prefix  // "server"
 * msg.params  // ["nick", "Welcome to IRC"]
 * ```
 *
 * @example
 * ```ts
 * const msg = IrcMessage.parse("@time=2026-01-01T00:00:00.000Z :nick!u@h PRIVMSG #chan :hello");
 * msg.tags  // [["time", "2026-01-01T00:00:00.000Z"]]
 * ```
 */
export class IrcMessage {
  /** Optional message prefix (source). */
  prefix: string | undefined;
  /** The parsed command. */
  command: Command;
  /** Command parameters. Trailing param stored without leading `:`. */
  params: string[];
  /**
   * IRCv3 message tags — key/value pairs from the `@tag=value;...` block.
   * Bare tags (no `=`) have `value = undefined`. Values are already unescaped.
   */
  tags: Array<[string, string | undefined]>;

  constructor(command: Command, params: string[] = [], prefix?: string, tags: Array<[string, string | undefined]> = []) {
    this.prefix = prefix;
    this.command = command;
    this.params = params;
    this.tags = tags;
  }

  /**
   * Look up a tag value by key. Returns the value string (may be empty),
   * `undefined` if the tag is present as a bare key (no `=`), or `null` if
   * the tag is absent entirely.
   */
  tag(key: string): string | undefined | null {
    const entry = this.tags.find(([k]) => k === key);
    if (entry === undefined) return null;
    return entry[1];
  }

  // -- Parsing --------------------------------------------------------------

  /**
   * Parse a raw IRC line into an `IrcMessage`.
   *
   * The input should **not** contain the trailing `\r\n` (though it is
   * tolerated and stripped). IRCv3 message tags (`@key=value;...`) are parsed
   * and stored in `tags`; the rest is parsed per RFC 2812.
   */
  static parse(line: string): IrcMessage {
    let rest = line.replace(/\r?\n$/, "");

    if (rest.length === 0) {
      throw new ParseError("empty", "empty message");
    }

    // --- IRCv3 tags (optional @-prefixed block) ---
    let tags: Array<[string, string | undefined]> = [];
    if (rest.startsWith("@")) {
      const end = rest.indexOf(" ");
      if (end < 0) {
        throw new ParseError("missing_command", "missing command after tags");
      }
      tags = parseTags(rest.slice(1, end));
      rest = rest.slice(end + 1).trimStart();
    }

    if (rest.length === 0) {
      throw new ParseError("missing_command", "missing command");
    }

    // --- prefix ---
    let prefix: string | undefined;
    if (rest.startsWith(":")) {
      const end = rest.indexOf(" ");
      if (end < 0) {
        throw new ParseError("missing_command", "missing command");
      }
      const pfx = rest.slice(1, end);
      if (pfx.length === 0) {
        throw new ParseError("empty_prefix", "empty prefix");
      }
      rest = rest.slice(end + 1).trimStart();
      prefix = pfx;
    }

    if (rest.length === 0) {
      throw new ParseError("missing_command", "missing command");
    }

    // --- command ---
    let cmdStr: string;
    let remainder: string;
    const spaceIdx = rest.indexOf(" ");
    if (spaceIdx >= 0) {
      cmdStr = rest.slice(0, spaceIdx);
      remainder = rest.slice(spaceIdx + 1);
    } else {
      cmdStr = rest;
      remainder = "";
    }

    const command = parseCommand(cmdStr);

    // --- params ---
    const params = parseParams(remainder);

    return new IrcMessage(command, params, prefix, tags);
  }

  // -- Serialization --------------------------------------------------------

  /**
   * Serialize this message to IRC wire format **without** the trailing
   * `\r\n`. The caller is responsible for appending `\r\n` if needed.
   */
  serialize(): string {
    let out = "";

    // Prefix
    if (this.prefix !== undefined) {
      out += `:${this.prefix} `;
    }

    // Command
    out += serializeCommand(this.command);

    // Parameters
    const len = this.params.length;
    for (let i = 0; i < len; i++) {
      const param = this.params[i];
      const isLast = i + 1 === len;
      // The last parameter gets a `:` prefix if it contains spaces,
      // is empty, or starts with `:`.
      if (isLast && (param.includes(" ") || param.length === 0 || param.startsWith(":"))) {
        out += ` :${param}`;
      } else {
        out += ` ${param}`;
      }
    }

    return out;
  }

  // -- Builder / convenience constructors -----------------------------------

  /** Return a clone with the given prefix set. */
  withPrefix(prefix: string): IrcMessage {
    return new IrcMessage(this.command, [...this.params], prefix, [...this.tags]);
  }

  /** Return a clone with an additional tag appended. */
  withTag(key: string, value?: string): IrcMessage {
    return new IrcMessage(this.command, [...this.params], this.prefix, [...this.tags, [key, value]]);
  }

  /** Create a `PRIVMSG` message. */
  static privmsg(target: string, text: string): IrcMessage {
    return new IrcMessage({ kind: "named", name: "PRIVMSG" }, [target, text]);
  }

  /** Create a `NOTICE` message. */
  static notice(target: string, text: string): IrcMessage {
    return new IrcMessage({ kind: "named", name: "NOTICE" }, [target, text]);
  }

  /** Create a `NICK` message. */
  static nick(nickname: string): IrcMessage {
    return new IrcMessage({ kind: "named", name: "NICK" }, [nickname]);
  }

  /** Create a `JOIN` message. */
  static join(channel: string): IrcMessage {
    return new IrcMessage({ kind: "named", name: "JOIN" }, [channel]);
  }

  /** Create a `PART` message with an optional reason. */
  static part(channel: string, reason?: string): IrcMessage {
    const params = [channel];
    if (reason !== undefined) params.push(reason);
    return new IrcMessage({ kind: "named", name: "PART" }, params);
  }

  /** Create a `QUIT` message with an optional reason. */
  static quit(reason?: string): IrcMessage {
    const params = reason !== undefined ? [reason] : [];
    return new IrcMessage({ kind: "named", name: "QUIT" }, params);
  }

  /** Create a `PING` message. */
  static ping(token: string): IrcMessage {
    return new IrcMessage({ kind: "named", name: "PING" }, [token]);
  }

  /** Create a `PONG` message. */
  static pong(token: string): IrcMessage {
    return new IrcMessage({ kind: "named", name: "PONG" }, [token]);
  }

  /** Create a `USER` message. */
  static user(username: string, realname: string): IrcMessage {
    return new IrcMessage({ kind: "named", name: "USER" }, [username, "0", "*", realname]);
  }

  /** Create a `PASS` message. */
  static pass(password: string): IrcMessage {
    return new IrcMessage({ kind: "named", name: "PASS" }, [password]);
  }

  /** Create a `MODE` message. */
  static mode(target: string, modes?: string): IrcMessage {
    const params = [target];
    if (modes !== undefined) params.push(modes);
    return new IrcMessage({ kind: "named", name: "MODE" }, params);
  }

  /** Create a numeric reply message. */
  static numeric(code: number, target: string, params: string[]): IrcMessage {
    return new IrcMessage({ kind: "numeric", code }, [target, ...params]);
  }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/**
 * Parse the parameter portion of an IRC message into an array.
 *
 * Parameters are space-separated. A parameter starting with `:` begins the
 * "trailing" parameter which consumes the rest of the line (and may contain
 * spaces). The leading `:` is stripped from the trailing value.
 */
function parseParams(input: string): string[] {
  const params: string[] = [];
  let rest = input;

  while (rest.length > 0) {
    // Trailing parameter — everything after the `:` is one parameter.
    if (rest.startsWith(":")) {
      params.push(rest.slice(1));
      break;
    }

    const spaceIdx = rest.indexOf(" ");
    if (spaceIdx >= 0) {
      const param = rest.slice(0, spaceIdx);
      if (param.length > 0) {
        params.push(param);
      }
      rest = rest.slice(spaceIdx + 1).trimStart();
    } else {
      // Last parameter, no trailing colon.
      params.push(rest);
      break;
    }
  }

  return params;
}

/**
 * Parse an IRCv3 tag block into key/value pairs.
 *
 * Input is the raw string after the leading `@` and before the first space:
 * `tag1=value1;tag2;tag3=val\:ue3`
 *
 * IRCv3 escape sequences in values:
 *   `\:` → `;`   `\s` → ` `   `\\` → `\`   `\r` → CR   `\n` → LF
 * Any other `\X` → `X` (unknown escapes pass through the char after backslash).
 */
function parseTags(raw: string): Array<[string, string | undefined]> {
  if (raw.length === 0) return [];
  return raw.split(";").map((part) => {
    const eqIdx = part.indexOf("=");
    if (eqIdx < 0) {
      return [part, undefined] as [string, string | undefined];
    }
    const key = part.slice(0, eqIdx);
    const value = unescapeTagValue(part.slice(eqIdx + 1));
    return [key, value] as [string, string | undefined];
  });
}

/** Unescape an IRCv3 tag value. */
function unescapeTagValue(raw: string): string {
  let out = "";
  let i = 0;
  while (i < raw.length) {
    if (raw[i] === "\\" && i + 1 < raw.length) {
      const next = raw[i + 1];
      switch (next) {
        case ":": out += ";"; break;
        case "s": out += " "; break;
        case "\\": out += "\\"; break;
        case "r": out += "\r"; break;
        case "n": out += "\n"; break;
        default: out += next; break;
      }
      i += 2;
    } else {
      out += raw[i];
      i++;
    }
  }
  return out;
}
