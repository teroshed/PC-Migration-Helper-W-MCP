import test from 'node:test';
import assert from 'node:assert/strict';
import { performance } from 'node:perf_hooks';
import { ReadCache, StaleReadError, folderQueryKey, reviewContentKey } from '../src/read-cache.ts';

test('repeated navigation reuses pages and concurrent reads share one native call', async () => {
  const cache = new ReadCache<object>();
  let calls = 0;
  const load = async () => { calls++; return { observedAt: 'original', entries: ['one'] }; };
  const [a, b] = await Promise.all([cache.read('A', load), cache.read('A', load)]);
  assert.equal(a, b);
  await cache.read('B', load);
  assert.equal(await cache.read('A', load), a);
  assert.equal(calls, 2);
});

test('scope changes, explicit refresh and late completions cannot repopulate older data', async () => {
  const cache = new ReadCache<string>();
  let complete!: (value: string) => void;
  const old = cache.read('A', () => new Promise(resolve => { complete = resolve; }));
  await Promise.resolve();
  cache.setScope('new journal/audit/source');
  assert.equal(await cache.read('A', async () => 'new'), 'new');
  const canceled = assert.rejects(old, StaleReadError);
  complete('old'); await canceled;
  assert.equal(cache.peek('A'), 'new');
  cache.clear();
  assert.equal(cache.peek('A'), undefined);
});

test('a superseded request rejects before its loader starts', async () => {
  const cache = new ReadCache<string>();
  let nativeCalls = 0;
  const old = cache.read('A', async () => { nativeCalls += 1; return 'obsolete'; });
  cache.clear();
  await assert.rejects(old, StaleReadError);
  assert.equal(nativeCalls, 0);
});

test('progress expires future cache reuse without starving a coherent in-flight source read', async () => {
  const cache = new ReadCache<string>();
  let complete: (value: string) => void = () => {};
  const pending = cache.read('A', () => new Promise(resolve => { complete = resolve; }));
  await Promise.resolve();
  cache.expireValues();
  complete('observed snapshot');
  assert.equal(await pending, 'observed snapshot');
  assert.equal(cache.peek('A'), undefined);
  assert.equal(await cache.read('A', async () => 'current progress'), 'current progress');
});

test('expired, evicted and oversized values are not reused; errors can be retried', async () => {
  let now = 0;
  const cache = new ReadCache<string>(2, 100, 10, () => now);
  await cache.read('A', async () => 'a'); await cache.read('B', async () => 'b');
  cache.peek('A'); await cache.read('C', async () => 'c');
  assert.equal(cache.peek('B'), undefined);
  now = 10; assert.equal(cache.peek('A'), undefined);
  await cache.read('huge', async () => 'x'.repeat(100));
  assert.equal(cache.peek('huge'), undefined);
  await assert.rejects(cache.read('bad', async () => { throw new Error('offline'); }));
  assert.equal(await cache.read('bad', async () => 'recovered'), 'recovered');
});

test('page/filter/search/path keys are isolated; journal view writes alone preserve reuse', () => {
  const query = { path: 'E:\\fixture', search: '', filter: 'all', page: 0 };
  const original = folderQueryKey(query);
  for (const change of [{ path: 'F:\\fixture' }, { search: 'a' }, { filter: 'attention' }, { page: 1 }]) {
    assert.notEqual(folderQueryKey({ ...query, ...change }), original);
  }
  const doc = { decisions: { a: { status: 'attention' } }, history: [], revision: 1, view: { path: 'A' } };
  const key = reviewContentKey(doc);
  assert.equal(reviewContentKey({ ...doc, revision: 2, updatedAt: 'later', view: { path: 'B' }, recentOperations: ['view'], viewProvenance: { source: 'user' }, agentOperations: {} }), key);
  for (const change of [{ decisions: {} }, { history: ['undo'] }, { suggestions: ['proposed'] }, { volumes: { E: 'offline' } }, { futureField: 1 }]) {
    assert.notEqual(reviewContentKey({ ...doc, ...change }), key);
  }
});

test('synthetic 30-navigation fixture measures eliminated repeated read work', async t => {
  const paths = Array.from({ length: 30 }, (_, n) => `folder-${n % 3}`);
  let nativeCalls = 0;
  const load = async () => {
    nativeCalls++;
    await new Promise(resolve => setTimeout(resolve, 8));
    return { entries: Array.from({ length: 100 }, (_, i) => ({ name: `file-${i}`, effective: { status: 'attention' } })), observedAt: 'fixture-original' };
  };
  let start = performance.now();
  for (const _ of paths) await load();
  const uncachedMs = performance.now() - start;
  assert.equal(nativeCalls, 30);
  nativeCalls = 0;
  const cache = new ReadCache<Awaited<ReturnType<typeof load>>>();
  start = performance.now();
  for (const path of paths) await cache.read(path, load);
  const cachedMs = performance.now() - start;
  assert.equal(nativeCalls, 3);
  t.diagnostic(JSON.stringify({ synthetic: true, navigationCount: 30, uncachedCalls: 30, cachedCalls: 3, uncachedMs, cachedMs }));
});

import { FolderReadCache } from '../src/folder-read-cache.ts';
import { parseAuditStatus } from '../src/audit-model.ts';

function deferred<T>(): { promise: Promise<T>; resolve: (value: T) => void } {
  let resolve: (value: T) => void = () => { throw new Error('Deferred promise is not initialized'); };
  const promise = new Promise<T>(complete => { resolve = complete; });
  return { promise, resolve };
}
const listing = (path: string, total: number) => ({ path, entries: [], total, attention: 0, counts: {}, observedAt: 'source metadata' });

test('a watcher generation cannot share or display an older in-flight listing', async () => {
  const oldNative = deferred<ReturnType<typeof listing>>();
  let calls = 0;
  const cache = new FolderReadCache(async request => ++calls === 1 ? oldNative.promise : listing(request.path, 2));
  const query = { path: 'A', search: '', filter: 'all', page: 0 };
  const old = cache.listing(query);
  await Promise.resolve();
  cache.setSourceScope('new watcher generation');
  const fresh = await cache.listing(query);
  assert.equal(fresh.total, 2);
  const canceled = assert.rejects(old, StaleReadError);
  oldNative.resolve(listing('A', 1)); await canceled;
  assert.equal(cache.peekListing(query)?.total, 2);
  assert.equal(calls, 2);
});

test('different filters share directory enumeration but obtain their own derived result', async () => {
  const metadata = deferred<ReturnType<typeof listing>>();
  const calls: boolean[] = [];
  const cache = new FolderReadCache(async request => {
    calls.push(request.refresh);
    return request.refresh ? metadata.promise : listing(request.path, 3);
  });
  const query = { path: 'A', search: '', filter: 'all', page: 0 };
  const all = cache.listing(query);
  const attention = cache.listing({ ...query, filter: 'attention' });
  await Promise.resolve();
  assert.deepEqual(calls, [true]);
  metadata.resolve(listing('A', 7));
  assert.equal((await all).total, 7);
  assert.equal((await attention).total, 3);
  assert.deepEqual(calls, [true, false]);
});

test('refreshing A supersedes old A without canceling or evicting independent B', async () => {
  const firstA = deferred<ReturnType<typeof listing>>();
  const firstB = deferred<ReturnType<typeof listing>>();
  const cache = new FolderReadCache(async request => request.path === 'B' ? firstB.promise : request.filter === 'all' ? firstA.promise : listing('A', 3));
  const A = { path: 'A', search: '', filter: 'all', page: 0 };
  const B = { ...A, path: 'B' };
  const old = cache.listing(A);
  const independent = cache.listing(B);
  await Promise.resolve();
  const refreshed = await cache.listing({ ...A, filter: 'attention' }, true);
  firstB.resolve(listing('B', 2));
  assert.equal((await independent).total, 2);
  const canceled = assert.rejects(old, StaleReadError);
  firstA.resolve(listing('A', 1)); await canceled;
  assert.equal(refreshed.total, 3);
  assert.equal(cache.peekListing(B)?.total, 2);
});

test('TTL expiry cannot reuse a derived-only request started from older metadata', async () => {
  let now = 0;
  const oldDerived = deferred<ReturnType<typeof listing>>();
  const requests: boolean[] = [];
  const cache = new FolderReadCache(async request => {
    requests.push(request.refresh);
    return !request.refresh ? oldDerived.promise : listing(request.path, requests.length);
  }, () => now);
  const query = { path: 'A', search: '', filter: 'all', page: 0 };
  await cache.listing(query);
  now = 14_000; cache.setScope('changed review');
  const old = cache.listing(query); await Promise.resolve();
  now = 15_001;
  assert.equal((await cache.listing(query)).total, 3);
  const canceled = assert.rejects(old, StaleReadError);
  oldDerived.resolve(listing('A', 99)); await canceled;
  assert.deepEqual(requests, [true, false, true]);
});

test('a decision change reuses in-flight metadata but recomputes effective decisions', async () => {
  const source = deferred<ReturnType<typeof listing>>();
  const requests: boolean[] = [];
  const cache = new FolderReadCache(async request => {
    requests.push(request.refresh);
    return request.refresh ? source.promise : listing(request.path, 2);
  });
  const query = { path: 'A', search: '', filter: 'all', page: 0 };
  const old = cache.listing(query); await Promise.resolve();
  cache.setScope('new decision');
  const current = cache.listing(query);
  const canceled = assert.rejects(old, StaleReadError);
  source.resolve(listing('A', 1));
  await canceled;
  assert.equal((await current).total, 2);
  assert.deepEqual(requests, [true, false]);
});

test('source metadata refresh is separate from view, decision and hash-progress invalidation', async () => {
  let now = 0;
  let sourceSize = 10;
  let indexedSize = 0;
  const refreshes: boolean[] = [];
  const query = { path: 'E:\\fixture', search: '', filter: 'all', page: 0 };
  const cache = new FolderReadCache(async request => {
    refreshes.push(request.refresh);
    if (request.refresh) indexedSize = sourceSize;
    return { path: request.path, entries: [], total: indexedSize, attention: 0, counts: {}, observedAt: String(now) };
  }, () => now);
  assert.equal((await cache.listing(query)).total, 10);
  await cache.listing(query);
  assert.deepEqual(refreshes, [true]);
  cache.setScope('new decision or hash progress');
  await cache.listing(query);
  assert.deepEqual(refreshes, [true, false]);
  sourceSize = 20;
  cache.setSourceScope('watcher event 1');
  assert.equal(cache.peekListing(query), undefined);
  assert.equal((await cache.listing(query)).total, 20);
  assert.deepEqual(refreshes, [true, false, true]);
  sourceSize = 30; now = 15_000;
  assert.equal(cache.peekListing(query), undefined);
  assert.equal((await cache.listing(query)).total, 30);
  await cache.listing(query, true);
  assert.deepEqual(refreshes, [true, false, true, true, true]);
});

test('metadata TTL does not slide with repeated derived reads; refreshed siblings keep independent snapshots', async () => {
  let now = 0;
  const calls: { path: string; refresh: boolean }[] = [];
  const cache = new FolderReadCache(async request => {
    calls.push(request);
    return { path: request.path, entries: [], total: 0, attention: 0, counts: {}, observedAt: String(now) };
  }, () => now);
  const A = { path: 'A', search: '', filter: 'all', page: 0 };
  const B = { ...A, path: 'B' };
  await Promise.all([cache.listing(A), cache.listing(A)]);
  await cache.listing(B); await cache.listing(A);
  assert.equal(calls.length, 2);
  now = 14_000; cache.setScope('hash progress'); await cache.listing(A);
  assert.equal(calls.at(-1)?.refresh, false);
  now = 15_001; await cache.listing(A);
  assert.equal(calls.at(-1)?.refresh, true);
});

test('audit boundary separates watcher sequence and durable checkpoint counters with honest legacy fallback', () => {
  const base = { phase: 'idle', discoveredLeaves: 0, countedLeaves: 0, hashedLeaves: 7, failedLeaves: 0, logicalBytes: 0, allocatedBytes: 0, allocationUnknown: 0, totalKnown: false };
  const legacy = parseAuditStatus(base);
  assert.equal(legacy.savedHashFiles, 7); assert.equal(legacy.pendingHashFiles, 0); assert.equal(legacy.activeHashFiles, 0);
  assert.equal(legacy.hashCheckpointUnit, undefined); assert.equal(legacy.watcherChangeSequence, undefined);
  const current = parseAuditStatus({ ...base, watcherChangeSequence: 5, savedHashFiles: 6, pendingHashFiles: 3, activeHashFiles: 2, hashCheckpointUnit: 'completed-file' });
  assert.equal(current.watcherChangeSequence, 5); assert.equal(current.savedHashFiles, 6);
  assert.equal(current.pendingHashFiles, 3); assert.equal(current.activeHashFiles, 2); assert.equal(current.hashCheckpointUnit, 'completed-file');
  assert.throws(() => parseAuditStatus({ ...base, watcherChangeSequence: -1 }));
});
