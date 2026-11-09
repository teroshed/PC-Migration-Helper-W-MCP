/** Session-only LRU: never persists source observations or changes their freshness labels. */
export class StaleReadError extends Error {
  constructor() { super('This read belongs to an older folder context.'); this.name = 'StaleReadError'; }
}
export function isStaleRead(cause: unknown): cause is StaleReadError { return cause instanceof StaleReadError; }

export class ReadCache<T> {
  private values = new Map<string, { value: T; expires: number; bytes: number; retention: number }>();
  private pending = new Map<string, Promise<T>>();
  private generation = 0;
  private retentionGeneration = 0;
  private scope = '';
  private bytes = 0;
  private maxEntries: number;
  private maxBytes: number;
  private ttlMs: number;
  private now: () => number;
  constructor(maxEntries = 64, maxBytes = 8 * 1024 * 1024, ttlMs = 15_000, now = Date.now) {
    this.maxEntries = maxEntries; this.maxBytes = maxBytes; this.ttlMs = ttlMs; this.now = now;
  }
  setScope(scope: string): void {
    if (this.scope === scope) return;
    this.scope = scope;
    this.clear();
  }
  clear(): void {
    this.generation += 1;
    this.values.clear(); this.pending.clear(); this.bytes = 0;
  }
  evictValues(matches: (key: string) => boolean): void {
    for (const [key, entry] of this.values) {
      if (matches(key)) { this.values.delete(key); this.bytes -= entry.bytes; }
    }
  }
  expireValues(): void {
    // Progress counters can supersede cached derived statistics without making
    // an already-running source observation invalid for its current caller.
    this.retentionGeneration += 1;
  }
  invalidate(matches: (key: string) => boolean): void {
    this.evictValues(matches);
    for (const key of this.pending.keys()) if (matches(key)) this.pending.delete(key);
  }
  peek(key: string): T | undefined {
    if (this.values.get(key)?.retention !== this.retentionGeneration) return undefined;
    return this.peekRecent(key);
  }
  peekRecent(key: string): T | undefined {
    const entry = this.values.get(key);
    if (!entry) return undefined;
    this.values.delete(key);
    if (entry.expires <= this.now()) { this.bytes -= entry.bytes; return undefined; }
    this.values.set(key, entry);
    return entry.value;
  }
  read(key: string, loader: () => Promise<T>): Promise<T> {
    const cached = this.peek(key);
    if (cached !== undefined) return Promise.resolve(cached);
    const pending = this.pending.get(key);
    if (pending) return pending;
    const generation = this.generation;
    const retentionGeneration = this.retentionGeneration;
    const promise = Promise.resolve().then(() => {
      if (generation !== this.generation || this.pending.get(key) !== promise) throw new StaleReadError();
      return loader();
    }).then(value => {
      if (generation !== this.generation || this.pending.get(key) !== promise) throw new StaleReadError();
      // UTF-16 estimate bounds retained payloads as well as page count.
      const bytes = JSON.stringify(value).length * 2;
      if (bytes <= this.maxBytes) {
        this.bytes -= this.values.get(key)?.bytes ?? 0;
        this.values.delete(key);
        this.values.set(key, { value, bytes, expires: this.now() + this.ttlMs, retention: retentionGeneration });
        this.bytes += bytes;
        while (this.values.size > this.maxEntries || this.bytes > this.maxBytes) {
          const oldest = this.values.keys().next().value;
          if (oldest === undefined) break;
          const oldestEntry = this.values.get(oldest);
          if (!oldestEntry) break;
          this.bytes -= oldestEntry.bytes;
          this.values.delete(oldest);
        }
      }
      return value;
    }).finally(() => { if (this.pending.get(key) === promise) this.pending.delete(key); });
    this.pending.set(key, promise);
    return promise;
  }
}

export interface FolderQuery { path: string; search: string; filter: string; page: number }
export function folderQueryKey(query: FolderQuery): string {
  // Preserve the native path spelling; aliases miss rather than risk mixing sources.
  return JSON.stringify([query.path, query.search, query.filter, query.page]);
}

/** Navigation writes advance journal revision but cannot alter a listing. Everything
 * else (including unknown future fields) conservatively invalidates cached results. */
export function reviewContentKey(document: Record<string, unknown> | null): string {
  if (!document) return '';
  const { revision, view, viewProvenance, updatedAt, recentOperations, agentOperations, ...content } = document;
  return JSON.stringify(content);
}

/** Immediate-directory metadata freshness is separate from derived review state.
 * Hash progress or journal edits must not turn into repeated filesystem scans. */
export class FolderObservations {
  private observed = new Map<string, number>();
  private scope = '';
  private generation = 0;
  private pathTokens = new Map<string, object>();
  private now: () => number;
  private ttlMs: number;
  constructor(now = Date.now, ttlMs = 15_000) { this.now = now; this.ttlMs = ttlMs; }
  setScope(scope: string): void {
    if (scope === this.scope) return;
    this.scope = scope; this.clear();
  }
  clear(): void { this.generation++; this.observed.clear(); this.pathTokens.clear(); }
  invalidate(path: string): void { this.observed.delete(path); this.pathTokens.delete(path); }
  token(path: string): { generation: number; path: object } {
    let token = this.pathTokens.get(path);
    if (!token) { token = {}; this.pathTokens.set(path, token); }
    return { generation: this.generation, path: token };
  }
  current(path: string, token: { generation: number; path: object }): boolean {
    return token.generation === this.generation && token.path === this.pathTokens.get(path);
  }
  discard(path: string, token: { generation: number; path: object }): void {
    if (this.current(path, token) && !this.observed.has(path)) this.pathTokens.delete(path);
  }
  fresh(path: string): boolean {
    const at = this.observed.get(path);
    return at !== undefined && this.now() - at < this.ttlMs;
  }
  record(path: string, token: { generation: number; path: object }): void {
    if (!this.current(path, token)) return;
    this.observed.delete(path); this.observed.set(path, this.now());
    while (this.observed.size > 256) {
      const oldest = this.observed.keys().next().value;
      if (oldest === undefined) break;
      this.observed.delete(oldest);
      this.pathTokens.delete(oldest);
    }
  }
}
