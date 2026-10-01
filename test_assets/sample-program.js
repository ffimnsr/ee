/**
 * Sample JavaScript (ES2022) file for editor syntax highlighting,
 * tree-sitter navigation, and code folding testing.
 */

'use strict';

class EventEmitter {
  #events = new Map();

  on(eventName, listener) {
    if (typeof listener !== 'function') {
      throw new TypeError('Listener must be a function');
    }
    if (!this.#events.has(eventName)) {
      this.#events.set(eventName, new Set());
    }
    this.#events.get(eventName).add(listener);
    return () => this.off(eventName, listener);
  }

  off(eventName, listener) {
    const listeners = this.#events.get(eventName);
    if (listeners) {
      listeners.delete(listener);
      if (listeners.size === 0) {
        this.#events.delete(eventName);
      }
    }
  }

  emit(eventName, ...args) {
    const listeners = this.#events.get(eventName);
    if (!listeners) return false;
    for (const listener of Array.from(listeners)) {
      try {
        listener(...args);
      } catch (err) {
        console.error(`Unhandled error in listener for ${eventName}:`, err);
      }
    }
    return true;
  }
}

/**
 * Pipeline processor demonstrating generators and async iteration.
 */
class DataPipeline extends EventEmitter {
  constructor(stages = []) {
    super();
    this.stages = [...stages];
  }

  use(stageFn) {
    this.stages.push(stageFn);
    return this;
  }

  async *processStream(inputStream) {
    for await (const chunk of inputStream) {
      let current = chunk;
      let shouldYield = true;

      for (const [idx, stage] of this.stages.entries()) {
        try {
          current = await stage(current);
          if (current === undefined || current === null) {
            shouldYield = false;
            break;
          }
        } catch (err) {
          this.emit('error', { error: err, stageIndex: idx, item: chunk });
          shouldYield = false;
          break;
        }
      }

      if (shouldYield) {
        this.emit('item', current);
        yield current;
      }
    }
    this.emit('done');
  }
}

async function* generateNumbers(limit = 10) {
  for (let i = 1; i <= limit; i++) {
    await new Promise((res) => setTimeout(res, 10));
    yield i;
  }
}

async function main() {
  const pipeline = new DataPipeline([
    (x) => x * 2,
    (x) => (x % 3 === 0 ? null : x), // Filter out multiples of 3
    (x) => ({ value: x, timestamp: Date.now() }),
  ]);

  pipeline.on('item', (item) => console.log('Processed:', item));
  pipeline.on('done', () => console.log('Pipeline processing complete.'));

  for await (const result of pipeline.processStream(generateNumbers(8))) {
    // Collect or inspect results
  }
}

if (typeof module !== 'undefined' && require.main === module) {
  main().catch(console.error);
}

module.exports = { EventEmitter, DataPipeline };
