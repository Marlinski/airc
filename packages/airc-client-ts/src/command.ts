/**
 * IRC command types — mirrors the Rust `Command` enum from `airc-shared`.
 *
 * Uses a discriminated string union rather than a TypeScript enum so that
 * commands are plain strings at runtime and JSON-friendly.
 */

/** Named IRC commands that the client explicitly handles. */
export type NamedCommand =
  // Registration
  | "NICK"
  | "USER"
  | "PASS"
  | "QUIT"
  // Messaging
  | "PRIVMSG"
  | "NOTICE"
  // Channels
  | "JOIN"
  | "PART"
  | "KICK"
  | "TOPIC"
  | "MODE"
  | "INVITE"
  // Queries
  | "WHO"
  | "WHOIS"
  | "LIST"
  | "NAMES"
  | "ISON"
  // Availability
  | "AWAY"
  // Moderation / social
  | "SILENCE"
  | "FRIEND"
  // Server
  | "PING"
  | "PONG"
  | "MOTD";

/** Set of all known named commands for fast lookup. */
const KNOWN_COMMANDS: ReadonlySet<string> = new Set<NamedCommand>([
  "NICK",
  "USER",
  "PASS",
  "QUIT",
  "PRIVMSG",
  "NOTICE",
  "JOIN",
  "PART",
  "KICK",
  "TOPIC",
  "MODE",
  "INVITE",
  "WHO",
  "WHOIS",
  "LIST",
  "NAMES",
  "ISON",
  "AWAY",
  "SILENCE",
  "FRIEND",
  "PING",
  "PONG",
  "MOTD",
]);

/**
 * A parsed IRC command.
 *
 * - Named commands are stored as `{ kind: "named"; name: NamedCommand }`.
 * - Three-digit numeric replies as `{ kind: "numeric"; code: number }`.
 * - Anything else as `{ kind: "unknown"; raw: string }`.
 */
export type Command =
  | { readonly kind: "named"; readonly name: NamedCommand }
  | { readonly kind: "numeric"; readonly code: number }
  | { readonly kind: "unknown"; readonly raw: string };

/**
 * Parse an uppercased command string into a `Command`.
 *
 * Three-digit numeric strings become `{ kind: "numeric" }`, recognised
 * command names become `{ kind: "named" }`, and everything else becomes
 * `{ kind: "unknown" }`.
 */
export function parseCommand(s: string): Command {
  const upper = s.toUpperCase();

  // Try numeric first — must be exactly three ASCII digits.
  if (upper.length === 3 && /^\d{3}$/.test(upper)) {
    return { kind: "numeric", code: parseInt(upper, 10) };
  }

  if (KNOWN_COMMANDS.has(upper)) {
    return { kind: "named", name: upper as NamedCommand };
  }

  return { kind: "unknown", raw: upper };
}

/**
 * Serialize a `Command` to its IRC wire string.
 *
 * Numeric codes are zero-padded to 3 digits.
 */
export function serializeCommand(cmd: Command): string {
  switch (cmd.kind) {
    case "named":
      return cmd.name;
    case "numeric":
      return String(cmd.code).padStart(3, "0");
    case "unknown":
      return cmd.raw;
  }
}

// -- Convenience helpers for matching --

/** Check if a command is a specific named command. */
export function isNamed(cmd: Command, name: NamedCommand): boolean {
  return cmd.kind === "named" && cmd.name === name;
}

/** Check if a command is a specific numeric code. */
export function isNumeric(cmd: Command, code: number): boolean {
  return cmd.kind === "numeric" && cmd.code === code;
}
