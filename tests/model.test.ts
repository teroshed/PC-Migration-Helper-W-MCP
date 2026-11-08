import test from 'node:test';
import assert from 'node:assert/strict';
import { MutationQueue, parseDocument, parentPath, normalizeReviewPath, parseEffective, parseGateway, previewBranch, closePreviewBranch } from '../src/model.ts';
import type { Document, Operation } from '../src/model.ts';
import { changeSelection, latestUndoEvent, loadSelection, reportSelectionTarget, replaceSelection,
  resolvedSelection, rootForSelection, savedGroup, selectedFoldersWithDescendants, selectedFor,
  selectionKey, selectionSignature, selectionTarget } from '../src/review-selection.ts';
import { inspectPending, pendingFingerprint, pendingJSON, pendingMetadata, textChunk, textHash } from '../src/pending-inspection.ts';
import type { PendingSnapshot } from '../src/pending-inspection.ts';

function doc(revision = 1): Document {
  return parseDocument({ schemaVersion: 1, revision, history: [], decisions: {}, futureField: { preserved: true }, view: { path: 'E:\\', selectedPath: null, search: '', filter: 'all', page: 0, scrollTop: 0, recentPaths: [] } });
}

test('selection uses volume identity across locations, filters and alternate path casing', () => {
  const volume = { root: 'E:\\Fixture', id: 'volume-a', label: 'Synthetic', available: true, problem: null };
  const other = { ...volume, root: 'F:\\', id: 'volume-b' };
  assert.equal(rootForSelection('E:\\Fixture\\Nested', [volume, other]), volume);
  assert.equal(rootForSelection('E:\\FixtureOther', [volume, other]), undefined);
  const file = selectionTarget({ path: 'E:/Fixture/Nested/Test.txt', kind: 'file', fingerprint: 'first', problem: null }, volume)!;
  assert.deepEqual(file, { path: 'E:\\Fixture\\Nested\\Test.txt', volumeId: 'volume-a', relativePath: 'Nested\\Test.txt', kind: 'file', fingerprint: 'first' });
  const differentCase = { ...file, relativePath: 'nested\\test.TXT', path: 'E:\\fixture\\nested\\test.TXT' };
  assert.equal(selectionKey(file), selectionKey(differentCase));
  const selected = changeSelection([], file);
  assert.deepEqual(changeSelection(selected, differentCase), []);
  const sets = replaceSelection(replaceSelection(new Map(), other.id, [{ ...file, volumeId: other.id }]), volume.id, selected);
  const restored = loadSelection(JSON.stringify(Object.fromEntries(sets)));
  assert.deepEqual(selectedFor(restored, volume.id), selected);
  assert.equal(selectedFor(restored, other.id).length, 1);
  assert.deepEqual(resolvedSelection(selected, other), []);
  assert.equal(resolvedSelection(selected, { ...volume, root: 'G:\\' })[0]?.path, 'G:\\Nested\\Test.txt');
  assert.equal(selectionTarget({ ...file, kind: 'link', problem: null }, volume), undefined);
  assert.equal(selectionTarget({ ...file, path: 'E:\\Fixture\\..\\Test.txt', problem: null }, volume), undefined);
});

test('selection snapshots bound the target count and preserve reviewed fingerprints and ancestry', () => {
  const target = { volumeId: 'volume-a', kind: 'file' as const, path: 'E:\\Parent\\a', relativePath: 'Parent\\a', fingerprint: 'first' };
  const parent = { ...target, kind: 'folder' as const, path: 'E:\\Parent', relativePath: 'Parent' };
  const sibling = { ...parent, path: 'E:\\ParentElse', relativePath: 'ParentElse' };
  const targets = [target, parent, sibling];
  assert.deepEqual([...selectedFoldersWithDescendants(targets)], ['volume-a|parent']);
  const signature = selectionSignature(target.volumeId, targets);
  assert.equal(selectionSignature(target.volumeId, [...targets].reverse()), signature);
  assert.notEqual(selectionSignature(target.volumeId, changeSelection(targets, { ...target, fingerprint: 'changed' }, true)), signature);
  const atLimit = Array.from({ length: 128 }, (_, index) => ({ ...target, relativePath: 'f-' + index }));
  assert.throws(() => changeSelection(atLimit, target), /128/);
  const long = { ...target, path: 'E:\\' + '🙂'.repeat(520) };
  assert.deepEqual(reportSelectionTarget(long), { volumeId: 'volume-a', kind: 'file', fingerprint: 'first',
    path: null, relativePath: null, pathOmitted: true });
  assert.ok(long.path.endsWith('🙂'));
});

test('schema two preserves exact group history and never treats note-only as a restored mark', () => {
  const target = { volumeId: 'volume-a', relativePath: 'leaf', kind: 'file', path: 'E:\\leaf', fingerprint: 'current' };
  const first = { id: 'earlier', type: 'classify' };
  const group = { id: 'reviewed-group', type: 'bulkClassify', memberCount: 1,
    members: [{ key: 'volume-a|leaf', target, before: { status: 'attention', noteOnly: true }, after: { status: 'manual-pending' } }] };
  const document = parseDocument({ ...doc(), schemaVersion: 2, history: [first, group] });
  assert.equal(document.schemaVersion, 2);
  assert.deepEqual(latestUndoEvent(document), group);
  assert.deepEqual(savedGroup(group), { id: 'reviewed-group', memberCount: 1,
    members: [{ target, before: null, after: 'manual-pending' }] });
  assert.equal(savedGroup({ ...group, memberCount: 2 }), null);
  assert.equal(savedGroup({ ...group, undoneAt: 'fixture' }), null);
  assert.deepEqual(latestUndoEvent({ ...document, history: [first, { ...group, undoneAt: 'fixture' }] }), first);
  assert.throws(() => parseDocument({ ...document, schemaVersion: 3 }), /Unsupported/);
});

test('folder rollups preserve zero-entry coverage gaps without becoming saved marks', () => {
  const result = parseEffective({
    status: 'attention', own: false, stale: false, mixed: 0,
    rollup: { kind: 'unknown', childCount: 0, leafCount: 0, emptyFolderCount: 0, undecidedCount: 0,
      unknownBranches: 1, coverageGaps: 1, statusCounts: {}, coverageComplete: false, problem: 'Membership needs a fresh observation' },
  });
  assert.equal(result.status, 'attention');
  assert.equal(result.own, false);
  assert.equal(result.rollup?.kind, 'unknown');
  assert.equal(result.rollup?.childCount, 0);
  assert.deepEqual(result.rollup?.coverage, {
    complete: false, leafCount: 0, emptyFolderCount: 0, undecidedCount: 0, unknownBranches: 1, coverageGaps: 1,
    statusCounts: {}, problem: 'Membership needs a fresh observation',
  });
});

test('observed undecided entries remain distinct from unknown branches and legacy summaries still parse', () => {
  const base = { status: 'attention', own: false, stale: false, mixed: 0 };
  const observed = { kind: 'unknown', childCount: 3, leafCount: 2, emptyFolderCount: 1, undecidedCount: 2,
    unknownBranches: 0, coverageGaps: 0, statusCounts: { 'migrate-now': 1, erase: -1, unsupported: 2 }, coverageComplete: true };
  const coverage = parseEffective({ ...base, rollup: observed }).rollup?.coverage;
  assert.equal(coverage?.complete, true);
  assert.equal(coverage?.undecidedCount, 2);
  assert.deepEqual(coverage?.statusCounts, { 'migrate-now': 1 });
  assert.equal(parseEffective({ ...base, rollup: { ...observed, coverageGaps: 1 } }).rollup?.coverage?.complete, false);
  assert.equal(parseEffective({ ...base, rollup: { ...observed, leafCount: -1 } }).rollup?.coverage, undefined);
  assert.deepEqual(parseEffective({ ...base, rollup: { kind: 'mixed', childCount: 2 } }).rollup, { kind: 'mixed', childCount: 2 });
});

test('large pending queues retain all text while reports and UTF-8 inspection stay bounded', async () => {
  const requests = Array.from({ length: 24 }, (_, index) => ({ id: 'retained-' + index, operation: {
    type: 'note', path: 'E:\\synthetic\\' + index, notes: '🙂é漢字'.repeat(10_000), unknownField: { preserved: true },
  } }));
  const snapshot: PendingSnapshot = { writer: { revision: 7, retainedRequests: requests }, drafts: {}, requests };
  assert.ok(new TextEncoder().encode(pendingJSON(snapshot)).length > 2 * 1024 * 1024);
  assert.ok(JSON.stringify(pendingMetadata(requests)).length < 10_000);
  const fingerprint = await pendingFingerprint(snapshot);
  const listed = await inspectPending(() => snapshot, { action: 'listPending', offset: 16, limit: 8, expectedPendingFingerprint: fingerprint });
  assert.equal(listed.total, 24);
  assert.equal(listed.nextOffset, null);
  assert.ok(JSON.stringify(listed).length < 64 * 1024);
  let offset = 0, text = '', hash = '';
  do {
    const chunk = await inspectPending(() => snapshot, { action: 'readPendingText', operationId: requests[0]!.id, offset, limit: 16_384, expectedPendingFingerprint: fingerprint });
    assert.equal(chunk.pendingFingerprint, fingerprint);
    assert.ok(JSON.stringify(chunk).length < 64 * 1024);
    if (hash) assert.equal(chunk.sha256, hash);
    hash = String(chunk.sha256); text += String(chunk.text);
    offset = chunk.nextOffset === null ? -1 : Number(chunk.nextOffset);
  } while (offset >= 0);
  assert.deepEqual(JSON.parse(text), requests[0]);
  assert.equal(hash, await textHash(pendingJSON(requests[0])));
  assert.equal(requests.length, 24);
});

test('pending inspection rejects changed fingerprints and mid-character UTF-8 offsets', async () => {
  const requests = [{ id: 'first', operation: { type: 'note', notes: 'Keep 🙂é' } }];
  const before: PendingSnapshot = { writer: { retainedRequests: requests }, drafts: {}, requests };
  const after: PendingSnapshot = { writer: { retainedRequests: [] }, drafts: {}, requests: [] };
  const fingerprint = await pendingFingerprint(before);
  let reads = 0;
  await assert.rejects(inspectPending(() => reads++ === 0 ? before : after, { action: 'listPending', offset: 0, limit: 1, expectedPendingFingerprint: fingerprint }), /changed during inspection/);
  await assert.rejects(inspectPending(() => after, { action: 'listPending', offset: 0, limit: 1, expectedPendingFingerprint: fingerprint }), /changed/);
  assert.throws(() => textChunk('A🙂éB', 2, 4), /UTF-8 boundary/);
  assert.deepEqual(textChunk('A🙂éB', 1, 4), { text: '🙂', totalBytes: 8, nextOffset: 5 });
});

test('native copy revisions are adopted before later retained edits drain', async () => {
  const revisions: number[] = [];
  const queue = new MutationQueue(doc(4), async (revision, id) => {
    revisions.push(revision);
    return { ...doc(revision + 1), recentOperations: [id] };
  }, () => undefined);
  queue.holdForExternal();
  const pending = queue.request({ type: 'note', path: 'E:\\work', notes: 'Typed while copy control was saving its receipt' });
  const retainedId = queue.retainedRequests[0]?.id;
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.deepEqual(revisions, []);
  assert.equal(queue.acceptExternal(doc(6)), false);
  queue.finishExternal(doc(6));
  await pending;
  assert.deepEqual(revisions, [6]);
  assert.ok(retainedId);
  assert.equal(queue.revision, 7);
});

test('uncertain native revision reconciliation retains queued edits without replay', async () => {
  const revisions: number[] = [];
  const queue = new MutationQueue(doc(4), async revision => { revisions.push(revision); return doc(revision + 1); }, () => undefined);
  queue.holdForExternal();
  void queue.request({ type: 'classify', path: 'E:\\work', status: 'manual-pending' });
  queue.failExternal('latest journal cannot be read');
  await assert.rejects(queue.flush(), /needs reconciliation/);
  assert.equal(queue.retainedRequests.length, 1);
  assert.deepEqual(revisions, []);
  assert.equal(queue.reconcileExternal(doc(5)), false);
  assert.throws(() => queue.holdForExternal(), /Finish pending/);
});
test('a reread resolves an empty writer after a lost group acknowledgement', async () => {
  const queue = new MutationQueue(doc(4), async () => { throw new Error('No mutation was expected.'); }, () => undefined);
  queue.holdForExternal();
  queue.failExternal('Acknowledgement and journal reread unavailable');
  assert.equal(queue.needsExternalReconciliation, true);
  assert.equal(queue.reconcileExternal(doc(3)), false);
  const recovered = { ...doc(5), schemaVersion: 2, history: [{ id: 'reviewed-group', type: 'bulkClassify' }] };
  assert.equal(queue.reconcileExternal(recovered), true);
  assert.equal(queue.revision, 5);
  assert.equal(queue.idle, true);
  assert.equal(queue.needsExternalReconciliation, false);
  await queue.flush();
});
test('serial native saves use the last acknowledged revision during rapid edits', async () => {
  const seen: Array<{ revision: number; operation: Operation }> = [];
  let native = doc();
  const queue = new MutationQueue(native, async (revision, id, operation) => {
    assert.equal(revision, native.revision);
    seen.push({ revision, operation });
    await new Promise(resolve => setTimeout(resolve, 4));
    native = { ...native, revision: revision + 1 };
    return native;
  }, () => undefined);
  const first = queue.request({ type: 'note', path: 'E:\\work', notes: 'A' });
  const second = queue.request({ type: 'note', path: 'E:\\work', notes: 'A newer draft' });
  const third = queue.request({ type: 'classify', path: 'E:\\work', status: 'last-moment' });
  await queue.flush();
  await Promise.all([first, second, third]);
  assert.deepEqual(seen.map(item => item.revision), [1, 2, 3]);
  assert.deepEqual(seen.map(item => item.operation.type), ['note', 'note', 'classify']);
  assert.equal(queue.state.pending, 0);
  assert.equal(native.revision, 4);
});
test('failed acknowledgements retain pending edits and retry the same idempotency key', async () => {
  let fail = true;
  const ids: string[] = [];
  const queue = new MutationQueue(doc(), async (revision, id) => {
    ids.push(id);
    if (fail) throw new Error('Fixture write failure');
    return doc(revision + 1);
  }, () => undefined);
  const saved = queue.request({ type: 'note', path: 'E:\\work', notes: 'Keep this draft' });
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.equal(queue.state.pending, 1);
  assert.equal(queue.state.error, 'Fixture write failure');
  await assert.rejects(queue.flush(), /Fixture write failure/);
  fail = false;
  queue.retry();
  await saved;
  await queue.flush();
  assert.equal(ids[0], ids[1]);
  assert.equal(queue.state.error, null);
});
test('saved boundary rejects malformed decisions and preserves future fields', () => {
  assert.deepEqual(doc().futureField, { preserved: true });
  assert.throws(() => parseDocument({ ...doc(), decisions: { path: { path: 'E:\\work', status: 'green-means-verified' } } }), /unsupported/);
  assert.throws(() => parseDocument({ ...doc(), revision: '1' }), /Unsupported/);
});
test('manual-backup marks preserve distinct saved intent and confirmation with legacy records', () => {
  const decisions = {
    pending: {
      path: 'E:\\Work',
      status: 'manual-pending',
      notes: 'Copy this folder manually',
      futureMetadata: { owner: 'fixture' },
    },
    confirmed: {
      path: 'E:\\Originals',
      status: 'manual-confirmed',
      decidedAt: '2026-11-08T10:00:00Z',
      verification: { kind: 'manual-assertion', futureEvidence: 'preserved' },
    },
    legacy: { path: 'E:\\Archive', status: 'on-external' },
  };
  const parsed = parseDocument({ ...doc(), decisions });
  assert.deepEqual(parsed.decisions, decisions);
  assert.equal(parsed.decisions.pending?.status, 'manual-pending');
  assert.equal(parsed.decisions.confirmed?.status, 'manual-confirmed');
  assert.equal(parsed.decisions.legacy?.status, 'on-external');
});
test('Windows breadcrumbs stop at the drive root', () => {
  assert.equal(parentPath('E:\\'), null);
  assert.equal(parentPath('E:\\work'), 'E:\\');
  assert.equal(parentPath('E:\\work\\nested'), 'E:\\work');
});
test('invalid navigation cannot poison the native save queue or escape the fixture root', () => {
  assert.equal(normalizeReviewPath('E:/work/nested/', ['E:\\work']), 'E:\\work\\nested');
  assert.throws(() => normalizeReviewPath('E:\\work-neighbor', ['E:\\work']), /outside/);
  assert.throws(() => normalizeReviewPath('E:\\work\\..\\other', ['E:\\work']), /absolute/);
  assert.throws(() => normalizeReviewPath('relative\\folder', ['C:\\', 'E:\\']), /absolute/);
});
test('idle queue adopts only a newer external document and uses its revision for the next edit', async () => {
  const seen: number[] = [];
  let latest: Document = doc();
  const queue = new MutationQueue(latest, async revision => { seen.push(revision); return doc(revision + 1); }, value => { latest = value; });
  const external = { ...doc(5), decisions: { external: { path: 'E:\\work', status: 'last-moment', notes: 'Agent update' } } };
  assert.equal(queue.acceptExternal(external), true);
  assert.equal(queue.revision, 5);
  assert.equal(latest.decisions.external?.notes, 'Agent update');
  assert.equal(queue.acceptExternal(doc(4)), false);
  assert.equal(queue.acceptExternal(doc(5)), false);
  await queue.request({ type: 'view', patch: { search: 'work' } });
  assert.deepEqual(seen, [5]);
  assert.equal(queue.revision, 6);
});
test('pending and failed edits fence external hydration without dropping a retained draft', async () => {
  let release: (value: unknown) => void = () => undefined;
  const queue = new MutationQueue(doc(3), () => new Promise(resolve => { release = resolve; }), () => undefined);
  const saved = queue.request({ type: 'note', path: 'E:\\work', notes: 'Local draft' });
  assert.equal(queue.idle, false);
  assert.equal(queue.acceptExternal(doc(8)), false);
  assert.equal(queue.revision, 3);
  release(doc(2));
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.match(queue.state.error ?? '', /stale save response/i);
  assert.equal(queue.revision, 3);
  assert.equal(queue.acceptExternal(doc(8)), false);
  assert.deepEqual(queue.retainedOperations, [{ type: 'note', path: 'E:\\work', notes: 'Local draft' }]);
  // The promise intentionally remains unresolved until explicit retry acknowledges the edit.
  void saved;
});
test('reviewed conflict rebase preserves operation ids and uses the explicitly reviewed revision', async () => {
  const requests: Array<{ revision: number; id: string }> = [];
  let nativeRevision = 7;
  const queue = new MutationQueue(doc(3), async (revision, id) => {
    requests.push({ revision, id });
    if (revision !== nativeRevision) throw new Error('The app has newer saved state.');
    nativeRevision += 1;
    return doc(nativeRevision);
  }, () => undefined);
  const saved = queue.request({ type: 'note', path: 'E:\\work', notes: 'Reviewed pending edit' });
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.equal(queue.acceptExternal(doc(7)), false);
  assert.equal(queue.rebaseAfterReview(doc(2)), false);
  assert.equal(queue.rebaseAfterReview(doc(7)), true);
  assert.equal(queue.state.pending, 1);
  assert.equal(queue.state.error, 'The app has newer saved state.');
  queue.retry();
  await saved;
  await queue.flush();
  assert.deepEqual(requests.map(request => request.revision), [3, 7]);
  assert.equal(requests[0]?.id, requests[1]?.id);
  assert.equal(queue.revision, 8);
  assert.equal(queue.rebaseAfterReview(doc(9)), false);
});
test('reviewed retry accepts a previously applied operation only with its native idempotency receipt', async () => {
  let id = '';
  let acknowledge = false;
  const queue = new MutationQueue(doc(1), async (revision, requestId) => {
    id = requestId;
    if (!acknowledge) throw new Error('Acknowledgement lost after native save');
    return { ...doc(2), recentOperations: [requestId] };
  }, () => undefined);
  const saved = queue.request({ type: 'classify', path: 'E:\\work', status: 'last-moment' });
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.equal(queue.rebaseAfterReview({ ...doc(2), recentOperations: [id] }), true);
  acknowledge = true;
  queue.retry();
  await saved;
  await queue.flush();
  assert.equal(queue.revision, 2);
  assert.equal(queue.state.pending, 0);
});
test('public gateway parsing degrades safely and never retains credentials', () => {
  assert.equal(parseGateway(undefined).state, 'unavailable');
  const gateway = parseGateway({ state: 'running', endpoint: 'http://127.0.0.1:4321', instanceId: 'fixture', token: 'private-value' });
  assert.equal(gateway.state, 'running');
  assert.equal('token' in gateway, false);
});
test('folder previews replace only the right branch and retain independent positions', () => {
  const first = { path: 'E:\\one', page: 3, scrollTop: 120 };
  const second = { path: 'E:\\one\\two', page: 2, scrollTop: 80 };
  const third = { path: 'E:\\one\\two\\three', page: 0, scrollTop: 5 };
  assert.deepEqual(previewBranch([first, second, third], 1, 'E:\\one\\other'), [first, { path: 'E:\\one\\other', page: 0, scrollTop: 0 }]);
  assert.deepEqual(previewBranch([first, second, third], 1, second.path), [first, second, third]);
  assert.deepEqual(closePreviewBranch([first, second, third], 2), [first]);
  let columns = [first];
  for (let depth = 1; depth < 12; depth++) columns = previewBranch(columns, depth, `${columns[depth - 1]?.path}\\child`);
  assert.equal(columns.length, 12);
  assert.throws(() => previewBranch(columns, 13, 'E:\\bad'), /depth/);
});
test('preview positions survive saved-document parsing with old-document defaults', () => {
  assert.deepEqual(doc().view.previewColumns, []);
  assert.equal(doc().view.previewScrollLeft, 0);
  const saved = parseDocument({ ...doc(), view: { ...doc().view, previewColumns: [{ path: 'E:\\one', page: 4, scrollTop: 91, width: 370 }], previewScrollLeft: 224 } });
  assert.deepEqual(saved.view.previewColumns, [{ path: 'E:\\one', page: 4, scrollTop: 91, width: 370 }]);
  assert.equal(saved.view.previewScrollLeft, 224);
  assert.throws(() => parseDocument({ ...doc(), view: { ...doc().view, previewColumns: [{ path: 'E:\\one', page: -1, scrollTop: 0 }] } }), /preview/);
});
