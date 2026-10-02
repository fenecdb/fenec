// The part of IndexedDB `persist`, `restore` and the sync layer use, in
// memory, for the tests: Node has none. Keys are kept sorted as the real
// one keeps them. web/fenec.persist.test.js has its own, which logs writes.

export class KeyRange {
  constructor(lo, hi) {
    this.lo = lo;
    this.hi = hi;
  }
  static bound(lo, hi) {
    return new KeyRange(lo, hi);
  }
  has(k) {
    return k >= this.lo && k <= this.hi;
  }
}

export function fakeIndexedDB() {
  const records = new Map();
  const request = (fn) => {
    const req = {};
    queueMicrotask(() => {
      req.result = fn();
      req.onsuccess?.();
    });
    return req;
  };
  const store = {
    put: (value, key) => records.set(key, value),
    delete(range) {
      for (const k of [...records.keys()]) if (range.has(k)) records.delete(k);
    },
    get: (key) => request(() => records.get(key)),
    getAll: (range) => request(() => [...records.keys()].filter((k) => range.has(k)).sort().map((k) => records.get(k))),
  };
  const db = {
    transaction() {
      const tx = { objectStore: () => store };
      setImmediate(() => tx.oncomplete?.());
      return tx;
    },
  };
  return {
    records,
    open() {
      const req = {};
      queueMicrotask(() => {
        req.result = db;
        req.onsuccess?.();
      });
      return req;
    },
  };
}

/** Installs a fresh one as the global, with `IDBKeyRange`; returns it. */
export function installIndexedDB() {
  const idb = fakeIndexedDB();
  globalThis.indexedDB = idb;
  globalThis.IDBKeyRange = KeyRange;
  return idb;
}
