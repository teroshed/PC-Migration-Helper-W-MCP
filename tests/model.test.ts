import test from 'node:test';
import assert from 'node:assert/strict';
import { MutationQueue, parseDocument, parentPath, normalizeReviewPath, parseGateway, previewBranch, closePreviewBranch } from '../src/model.ts';
import type { Document, Operation } from '../src/model.ts';

function doc(revision = 1): Document {
  return parseDocument({ schemaVersion: 1, revision, history: [], decisions: {}, futureField: { preserved: true }, view: { path: 'E:\\', selectedPath: null, search: '', filter: 'all', page: 0, scrollTop: 0, recentPaths: [] } });
}
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
