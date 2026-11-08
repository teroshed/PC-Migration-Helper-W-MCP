import assert from 'node:assert/strict';
import test from 'node:test';
import { mkdtemp, mkdir, readFile, readdir, stat, symlink, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { assertDisjoint, captureMetadata, commitment, digest, generateOracle, materializeFixture,
  sanitizeMetadata, validateFixture, verifyCommitment, writeBundle } from '../scripts/question-benchmark/fixture.mjs';
import { createInterview, evaluationCellId, heldOutGate, POLICIES, rankResults, replay } from '../scripts/question-benchmark/replay.mjs';
import { builtInCases, runBuiltInSuite } from '../scripts/question-benchmark/cases.mjs';

const TEMP_ROOT = process.env.MIGRATION_BENCHMARK_TEMP_ROOT ?? tmpdir();
const cli = fileURLToPath(new URL('../scripts/question-benchmark.mjs', import.meta.url));
const example = () => sanitizeMetadata({ entries: [
  { path: 'Project', kind: 'folder' },
  { path: 'Project/source.txt', kind: 'file', logicalBytes: 31 },
  { path: 'Project/node_modules', kind: 'folder' },
  { path: 'Project/node_modules/package.bin', kind: 'file', logicalBytes: 511 },
] });

test('metadata sanitization preserves shape and category clues while discarding names and notes', () => {
  const fixture = sanitizeMetadata({ notes: 'PRIVATE NOTE', root: 'E:/PrivateOwner', entries: [
    { path: 'UniqueOwner/name@example.com.txt', kind: 'file', logicalBytes: 63, notes: 'SECRET TEXT', checksum: 'secret' },
    { path: 'UniqueOwner/node_modules/install.bin', kind: 'file', logicalBytes: 1025 },
    { path: 'UniqueOwner/node_modules/patches/unique.bin', kind: 'file', logicalBytes: 7 },
    { path: 'UniqueOwner/Unavailable', kind: 'gap', reason: 'unreadable', error: 'E:/PrivateOwner' },
  ] });
  const json = JSON.stringify(fixture);
  for (const privateText of ['UniqueOwner', 'name@example.com', 'PRIVATE', 'SECRET', 'secret', 'E:/']) assert.equal(json.includes(privateText), false);
  assert.equal(fixture.nodes.filter(node => node.kind === 'file').length, 3);
  assert.equal(fixture.nodes.filter(node => node.kind === 'gap').length, 1);
  assert.equal(fixture.nodes.filter(node => node.category === 'rebuild-candidate').length, 1);
  assert.equal(fixture.nodes.filter(node => node.category === 'protected-candidate').length, 1);
  assert.equal(fixture.nodes.filter(node => node.fileType === 'document').length, 1);
  assert.equal(fixture.nodes.find(node => node.kind === 'file' && node.logicalBytes === 32)?.logicalBytes, 32);
  assert.equal(digest(sanitizeMetadata({ entries: [] })), digest(sanitizeMetadata({ entries: [] })));
});

test('invalid paths, collisions, unsupported metadata and dishonest counts are rejected', () => {
  for (const relative of ['../escape', '/absolute', 'C:\\private', 'folder/../escape', 'folder//child']) {
    assert.throws(() => sanitizeMetadata({ entries: [{ path: relative, kind: 'file', logicalBytes: 1 }] }));
  }
  assert.throws(() => sanitizeMetadata({ entries: [
    { path: 'a', kind: 'file', logicalBytes: 1 }, { path: 'A', kind: 'file', logicalBytes: 2 },
  ] }), /case-colliding/);
  assert.throws(() => sanitizeMetadata({ entries: [
    { path: 'A/one', kind: 'file', logicalBytes: 1 }, { path: 'a/two', kind: 'file', logicalBytes: 1 },
  ] }), /Case-colliding/);
  assert.throws(() => sanitizeMetadata({ entries: [
    { path: 'one', kind: 'file', logicalBytes: 2 ** 52 }, { path: 'two', kind: 'file', logicalBytes: 2 ** 52 },
  ] }, { sizeMode: 'exact' }), /total fixture/);
  assert.throws(() => sanitizeMetadata({ entries: [
    { path: 'a', kind: 'file', logicalBytes: 1 }, { path: 'a/child', kind: 'file', logicalBytes: 1 },
  ] }), /cannot contain/);
  assert.throws(() => sanitizeMetadata({ entries: [{ path: 'a', kind: 'file', logicalBytes: Number.MAX_SAFE_INTEGER + 1 }] }));
  assert.throws(() => sanitizeMetadata({ entries: [{ path: 'a/b', kind: 'file', logicalBytes: 1 }] }, { maxEntries: 1 }), /entry limit/);
  const fixture = example();
  fixture.nodes[1].privateNotes = 'must not survive';
  assert.throws(() => validateFixture(fixture), /Unexpected fixture field/);
});

test('opt-in metadata scan skips junctions and creates only sanitized invented evidence', async () => {
  const base = await mkdtemp(path.join(TEMP_ROOT, 'question-capture-'));
  const source = path.join(base, 'invented-source');
  const outside = path.join(base, 'invented-outside');
  const output = path.join(base, 'sanitized');
  await mkdir(path.join(source, 'node_modules'), { recursive: true });
  await mkdir(outside);
  await writeFile(path.join(source, 'owner-private.txt'), 'INVENTED FILE CONTENT MUST NEVER BE IN FIXTURE');
  await writeFile(path.join(source, 'node_modules', 'package.bin'), 'synthetic bytes');
  await writeFile(path.join(outside, 'outside-private.bin'), 'outside data');
  await symlink(outside, path.join(source, 'linked'), process.platform === 'win32' ? 'junction' : 'dir');
  await assert.rejects(captureMetadata(source, output), /explicit/);
  const captured = await captureMetadata(source, output, { allowMetadataScan: true });
  assert.equal(captured.entries.some(entry => entry.path.includes('outside-private')), false);
  assert.equal(captured.entries.find(entry => entry.path === 'linked').reason, 'link');
  const fixture = sanitizeMetadata(captured);
  const text = JSON.stringify(fixture);
  assert.equal(text.includes('INVENTED FILE CONTENT'), false);
  assert.equal(text.includes('owner-private'), false);
  await writeBundle(output, { 'fixture.json': fixture }, [source]);
  assert.equal((await readdir(output)).length, 1);
  await assert.rejects(captureMetadata(path.join(source, 'linked'), path.join(base, 'other'), { allowMetadataScan: true }), /Linked|junction/);
  await assert.rejects(assertDisjoint(source, path.join(source, 'nested-output')), /disjoint/);
  await assert.rejects(assertDisjoint(source, base), /disjoint/);
  await assert.rejects(writeBundle(output, { 'fixture.json': fixture }), /already exists/);
  await symlink(outside, path.join(base, 'output-link'), process.platform === 'win32' ? 'junction' : 'dir');
  await assert.rejects(writeBundle(path.join(base, 'output-link', 'new'), { 'fixture.json': fixture }), /Linked|junction/);
});

test('capture limits fail honestly and depth limits remain gaps', async () => {
  const base = await mkdtemp(path.join(TEMP_ROOT, 'question-limits-'));
  const source = path.join(base, 'source');
  await mkdir(path.join(source, 'deep'), { recursive: true });
  await writeFile(path.join(source, 'one'), 'x');
  await writeFile(path.join(source, 'deep', 'two'), 'y');
  await assert.rejects(captureMetadata(source, path.join(base, 'output'), { allowMetadataScan: true, maxEntries: 1 }), /entry limit/);
  const captured = await captureMetadata(source, path.join(base, 'output'), { allowMetadataScan: true, maxDepth: 0 });
  assert.deepEqual(captured.entries.find(entry => entry.path === 'deep'), { path: 'deep', kind: 'gap', reason: 'depth-limit' });
  assert.equal(captured.entries.filter(entry => entry.kind === 'file').length, 1);
});

test('physical dummies use sanitized shape and bounded fresh placeholders', async () => {
  const base = await mkdtemp(path.join(TEMP_ROOT, 'question-dummy-'));
  const fixture = example();
  const receipt = await materializeFixture(fixture, path.join(base, 'dummy'), { placeholderBytes: 3 });
  assert.equal(receipt.files, 2);
  assert.equal(receipt.actualFileBytes, 6);
  assert.equal(receipt.state, 'completed');
  const byId = new Map(fixture.nodes.map(node => [node.id, node]));
  for (const file of fixture.nodes.filter(node => node.kind === 'file')) {
    const labels = [];
    for (let current = file; current.parentId; current = byId.get(current.parentId)) labels.unshift(current.label);
    assert.equal((await stat(path.join(base, 'dummy', 'tree', ...labels))).size, 3);
  }
  await assert.rejects(materializeFixture(fixture, path.join(base, 'too-many'), { maxFiles: 1 }), /limit/);
  await assert.rejects(materializeFixture(fixture, path.join(base, 'too-large'), { placeholderBytes: 4097 }), /4096/);
});

test('oracle assignment is fixed before questions and ordinary confirmations cannot contradict keep intent', () => {
  const fixture = example();
  const first = generateOracle(fixture, { seed: 17 });
  assert.deepEqual(generateOracle(fixture, { seed: 17 }), first);
  const committed = commitment(fixture, first);
  assert.doesNotThrow(() => verifyCommitment(fixture, first, committed));
  const changed = structuredClone(first);
  changed.assignments[0].status = first.assignments[0].status === 'redownload' ? 'last-moment' : 'redownload';
  assert.throws(() => verifyCommitment(fixture, changed, committed), /changed after commitment/);
  assert.throws(() => generateOracle(fixture, { keepProbability: 1, knowledge: { verifiedRebuildGroups: [fixture.rootId] } }), /fault-injection/);
  const fault = generateOracle(fixture, { keepProbability: 1, knowledge: { verifiedRebuildGroups: [fixture.rootId] }, faultInjection: true });
  assert.equal(commitment(fixture, fault, { faultInjection: true }).mode, 'wrong-owner-answer-fault');
  assert.throws(() => commitment(fixture, { ...first, assignments: [] }), /Every observed file/);
  assert.throws(() => generateOracle(fixture, { keepProbability: NaN }), /probability/);
});

test('policy receives public metadata and previous answers without the oracle or knowledge', () => {
  const fixture = example();
  const oracle = generateOracle(fixture, { seed: 77, keepProbability: 1 });
  let policyCalls = 0;
  const interview = createInterview(fixture, { chooser: view => {
    policyCalls++;
    assert.equal('oracle' in view, false);
    assert.equal('assignments' in view, false);
    assert.equal('knowledge' in view, false);
    assert.equal('seed' in view, false);
    view.marks.push({ nodeId: 'fake', status: 'erase' });
    return view.candidates.find(item => item.nodeId === fixture.rootId).nodeId;
  } });
  const question = interview.next();
  assert.deepEqual(interview.next(), question);
  assert.equal(policyCalls, 1);
  assert.equal(interview.status().resolvedFiles, 0);
  assert.equal(digest(oracle), digest(generateOracle(fixture, { seed: 77, keepProbability: 1 })));
});

test('question fences reject stale answers and broad exclusions need explicit confirmation', () => {
  const fixture = example();
  const interview = createInterview(fixture);
  const question = interview.next();
  const response = { questionId: question.questionId, nodeId: question.nodeId, action: 'answer', status: 'redownload' };
  assert.throws(() => interview.apply(response), /explicit owner confirmation/);
  interview.apply({ ...response, explicitRebuildConfirmation: true });
  assert.throws(() => interview.apply(response), /current question/);
  fixture.nodes[1].logicalBytes = 1;
  assert.throws(() => interview.next(), /Fixture changed/);
  const omission = createInterview(example());
  const omitQuestion = omission.next();
  const omitResponse = { questionId: omitQuestion.questionId, nodeId: omitQuestion.nodeId, action: 'answer', status: 'erase' };
  assert.throws(() => omission.apply(omitResponse), /explicit owner confirmation/);
  omission.apply({ ...omitResponse, explicitOmissionConfirmation: true });
});

test('a deferred descendant blocks parent inheritance and unknown coverage never counts as complete', () => {
  const fixture = sanitizeMetadata({ entries: [
    { path: 'node_modules/private.bin', kind: 'file', category: 'protected-candidate', logicalBytes: 1 },
    { path: 'node_modules/rebuild.bin', kind: 'file', logicalBytes: 100 },
    { path: 'unavailable', kind: 'gap', reason: 'unreadable' },
  ] });
  const interview = createInterview(fixture);
  const protectedQuestion = interview.next();
  assert.equal(protectedQuestion.category, 'protected-candidate');
  interview.apply({ questionId: protectedQuestion.questionId, nodeId: protectedQuestion.nodeId, action: 'defer' });
  const next = interview.next();
  assert.equal(next.kind, 'file');
  interview.apply({ questionId: next.questionId, nodeId: next.nodeId, action: 'answer', status: 'last-moment' });
  assert.equal(interview.next(), null);
  const final = interview.status();
  assert.equal(final.state, 'partial');
  assert.equal(final.resolvedFiles, 1);
  assert.equal(final.deferredFiles, 1);
  assert.equal(final.unknownScopes, 1);
});

test('a broad keep retains the earlier exact child exception', () => {
  const fixture = sanitizeMetadata({ entries: [
    { path: 'a', kind: 'file', logicalBytes: 10 }, { path: 'b', kind: 'file', logicalBytes: 10 },
  ] });
  const ignored = fixture.nodes.find(node => node.kind === 'file');
  fixture.existingMarks = [{ nodeId: ignored.id, status: 'redownload' }];
  const interview = createInterview(fixture);
  const question = interview.next();
  assert.equal(question.nodeId, fixture.rootId);
  interview.apply({ questionId: question.questionId, nodeId: question.nodeId, action: 'answer', status: 'last-moment' });
  assert.equal(interview.status().effective.find(item => item.nodeId === ignored.id).status, 'redownload');
});

test('best, hidden-original, mixed and wrong-owner cases have meaningful safety scores', () => {
  const suite = runBuiltInSuite();
  assert.equal(suite.cases.length, 6);
  for (const item of suite.cases.filter(item => ['uniform-originals', 'confirmed-rebuild'].includes(item.name))) {
    for (const run of item.runs) assert.equal(run.questions, 1);
  }
  const hidden = suite.cases.find(item => item.name === 'hidden-original');
  const baseline = hidden.runs.find(run => run.policy === 'exception-first');
  const safe = hidden.runs.find(run => run.policy === 'confirm-or-keep');
  assert.equal(baseline.score.falseExcludedFiles, 1);
  assert.equal(safe.score.falseExcludedFiles, 0);
  assert.equal(safe.score.redundantKeepBytes, 1024 * 8 * 1024 * 1024);
  assert.equal(suite.faultInjection.run.score.falseExcludedFiles, 1);
  assert.throws(() => rankResults([suite.faultInjection.run]), /separately/);
  assert.equal(suite.gate.safetyPass, true);
  assert.equal(suite.gate.liveEvaluationPass, false);
  for (const item of suite.cases) {
    const selected = item.runs.find(run => run.policy === 'confirm-or-keep');
    assert.equal(selected.score.resolvedFiles + selected.score.deferredFiles + selected.score.pendingFiles, selected.score.knownFiles);
    assert.equal(selected.score.falseExcludedFiles, 0);
  }
});

test('recorded-answer exhaustion and absent live cells cannot pass a held-out evaluation', () => {
  const item = builtInCases()[0];
  const ordinary = replay(item.fixture, item.oracle, item.committed);
  const provenance = { model: 'synthetic-unit-test-model', route: 'unexecuted-test-route', trialId: 'test-only',
    authorizationRef: 'no-provider-call', promptLogHash: '1'.repeat(64), responseLogHash: '2'.repeat(64), reviewed: false };
  const recorded = replay(item.fixture, item.oracle, item.committed, { responses: [], evidenceKind: 'recorded-ai-responses', provenance });
  assert.equal(recorded.stopReason, 'recorded-responses-exhausted');
  assert.equal(recorded.score.resolvedFiles, 0);
  assert.ok(recorded.score.pendingFiles > 0);
  const gate = heldOutGate([recorded], { requiredCells: [evaluationCellId(recorded)] });
  assert.equal(gate.liveEvaluationPass, false);
  assert.equal(gate.missingCells.length, 1);
  assert.equal(ordinary.logicHash, replay(item.fixture, item.oracle, item.committed).logicHash);
  assert.throws(() => replay(item.fixture, item.oracle, item.committed, { evidenceKind: 'recorded-ai-responses' }), /requires actual|require actual/);
  assert.deepEqual(POLICIES.length, 4);
});

test('held-out gates require exact reviewed model, route and trial provenance for every declared cell', () => {
  const item = builtInCases()[0];
  item.fixture.split = 'held-out';
  item.oracle.fixtureHash = digest(item.fixture);
  const committed = commitment(item.fixture, item.oracle);
  const scripted = replay(item.fixture, item.oracle, committed);
  const responses = scripted.trace.map(entry => entry.response);
  const provenance = { model: 'unit-test-model-only', route: 'unit-test-route-only', trialId: 'unit-test-1',
    authorizationRef: 'no-spend-test', promptLogHash: '1'.repeat(64), responseLogHash: '2'.repeat(64), reviewed: false };
  const record = replay(item.fixture, item.oracle, committed, { responses, evidenceKind: 'recorded-ai-responses', provenance });
  const cell = evaluationCellId(record);
  assert.equal(heldOutGate([record], { requiredCells: [cell] }).liveEvaluationPass, false);
  const reviewedRecord = { ...record, provenance: { ...provenance, reviewed: true } };
  assert.equal(heldOutGate([reviewedRecord], { requiredCells: [cell] }).liveEvaluationPass, true);
  const wrongRoute = { ...reviewedRecord, provenance: { ...reviewedRecord.provenance, route: 'different-route' } };
  assert.equal(heldOutGate([wrongRoute], { requiredCells: [cell] }).liveEvaluationPass, false);
  assert.throws(() => heldOutGate([record], { requiredCells: [cell, cell] }), /unique/);
  assert.throws(() => replay(item.fixture, item.oracle, committed, { responses, evidenceKind: 'recorded-ai-responses' }), /provenance/);
  assert.throws(() => replay(item.fixture, item.oracle, committed, { responses, evidenceKind: 'recorded-ai-responses',
    provenance: { ...provenance, apiKey: 'must-never-be-exported' } }), /Unexpected provenance/);
  assert.throws(() => replay(item.fixture, item.oracle, committed, { responses, evidenceKind: 'recorded-ai-responses',
    provenance: { ...provenance, costUsd: -1 } }), /provider measurement/);
  assert.equal(heldOutGate([]).safetyPass, false);
});

test('CLI capture, commit and replay use explicit disjoint new outputs', async () => {
  const base = await mkdtemp(path.join(TEMP_ROOT, 'question-cli-'));
  const metadata = path.join(base, 'metadata.json');
  await writeFile(metadata, JSON.stringify({ entries: [{ path: 'synthetic-file', kind: 'file', logicalBytes: 123 }] }));
  const invoke = args => spawnSync(process.execPath, [cli, ...args], { encoding: 'utf8' });
  const fixtureOut = path.join(base, 'fixture');
  assert.equal(invoke(['snapshot', '--metadata', metadata, '--out', fixtureOut]).status, 0);
  const oracleOut = path.join(base, 'oracle');
  const fixturePath = path.join(fixtureOut, 'fixture.json');
  assert.equal(invoke(['commit', '--fixture', fixturePath, '--seed', '123', '--out', oracleOut]).status, 0);
  const resultOut = path.join(base, 'result');
  const result = invoke(['replay', '--fixture', fixturePath, '--oracle', path.join(oracleOut, 'oracle.json'),
    '--commitment', path.join(oracleOut, 'commitment.json'), '--out', resultOut]);
  assert.equal(result.status, 0, result.stderr);
  const report = JSON.parse(await readFile(path.join(resultOut, 'replay.json'), 'utf8'));
  assert.equal(report.evidenceKind, 'scripted-oracle');
  assert.equal(report.liveProviderExecutedByHarness, false);
  assert.equal(invoke(['snapshot', '--metadata', metadata, '--out', fixtureOut]).status, 1);
  assert.equal(invoke(['snapshot', '--metadata', metadata, '--out', path.join(base, 'invalid'), '--unknown', 'x']).status, 1);
});
