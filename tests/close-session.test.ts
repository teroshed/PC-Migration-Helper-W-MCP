import test from 'node:test';
import assert from 'node:assert/strict';
import { assertNewCloseAllowed, CloseSession } from '../src/close-session.ts';
import type { CloseServices } from '../src/close-session.ts';
import { MutationQueue, parseDocument } from '../src/model.ts';

function services(events: string[]): CloseServices {
  return {
    status: async () => ({ enabled: false, ready: false }),
    begin: async () => { events.push('freeze admission'); return 7; },
    flush: async () => { events.push('save drafts'); },
    hide: async () => { events.push('hide'); },
    confirm: async ticket => { events.push('stop and join ' + ticket); },
    cancel: async ticket => { events.push('cancel ' + ticket); },
  };
}

test('new Close or Exit cannot substitute for an exact-ticket retry after admission changes', async () => {
  const idle = { phase: 'idle', ticket: null } as const;
  assert.doesNotThrow(() => assertNewCloseAllowed(idle, { pending: null, draining: false }));
  assert.throws(() => assertNewCloseAllowed({ phase: 'waiting', ticket: 7 }, { pending: { ticket: 7 }, draining: false }), /Retry Exit or Cancel Exit/);
  // Native admission can change while a status response is in flight, even if
  // the UI has not yet received the corresponding close event.
  assert.throws(() => assertNewCloseAllowed(idle, { pending: { ticket: 8 }, draining: false }), /Retry Exit or Cancel Exit/);
  assert.throws(() => assertNewCloseAllowed(idle, { pending: null, draining: true }), /Retry Exit or Cancel Exit/);
  assert.throws(() => assertNewCloseAllowed({ phase: 'stopping', ticket: 9 }, { pending: null, draining: false }), /Retry Exit or Cancel Exit/);
});

test('normal close freezes starts before draft saving and joins only after acknowledgement', async () => {
  const events: string[] = [];
  let release: () => void = () => {};
  const saved = new Promise<void>(resolve => { release = resolve; });
  const operations = services(events);
  operations.flush = async () => { events.push('save drafts'); await saved; };
  const session = new CloseSession(operations, () => {});
  const first = session.request();
  assert.equal(session.request(), first);
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.deepEqual(events, ['freeze admission', 'save drafts']);
  assert.equal(session.snapshot().phase, 'saving');
  release(); await first;
  assert.deepEqual(events, ['freeze admission', 'save drafts', 'stop and join 7']);
  assert.equal(session.snapshot().phase, 'stopping');
});

test('failed saves retain exact pending requests and cancel exit without stopping workers', async () => {
  const events: string[] = [];
  const document = parseDocument({ schemaVersion: 1, revision: 1, history: [], decisions: {}, view: { path: 'E:\\', recentPaths: [] } });
  const writer = new MutationQueue(document, async () => { throw new Error('saved revision conflict'); }, () => {});
  void writer.request({ type: 'note', path: 'E:\\fixture', notes: 'Retain this draft' });
  await new Promise(resolve => setTimeout(resolve, 0));
  const requestId = writer.retainedRequests[0]?.id;
  const operations = services(events);
  operations.flush = () => writer.flush();
  const session = new CloseSession(operations, () => {});
  await assert.rejects(session.request(), /revision conflict/);
  assert.equal(writer.retainedRequests[0]?.id, requestId);
  assert.equal(writer.retainedRequests.length, 1);
  assert.deepEqual(events, ['freeze admission', 'cancel 7']);
  assert.equal(session.snapshot().phase, 'idle');
});

test('a join timeout retains the ticket and a retry never reopens job admission', async () => {
  const events: string[] = [];
  const operations = services(events);
  let confirms = 0;
  operations.confirm = async ticket => { events.push('confirm ' + ticket); if (++confirms === 1) throw new Error('checkpoint timeout'); };
  const session = new CloseSession(operations, () => {});
  await assert.rejects(session.request(), /checkpoint timeout/);
  assert.deepEqual(session.snapshot(), { phase: 'waiting', ticket: 7 });
  await session.request();
  assert.deepEqual(events, ['freeze admission', 'save drafts', 'confirm 7', 'save drafts', 'confirm 7']);
});

test('a failed save on retry cannot silently cancel an already-started exit', async () => {
  const events: string[] = [];
  const operations = services(events);
  operations.confirm = async () => { throw new Error('checkpoint timeout'); };
  const session = new CloseSession(operations, () => {});
  await assert.rejects(session.request(), /checkpoint timeout/);
  operations.flush = async () => { throw new Error('retained save failed'); };
  await assert.rejects(session.request(), /retained save failed/);
  assert.deepEqual(session.snapshot(), { phase: 'waiting', ticket: 7 });
  assert.ok(!events.includes('cancel 7'));
});

test('only explicit usable background mode hides; explicit Exit always drains', async () => {
  const events: string[] = [];
  const operations = services(events);
  operations.status = async () => ({ enabled: true, ready: true });
  const session = new CloseSession(operations, () => {});
  await session.request();
  assert.deepEqual(events, ['save drafts', 'hide']);
  await session.request(9);
  assert.deepEqual(events.slice(2), ['save drafts', 'stop and join 9']);
});

test('unavailable tray falls back to safe exit and a timed-out exit can be explicitly canceled', async () => {
  const events: string[] = [];
  const operations = services(events);
  operations.status = async () => ({ enabled: true, ready: false });
  operations.confirm = async () => { throw new Error('still stopping'); };
  const session = new CloseSession(operations, () => {});
  await assert.rejects(session.request(), /still stopping/);
  assert.ok(!events.includes('hide'));
  await session.cancel();
  assert.deepEqual(session.snapshot(), { phase: 'idle', ticket: null });
  assert.equal(events.at(-1), 'cancel 7');
});
