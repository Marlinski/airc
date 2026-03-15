/**
 * Structured IRC message prefix (source identifier).
 *
 * Mirrors the Rust `Prefix` from `airc-shared`. A prefix appears at the
 * start of server-originated messages as `:nick!user@host` or `:servername`.
 */

export class Prefix {
  /** The full raw prefix string (without leading `:`). */
  readonly raw: string;
  /** Byte offset of `!` if present (separates nick from user). */
  private readonly bang: number | undefined;
  /** Byte offset of `@` if present (separates user from host). */
  private readonly at: number | undefined;

  private constructor(raw: string, bang: number | undefined, at: number | undefined) {
    this.raw = raw;
    this.bang = bang;
    this.at = at;
  }

  /** Parse a prefix string into its components. Input should NOT include the leading `:`. */
  static parse(s: string): Prefix {
    const bang = s.indexOf("!");
    const at = s.indexOf("@");
    return new Prefix(s, bang >= 0 ? bang : undefined, at >= 0 ? at : undefined);
  }

  /** Build a user prefix from parts. */
  static userPrefix(nick: string, user: string, host: string): Prefix {
    const raw = `${nick}!${user}@${host}`;
    return new Prefix(raw, nick.length, nick.length + 1 + user.length);
  }

  /** Build a server prefix. */
  static server(name: string): Prefix {
    return new Prefix(name, undefined, undefined);
  }

  /** The nick portion (or the whole string if not a user prefix). */
  nick(): string {
    if (this.bang !== undefined) {
      return this.raw.slice(0, this.bang);
    }
    if (this.at !== undefined) {
      return this.raw.slice(0, this.at);
    }
    return this.raw;
  }

  /** The username portion, if present. */
  user(): string | undefined {
    if (this.bang === undefined) return undefined;
    const end = this.at ?? this.raw.length;
    return this.raw.slice(this.bang + 1, end);
  }

  /** The hostname portion, if present. */
  host(): string | undefined {
    if (this.at === undefined) return undefined;
    return this.raw.slice(this.at + 1);
  }

  /** Whether this looks like a user prefix (has `!` and `@`). */
  isUser(): boolean {
    return this.bang !== undefined && this.at !== undefined;
  }

  /** Whether this looks like a server prefix (no `!` or `@`). */
  isServer(): boolean {
    return this.bang === undefined && this.at === undefined;
  }

  toString(): string {
    return this.raw;
  }
}

/**
 * Extract the nick from an optional prefix string.
 *
 * Convenience helper matching the Rust `extract_nick` in conn.rs.
 */
export function extractNick(prefix: string | undefined): string {
  if (!prefix) return "";
  return Prefix.parse(prefix).nick();
}
