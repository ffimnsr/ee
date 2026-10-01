/**
 * Sample TypeScript file for editor syntax highlighting, code folding,
 * and LSP diagnostics testing.
 */

export interface CacheEntry<T> {
  key: string;
  value: T;
  expiresAt: number;
  accessCount: number;
}

export type CacheEvictionPolicy = 'lru' | 'lfu' | 'fifo' | 'ttl';

export interface CacheOptions {
  maxEntries?: number;
  defaultTtlMs?: number;
  evictionPolicy?: CacheEvictionPolicy;
}

export enum CacheEventKind {
  Hit = 'HIT',
  Miss = 'MISS',
  Set = 'SET',
  Evict = 'EVICT',
  Expire = 'EXPIRE',
}

export type CacheListener<T> = (event: CacheEventKind, key: string, value?: T) => void;

/**
 * An in-memory cache supporting TTL expiration and pluggable eviction.
 */
export class InMemoryCache<T> {
  private readonly store = new Map<string, CacheEntry<T>>();
  private readonly maxEntries: number;
  private readonly defaultTtlMs: number;
  private readonly evictionPolicy: CacheEvictionPolicy;
  private readonly listeners: CacheListener<T>[] = [];

  constructor(options: CacheOptions = {}) {
    this.maxEntries = options.maxEntries ?? 1000;
    this.defaultTtlMs = options.defaultTtlMs ?? 60_000;
    this.evictionPolicy = options.evictionPolicy ?? 'lru';
  }

  /**
   * Register an event listener for cache lifecycle events.
   */
  public addEventListener(listener: CacheListener<T>): () => void {
    this.listeners.push(listener);
    return () => {
      const index = this.listeners.indexOf(listener);
      if (index !== -1) {
        this.listeners.splice(index, 1);
      }
    };
  }

  private emit(event: CacheEventKind, key: string, value?: T): void {
    for (const listener of this.listeners) {
      try {
        listener(event, key, value);
      } catch (err) {
        console.error(`Error in cache listener for event ${event}:`, err);
      }
    }
  }

  /**
   * Retrieve a value from the cache.
   */
  public get(key: string): T | undefined {
    const entry = this.store.get(key);
    if (!entry) {
      this.emit(CacheEventKind.Miss, key);
      return undefined;
    }

    if (Date.now() > entry.expiresAt) {
      this.store.delete(key);
      this.emit(CacheEventKind.Expire, key, entry.value);
      return undefined;
    }

    entry.accessCount += 1;
    if (this.evictionPolicy === 'lru') {
      // Refresh map key insertion order for LRU tracking
      this.store.delete(key);
      this.store.set(key, entry);
    }

    this.emit(CacheEventKind.Hit, key, entry.value);
    return entry.value;
  }

  /**
   * Insert or update a value in the cache.
   */
  public set(key: string, value: T, ttlMs?: number): void {
    const ttl = ttlMs ?? this.defaultTtlMs;
    const expiresAt = Date.now() + ttl;

    if (this.store.size >= this.maxEntries && !this.store.has(key)) {
      this.evictOne();
    }

    const entry: CacheEntry<T> = {
      key,
      value,
      expiresAt,
      accessCount: 0,
    };

    this.store.set(key, entry);
    this.emit(CacheEventKind.Set, key, value);
  }

  private evictOne(): void {
    if (this.store.size === 0) return;

    let candidateKey: string | undefined;

    switch (this.evictionPolicy) {
      case 'fifo':
      case 'lru': {
        // Map iterators yield keys in insertion / refreshed order
        candidateKey = this.store.keys().next().value;
        break;
      }
      case 'lfu': {
        let lowestCount = Infinity;
        for (const [key, entry] of this.store.entries()) {
          if (entry.accessCount < lowestCount) {
            lowestCount = entry.accessCount;
            candidateKey = key;
          }
        }
        break;
      }
      case 'ttl': {
        let earliestExpiration = Infinity;
        for (const [key, entry] of this.store.entries()) {
          if (entry.expiresAt < earliestExpiration) {
            earliestExpiration = entry.expiresAt;
            candidateKey = key;
          }
        }
        break;
      }
    }

    if (candidateKey !== undefined) {
      const candidateValue = this.store.get(candidateKey)?.value;
      this.store.delete(candidateKey);
      this.emit(CacheEventKind.Evict, candidateKey, candidateValue);
    }
  }

  public clear(): void {
    this.store.clear();
  }

  public get size(): number {
    return this.store.size;
  }
}

// Example usage
export async function runExample(): Promise<void> {
  const cache = new InMemoryCache<string>({ maxEntries: 3, evictionPolicy: 'lru' });

  cache.addEventListener((event, key, val) => {
    console.log(`[CacheEvent] ${event} -> key: "${key}", val: "${val}"`);
  });

  cache.set('item1', 'Alpha');
  cache.set('item2', 'Beta');
  cache.set('item3', 'Gamma');

  cache.get('item1'); // item1 becomes most recently used
  cache.set('item4', 'Delta'); // item2 should be evicted under LRU

  console.log('Cache size:', cache.size);
}
