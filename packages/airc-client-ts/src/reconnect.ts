/**
 * Auto-reconnect with exponential backoff.
 *
 * Mirrors the Rust reconnect logic from `airc-client/src/client.rs`.
 */

/** Reconnect configuration. */
export interface ReconnectConfig {
  /** Initial delay in ms before first retry. Default: 1000. */
  initialDelay?: number;
  /** Maximum delay in ms. Default: 60000. */
  maxDelay?: number;
  /** Backoff multiplier. Default: 2. */
  backoffFactor?: number;
}

/** Resolved reconnect parameters. */
export interface ReconnectParams {
  initialDelay: number;
  maxDelay: number;
  backoffFactor: number;
}

const DEFAULTS: ReconnectParams = {
  initialDelay: 1000,
  maxDelay: 60_000,
  backoffFactor: 2,
};

/** Resolve optional reconnect config to concrete parameters. */
export function resolveReconnectConfig(config?: ReconnectConfig): ReconnectParams {
  return {
    initialDelay: config?.initialDelay ?? DEFAULTS.initialDelay,
    maxDelay: config?.maxDelay ?? DEFAULTS.maxDelay,
    backoffFactor: config?.backoffFactor ?? DEFAULTS.backoffFactor,
  };
}

/**
 * Exponential backoff state machine.
 *
 * Usage:
 * ```ts
 * const backoff = new Backoff(params);
 * while (needsRetry) {
 *   await backoff.wait();
 *   // try to reconnect...
 * }
 * backoff.reset(); // on success
 * ```
 */
export class Backoff {
  private params: ReconnectParams;
  private _delay: number;
  private _attempt = 0;

  constructor(params: ReconnectParams) {
    this.params = params;
    this._delay = params.initialDelay;
  }

  /** Current attempt number (1-based, incremented on each `wait()`). */
  get attempt(): number {
    return this._attempt;
  }

  /** Current delay in ms. */
  get delay(): number {
    return this._delay;
  }

  /** Wait for the current delay, then advance to the next backoff step. */
  async wait(): Promise<void> {
    this._attempt++;
    await sleep(this._delay);
    this._delay = Math.min(this._delay * this.params.backoffFactor, this.params.maxDelay);
  }

  /** Reset the backoff state (call after successful reconnect). */
  reset(): void {
    this._delay = this.params.initialDelay;
    this._attempt = 0;
  }
}

/** Promise-based sleep. */
function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
