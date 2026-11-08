import test from 'node:test';
import assert from 'node:assert/strict';
import { inventoryElapsed, inventoryProgressLabel, parseInventoryProgress } from '../src/assistance-progress.ts';

test('inventory counts completed and current files, without guessing a percentage', () => {
  const progress = parseInventoryProgress({ processed: 12, pending: 20, active: 4, workers: 4,
    files: 100, currentFiles: 8, bytes: 200, currentBytes: 16, gaps: 2 });
  assert.equal(progress.files, 108);
  assert.equal(progress.bytes, 216);
  assert.equal(progress.gaps, 2);
  assert.equal(inventoryProgressLabel(progress), '12 directories processed, 20 queued, 4 active. Final total unknown.');
  assert.doesNotMatch(inventoryProgressLabel(progress), /%/);
});

test('older snapshots do not present a missing queue count as zero', () => {
  const progress = parseInventoryProgress({ processed: 12 });
  assert.equal(progress.pending, null);
  assert.equal(inventoryProgressLabel(progress), '12 directories processed. Final total unknown.');
  assert.equal(parseInventoryProgress({ pending: 0 }).pending, 0);
});

test('malformed counters and times are not shown as valid progress', () => {
  const progress = parseInventoryProgress({ processed: -1, active: 2.3, workers: '4', files: NaN,
    currentBytes: Infinity, startedAt: 'not a date', currentPath: false });
  assert.equal(progress.processed, 0);
  assert.equal(progress.active, 0);
  assert.equal(progress.workers, 0);
  assert.equal(progress.files, 0);
  assert.equal(progress.bytes, 0);
  assert.equal(progress.startedAt, null);
  assert.equal(progress.currentPath, null);
});

test('elapsed means time since scan start, including pauses, never ETA', () => {
  const start = '2026-11-08T00:00:00Z';
  const time = Date.parse(start);
  assert.equal(inventoryElapsed(start, time + 65000), '1m 5s');
  assert.equal(inventoryElapsed(start, time + 3660000), '1h 1m');
  assert.equal(inventoryElapsed(start, time - 1000), '0s');
  assert.equal(inventoryElapsed(null, time), null);
});
