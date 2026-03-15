/**
 * IRC message parsing and serialization per RFC 2812.
 *
 * Mirrors the Rust `IrcMessage` from `airc-shared/src/message.rs`.
 *
 * Wire format: `[:prefix] COMMAND [params...] [:trailing]\r\n`
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
 */
export class IrcMessage {
  /** Optional message prefix (source). */
  prefix: string | undefined;
  /** The parsed command. */
  command: Command;
  /** Command parameters. Trailing param stored without leading `:`. */
  params: string[];

  constructor(command: Command, params: string[] = [], prefix?: string) {
    this.prefix = prefix;
    this.command = command;
    this.params = params;
  }

  // -- Parsing --------------------------------------------------------------

  /**
   * Parse a raw IRC line into an `IrcMessage`.
   *
   * The input should **not** contain the trailing `\r\n` (though it is
   * tolerated and stripped).
   */
  static parse(line: string): IrcMessage {
    let rest = line.replace(/\r?\n$/, "");

    if (rest.length === 0) {
      throw new ParseError("empty", "empty message");
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

    return new IrcMessage(command, params, prefix);
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
    return new IrcMessage(this.command, [...this.params], prefix);
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
