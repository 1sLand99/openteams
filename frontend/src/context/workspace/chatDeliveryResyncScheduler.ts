/**
 * Single-flight resync scheduler for the chat delivery runtime.
 *
 * The reducer flags `needsResync` on revision gaps, ambiguous terminal
 * transitions and unknown delivery statuses. This scheduler is the consumer:
 * every flag triggers exactly one in-flight authoritative replay/snapshot
 * recovery per session; failures retry on a timed backoff (a real timer, so a
 * first failure can never stall forever); `dispose()` cancels pending retries
 * and swallows late responses.
 *
 * All side effects (fetch, apply, clock, timers) are injectable so the
 * behavior is testable without React or real timers.
 */

export interface ChatDeliveryResyncSchedulerOptions {
  recover: (sessionId: string, requestedAt: number) => Promise<void>;
  onError?: (sessionId: string) => void;
  /**
   * Checked after a successful fetch: when it reports a continuing need
   * (e.g. a stale response could not clear the flag, or another gap arrived
   * mid-flight), the scheduler immediately starts the next fetch instead of
   * waiting for the caller to notice.
   */
  shouldResync?: (sessionId: string) => boolean;
  now?: () => number;
  setTimeoutFn?: (callback: () => void, delayMs: number) => unknown;
  clearTimeoutFn?: (handle: unknown) => void;
  baseDelayMs?: number;
  maxDelayMs?: number;
}

interface SessionEntry {
  inFlight: boolean;
  /** Another resync was requested while a fetch was in flight. */
  requestedAgain: boolean;
  attempts: number;
  timer: unknown | null;
}

const DEFAULT_BASE_DELAY_MS = 1000;
const DEFAULT_MAX_DELAY_MS = 30000;

export class ChatDeliveryResyncScheduler {
  private readonly entries = new Map<string, SessionEntry>();
  private disposed = false;

  constructor(
    private readonly options: ChatDeliveryResyncSchedulerOptions,
  ) {}

  private now(): number {
    return this.options.now?.() ?? Date.now();
  }

  private setTimer(callback: () => void, delayMs: number): unknown {
    return (this.options.setTimeoutFn ?? setTimeout)(callback, delayMs);
  }

  private clearTimer(handle: unknown): void {
    if (this.options.clearTimeoutFn) {
      this.options.clearTimeoutFn(handle);
      return;
    }
    clearTimeout(handle as Parameters<typeof clearTimeout>[0]);
  }

  private backoffDelayMs(attempts: number): number {
    const base = this.options.baseDelayMs ?? DEFAULT_BASE_DELAY_MS;
    const max = this.options.maxDelayMs ?? DEFAULT_MAX_DELAY_MS;
    return Math.min(max, base * 2 ** attempts);
  }

  /** Ask for authoritative replay/snapshot recovery. Idempotent per request. */
  request(sessionId: string): void {
    if (this.disposed || !sessionId) return;
    const entry = this.entries.get(sessionId);
    if (entry?.inFlight) {
      // Coalesce repeated gap notifications into at most one follow-up.
      entry.requestedAgain = true;
      return;
    }
    if (entry?.timer != null) {
      // A retry is already scheduled; bring it forward is unnecessary —
      // the timer will fire and re-request.
      return;
    }
    this.start(sessionId, entry);
  }

  private start(sessionId: string, entry?: SessionEntry): void {
    const current: SessionEntry = entry ?? {
      inFlight: false,
      requestedAgain: false,
      attempts: 0,
      timer: null,
    };
    if (current.timer != null) {
      this.clearTimer(current.timer);
      current.timer = null;
    }
    current.inFlight = true;
    this.entries.set(sessionId, current);
    const requestedAt = this.now();
    this.options
      .recover(sessionId, requestedAt)
      .then(() => {
        if (this.disposed) return;
        const requestedAgain = current.requestedAgain;
        this.entries.delete(sessionId);
        if (this.disposed) return;
        // A gap raised while this fetch was in flight leaves the flag set
        // (stale responses never clear it); continue the cycle immediately
        // instead of waiting for the caller to notice.
        const stillNeeded =
          this.options.shouldResync?.(sessionId) ?? requestedAgain;
        if (stillNeeded) this.start(sessionId);
      })
      .catch(() => {
        if (this.disposed) return;
        current.inFlight = false;
        current.attempts += 1;
        this.options.onError?.(sessionId);
        // Timed retry: without this timer the first failure would stall the
        // session forever because no new state change re-triggers the flag.
        current.timer = this.setTimer(() => {
          if (this.disposed) return;
          current.timer = null;
          this.start(sessionId, current);
        }, this.backoffDelayMs(current.attempts));
      });
  }

  /** Cancel pending retries; in-flight responses are ignored afterwards. */
  dispose(): void {
    this.disposed = true;
    for (const entry of this.entries.values()) {
      if (entry.timer != null) this.clearTimer(entry.timer);
    }
    this.entries.clear();
  }
}
