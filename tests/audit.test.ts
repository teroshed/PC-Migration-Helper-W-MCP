import test from 'node:test';
import assert from 'node:assert/strict';
import { allocatedState, auditProgress, auditRefreshKey, parseAuditStats, parseAuditStatus, parseNotificationFeed } from '../src/audit-model.ts';
import { notificationLocation, parseListing } from '../src/model.ts';
import type { AuditStats } from '../src/audit-model.ts';

function stats(patch: Partial<AuditStats> = {}): AuditStats {
  return { phase: 'idle', discoveredLeaves: 1, countedLeaves: 1, hashedLeaves: 0, failedLeaves: 0, logicalBytes: 1024, allocatedBytes: 4096, allocationUnknown: 0, totalKnown: true, complete: false, ...patch };
}
test('discovery never reports a percentage from a growing leaf denominator', () => {
  const growing = stats({ phase: 'discovering', discoveredLeaves: 50, countedLeaves: 50, totalKnown: false });
  assert.equal(auditProgress(growing).fraction, null);
  assert.equal(auditProgress(growing).total, null);
  assert.match(auditProgress(growing).label, /final total unknown/);
  assert.equal(auditProgress(stats({ phase: 'hashing', discoveredLeaves: 100, countedLeaves: 100, hashedLeaves: 25 }), 'hash').fraction, .25);
});
test('a completed empty scope finishes without inventing leaf files or dividing by zero', () => {
  const empty = stats({ phase: 'complete', discoveredLeaves: 0, countedLeaves: 0, hashedLeaves: 0, complete: true, logicalBytes: 0, allocatedBytes: 0 });
  assert.deepEqual(auditProgress(empty), { fraction: 1, processed: 0, total: 0, label: 'No leaf files in this completed scope.' });
  assert.deepEqual(allocatedState(empty), { kind: 'known', bytes: 0 });
});
test('allocated bytes remain independent from logical length and checksum completion', () => {
  const listing = parseListing({ path: 'E:\\Root', entries: [{ path: 'E:\\Root\\file', name: 'file', kind: 'file', size: 1048576, effective: { status: 'attention', own: false, inheritedFrom: null, stale: false, mixed: 0 } }], total: 1, attention: 1, counts: { attention: 1 } });
  assert.deepEqual(allocatedState(listing.entries[0]?.audit), { kind: 'unknown', bytes: null });
  assert.deepEqual(allocatedState(stats()), { kind: 'known', bytes: 4096 });
  assert.equal(stats().complete, false);
  assert.equal(stats().hashedLeaves, 0);
});
test('partial allocation and inaccessible coverage are not presented as full folder size', () => {
  assert.deepEqual(allocatedState(stats({ totalKnown: false })), { kind: 'partial', bytes: 4096 });
  assert.deepEqual(allocatedState(stats({ allocationUnknown: 1 })), { kind: 'partial', bytes: 4096 });
  assert.deepEqual(allocatedState(stats({ allocationUnknown: 1, allocatedBytes: 0 })), { kind: 'unknown', bytes: null });
  assert.equal(allocatedState(stats({ coverageGaps: 2 })).kind, 'partial');
  assert.equal(allocatedState(stats({ problem: 'Protected subtree not counted' })).kind, 'partial');
});
test('audit boundary retains checksum age/state and reports native error without accepting invalid sizes', () => {
  const native = { ...stats(), phase: 'error', error: 'Read failed', sha256: 'a'.repeat(64), verifiedAt: '2026-11-03T00:00:00Z', hashState: 'changed' };
  const parsed = parseAuditStats(native);
  assert.equal(parsed?.problem, 'Read failed');
  assert.equal(parsed?.verifiedAt, '2026-11-03T00:00:00Z');
  assert.equal(parsed?.hashState, 'changed');
  assert.equal(parsed?.sha256, 'a'.repeat(64));
  assert.throws(() => parseAuditStats({ ...stats(), allocatedBytes: -1 }), /allocated bytes/);
  assert.equal(parseAuditStats(undefined), undefined);
  assert.equal(parseAuditStatus({ ...stats(), notificationsUnread: undefined }).notificationsUnread, 0);
});
test('notification identity and safe surviving folder fence navigation to the correct source', () => {
  const feed = parseNotificationFeed({ items: [{ id: 42, title: 'Source changed', message: 'A file changed.', path: 'E:\\Root', originalPath: 'E:\\Root\\removed.txt', navigable: true, volumeId: 'source-volume', createdAt: '2026-11-03T00:00:00Z', readAt: null, changeCount: 3 }], total: 1, unread: 1 });
  const item = feed.items[0];
  assert.ok(item);
  assert.equal(item.id, '42');
  assert.deepEqual(notificationLocation(item, [{ id: 'source-volume', root: 'E:\\', label: 'Source', available: true, problem: null }]), { path: 'E:\\Root', selection: 'E:\\Root\\removed.txt' });
  assert.equal(notificationLocation(item, [{ id: 'different-volume', root: 'E:\\', label: 'Other disk', available: true, problem: null }]), null);
  assert.equal(notificationLocation({ ...item, path: 'F:\\Elsewhere' }, [{ id: 'source-volume', root: 'E:\\', label: 'Source', available: true, problem: null }]), null);
  assert.equal(notificationLocation({ ...item, navigable: false }, [{ id: 'source-volume', root: 'E:\\', label: 'Source', available: true, problem: null }]), null);
});
test('row refresh keys react to audit evidence, not every current-file cursor change', () => {
  const first = parseAuditStatus({ ...stats(), workers: 4, currentPath: 'E:\\one', changeSequence: 1 });
  assert.equal(auditRefreshKey(first), auditRefreshKey({ ...first, currentPath: 'E:\\two' }));
  assert.notEqual(auditRefreshKey(first), auditRefreshKey({ ...first, hashedLeaves: 1 }));
  assert.notEqual(auditRefreshKey(first), auditRefreshKey({ ...first, changeSequence: 2 }));
});
