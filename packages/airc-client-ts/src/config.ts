/**
 * Client configuration — mirrors Rust `ClientConfig` from `airc-client`.
 *
 * For the TypeScript library the `serverAddr` is a WebSocket URL
 * (e.g. `ws://localhost:8080/ws`) instead of a TCP `host:port`.
 */

/**
 * Default WebSocket URL for connecting to aircd.
 *
 * Change this constant or override per-client via `ClientConfig.url`.
 */
export const DEFAULT_URL = "wss://irc.openlore.xyz/ws";

export interface ClientConfig {
  /**
   * WebSocket URL to connect to.
   * Defaults to {@link DEFAULT_URL} (`wss://irc.openlore.xyz/ws`) if not set.
   */
  url?: string;

  /** Desired nickname. */
  nick: string;

  /** Username (ident). Defaults to nick if not set. */
  username?: string;

  /** Real name / description. Defaults to nick if not set. */
  realname?: string;

  /**
   * Password for authentication. Used for SASL PLAIN if the server supports
   * it, otherwise falls back to PRIVMSG NickServ :IDENTIFY.
   */
  password?: string;

  /**
   * SASL account name (authcid). Defaults to `nick` if not set.
   * Only used when `password` is also set.
   */
  saslAccount?: string;

  /** Channels to auto-join after registration. */
  autoJoin?: string[];

  /** Maximum number of messages to buffer per channel. Default: 1000. */
  bufferSize?: number;
}

/** Resolve optional config fields to their defaults. */
export function resolveConfig(config: ClientConfig): Required<Omit<ClientConfig, "password" | "saslAccount">> & { password?: string; saslAccount?: string } {
  return {
    url: config.url ?? DEFAULT_URL,
    nick: config.nick,
    username: config.username ?? config.nick,
    realname: config.realname ?? config.nick,
    password: config.password,
    saslAccount: config.saslAccount,
    autoJoin: config.autoJoin ?? [],
    bufferSize: config.bufferSize ?? 1000,
  };
}
