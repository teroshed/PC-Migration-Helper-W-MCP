import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
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
import { assessReviewedMatrix, createLiveJournal, inspectAttemptEvents, runLiveMatrix } from '../scripts/question-benchmark/live-driver.mjs';
import { freezeLiveMatrix, heldOutCorpus, holdoutPrecommit, verifyLiveMatrix } from '../scripts/question-benchmark/live-matrix.mjs';

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

function fakeRoute(role) {
  return { kind: 'injected-fake', model: 'fixture-model-' + role, route: 'fake:' + role,
    settings: { contextMode: 'stateless', temperature: 0, seed: 42 },
    limits: { inputTokens: 512, outputTokens: 256, costMicros: 100, timeoutMs: 1000,
      inputBytes: 32 * 1024, responseBytes: 8192 } };
}

function fakeMatrix(corpus, limits = {}) {
  return freezeLiveMatrix(corpus, { routes: { policy: fakeRoute('policy'), owner: fakeRoute('owner') },
    budget: { attempts: 224, inputTokens: 200_000, outputTokens: 100_000, costMicros: 100_000, ...limits },
    trialId: 'deterministic-fake-test' });
}

function fakeReceipt(request, body, extra = {}) {
  return { model: request.model, route: request.route, configHash: request.configHash, status: 'ok',
    body: JSON.stringify(body), usage: { inputTokens: 34, outputTokens: 12, costMicros: 4 }, ...extra };
}

function fakeAnswer(request) {
  const { question, facts } = JSON.parse(request.messages[1].content);
  const base = { questionId: question.questionId, nodeId: question.nodeId };
  if (facts.deferred || (!facts.recognizedIntent && !facts.groupIntents.length && facts.unknownChoice === 'defer')) {
    return { ...base, action: 'defer' };
  }
  if (facts.mixed || facts.groupIntents.length > 1) return { ...base, action: 'split' };
  if (facts.rebuildConfirmed) return { ...base, action: 'answer', status: 'redownload', explicitRebuildConfirmation: true };
  const status = facts.recognizedIntent ?? facts.groupIntents[0]
    ?? (facts.acceptsNameHints ? question.proposedStatus : 'last-moment');
  return { ...base, action: 'answer', status };
}

function fakeAdapters(events, overrides = {}) {
  return {
    policy: { kind: 'injected-fake', async invoke(request) {
      assert.equal(events.at(-1).event, 'attempt-reserved');
      assert.equal(events.at(-1).attemptId, request.attemptId);
      const context = JSON.parse(request.messages[1].content);
      assert.deepEqual(Object.keys(context).sort(), ['candidates', 'decisions', 'totalEligibleCandidates']);
      assert.ok(!JSON.stringify(request.messages).includes('recognizedIntent'));
      if (overrides.policy) return overrides.policy(request);
      return fakeReceipt(request, { nodeId: context.candidates[0].nodeId,
        prompt: 'Is this scope reconstructible, or should it be kept?', proposedStatus: 'last-moment' });
    } },
    owner: { kind: 'injected-fake', async invoke(request) {
      assert.equal(events.at(-1).event, 'attempt-reserved');
      assert.equal(events.at(-1).attemptId, request.attemptId);
      assert.deepEqual(Object.keys(JSON.parse(request.messages[1].content)).sort(), ['facts', 'question']);
      assert.ok(!JSON.stringify(request.messages).includes('oracleHash'));
      if (overrides.owner) return overrides.owner(request);
      return fakeReceipt(request, fakeAnswer(request));
    } },
  };
}

function fakeAuthorization(frozen) {
  return { manifestHash: frozen.manifestHash, kind: 'synthetic-test-only', reference: 'injected-fake-test' };
}

test('held-out precommit fixes case truth, policy order, exact route configuration and all cells', () => {
  const corpus = heldOutCorpus();
  const precommit = holdoutPrecommit(corpus);
  assert.equal(precommit.precommitHash, holdoutPrecommit(heldOutCorpus()).precommitHash);
  assert.equal(precommit.plannedCells, 21);
  assert.equal(precommit.plannedAttemptCeiling, 224);
  assert.equal(precommit.routeBinding, 'pending-explicit-authorization');
  const matrix = fakeMatrix(corpus);
  verifyLiveMatrix(matrix, corpus);
  for (const item of corpus) {
    const cells = matrix.manifest.cells.filter(cell => cell.caseId === item.caseId);
    assert.equal(cells.length, 3);
    assert.ok(cells.every(cell => cell.fixtureHash === item.committed.fixtureHash && cell.oracleHash === item.committed.oracleHash));
  }
  const modified = structuredClone(matrix);
  modified.manifest.routes.owner.settings.temperature = 1;
  assert.throws(() => verifyLiveMatrix(modified, corpus), /changed after commitment/);
});

test('injected fake exchanges exercise all cells with blind roles and separate wrong-owner faults', async () => {
  const corpus = heldOutCorpus();
  const frozen = fakeMatrix(corpus);
  const events = [];
  const result = await runLiveMatrix(frozen, corpus, {
    authorization: fakeAuthorization(frozen), transports: fakeAdapters(events),
    journal: { record: event => events.push(event) },
  });
  assert.equal(result.rows.length, 21);
  assert.equal(result.completeDeclaredMatrix, true);
  assert.equal(result.liveEvaluationPass, false);
  assert.equal(result.safetyPass, true);
  assert.ok(result.rows.every(row => row.evidenceKind === 'simulated-transport'));
  assert.ok(result.rows.filter(row => row.policy === 'adaptive').every(row =>
    row.trace.every(step => step.question.authoredBy === 'adaptive-model')));
  const hidden = result.rows.find(row => row.caseId === 'case-03' && row.policy === 'templates');
  assert.equal(hidden.score.falseExcludedFiles, 1);
  assert.equal(result.rows.filter(row => row.mode === 'wrong-owner-answer-fault').length, 3);
  assert.ok(result.rows.filter(row => row.mode === 'wrong-owner-answer-fault').every(row => row.score.falseExcludedFiles === 1));
  assert.equal(result.comparisons.length, 6);
  assert.equal(result.accounted.costMicros, result.attempts.length * 4);
  const reviewedFake = assessReviewedMatrix(frozen, result, {
    resultHash: digest(result), journalHash: digest(events), reviewerId: 'synthetic-review-test',
    selectedPolicy: 'conservative', cellIds: result.rows.map(row => row.cellId),
    heldOutIndependenceReviewed: true, ownerFidelityReviewed: true, routeReceiptsReviewed: true,
  });
  assert.equal(reviewedFake.liveEvaluationPass, false);
  assert.equal(reviewedFake.roleplayCoveragePass, false);
  assert.equal(inspectAttemptEvents(events).accounted.costMicros, result.accounted.costMicros);
  const reserved = events.findIndex(event => event.event === 'attempt-reserved');
  const interrupted = inspectAttemptEvents(events.slice(0, reserved + 1));
  assert.equal(interrupted.uncertainReservations, 1);
  assert.equal(interrupted.accounted.costMicros, 100);
});

test('the fake driver records frozen budget stops and omitted cells before dispatch', async () => {
  const corpus = heldOutCorpus().slice(0, 1);
  const frozen = fakeMatrix(corpus, { attempts: 1 });
  const events = [];
  const result = await runLiveMatrix(frozen, corpus, {
    authorization: fakeAuthorization(frozen), transports: fakeAdapters(events), journal: { record: event => events.push(event) },
  });
  assert.equal(result.accounted.attempts, 1);
  assert.equal(result.globalStop, 'frozen-budget-stopped');
  assert.deepEqual(result.rows.map(row => row.status), ['completed', 'failed', 'omitted']);
  assert.equal(result.rows[2].reason, 'frozen-budget-stopped');
  assert.equal(result.completeDeclaredMatrix, false);
});

test('an uncertain fake transport failure retains its full reservation and stops without retry', async () => {
  const corpus = heldOutCorpus().slice(0, 1);
  const frozen = fakeMatrix(corpus);
  const events = [];
  let calls = 0;
  const result = await runLiveMatrix(frozen, corpus, {
    authorization: fakeAuthorization(frozen),
    transports: fakeAdapters(events, { owner() { calls++; throw new Error('synthetic transport error'); } }),
    journal: { record: event => events.push(event) },
  });
  assert.equal(calls, 1);
  assert.equal(result.attempts[0].status, 'transport-uncertain');
  assert.equal(result.attempts[0].usage.costMicros, null);
  assert.equal(result.accounted.costMicros, 100);
  assert.equal(result.rows.filter(row => row.status === 'omitted').length, 2);
});

test('known fake provider failure and missing route remain separate matrix cells', async () => {
  const corpus = heldOutCorpus().slice(0, 1);
  const frozen = fakeMatrix(corpus);
  const events = [];
  let calls = 0;
  const transports = fakeAdapters(events, { owner(request) {
    calls++;
    return calls === 1 ? fakeReceipt(request, {}, { status: 'failed' }) : fakeReceipt(request, fakeAnswer(request));
  } });
  delete transports.policy;
  const result = await runLiveMatrix(frozen, corpus, {
    authorization: fakeAuthorization(frozen), transports, journal: { record: event => events.push(event) },
  });
  assert.equal(calls, 2);
  assert.deepEqual(result.rows.map(row => row.status), ['failed', 'omitted', 'completed']);
  assert.equal(result.rows[0].stopReason, 'provider-reported-failure');
  assert.equal(result.rows[1].reason, 'declared-route-unavailable');
  assert.equal(result.accounted.costMicros, 8);
});

test('a changed frozen fixture cannot apply a reply obtained from the fake owner', async () => {
  const corpus = heldOutCorpus().slice(0, 1);
  const frozen = fakeMatrix(corpus);
  const events = [];
  const result = await runLiveMatrix(frozen, corpus, {
    authorization: fakeAuthorization(frozen),
    transports: fakeAdapters(events, { owner(request) {
      corpus[0].fixture.nodes.find(node => node.kind === 'file').logicalBytes++;
      return fakeReceipt(request, fakeAnswer(request));
    } }),
    journal: { record: event => events.push(event) },
  });
  assert.equal(result.rows[0].stopReason, 'frozen-input-changed');
  assert.equal(result.rows[0].score.resolvedFiles, 0);
  assert.equal(result.accounted.attempts, 1);
  assert.equal(result.rows.filter(row => row.status === 'omitted').length, 2);
});

test('authorization and durable admission errors prevent even fake dispatch', async () => {
  const corpus = heldOutCorpus().slice(0, 1);
  const frozen = fakeMatrix(corpus);
  const events = [];
  let calls = 0;
  const transports = fakeAdapters(events, { owner() { calls++; } });
  await assert.rejects(runLiveMatrix(frozen, corpus, {
    authorization: { ...fakeAuthorization(frozen), manifestHash: 'wrong' }, transports,
    journal: { record: event => events.push(event) },
  }), /authorization receipt/);
  const result = await runLiveMatrix(frozen, corpus, {
    authorization: fakeAuthorization(frozen), transports, journal: { record(event) {
      if (event.event === 'attempt-reserved') throw new Error('synthetic journal failure');
      events.push(event);
    } },
  });
  assert.equal(calls, 0);
  assert.equal(result.attempts[0].status, 'admission-log-failed');
  assert.equal(result.attempts[0].dispatchStarted, false);
  assert.equal(result.accounted.costMicros, 0);
  assert.equal(result.globalStop, 'attempt-journal-write-failed');
});

test('a fake receipt over its declared limit stops further attempts and keeps the reported charge', async () => {
  const corpus = heldOutCorpus().slice(0, 1);
  const frozen = fakeMatrix(corpus);
  const events = [];
  const result = await runLiveMatrix(frozen, corpus, {
    authorization: fakeAuthorization(frozen), transports: fakeAdapters(events, { owner(request) {
      return fakeReceipt(request, fakeAnswer(request), { usage: { costMicros: 101 } });
    } }), journal: { record: event => events.push(event) },
  });
  assert.equal(result.globalStop, 'provider-exceeded-reservation');
  assert.equal(result.accounted.attempts, 1);
  assert.equal(result.accounted.costMicros, 101);
  assert.equal(result.rows.filter(row => row.status === 'omitted').length, 2);
});

test('a timed-out fake attempt remains reserved even when the adapter ignores cancellation', async () => {
  const corpus = heldOutCorpus().slice(0, 1);
  const owner = fakeRoute('owner');
  owner.limits.timeoutMs = 5;
  const frozen = freezeLiveMatrix(corpus, { routes: { policy: fakeRoute('policy'), owner },
    budget: { attempts: 10, inputTokens: 20_000, outputTokens: 20_000, costMicros: 10_000 }, trialId: 'fake-timeout' });
  const events = [];
  let signal;
  const result = await runLiveMatrix(frozen, corpus, {
    authorization: fakeAuthorization(frozen), transports: fakeAdapters(events, { owner(request) {
      signal = request.signal;
      return new Promise(() => {});
    } }), journal: { record: event => events.push(event) },
  });
  assert.equal(signal.aborted, true);
  assert.equal(result.globalStop, 'timed-out-uncertain');
  assert.equal(result.accounted.attempts, 1);
  assert.equal(result.accounted.costMicros, 100);
});

test('exclusive durable journal retains fake prompts and replies for independent evidence review', async () => {
  const base = await mkdtemp(path.join(TEMP_ROOT, 'fake-live-ledger-'));
  const filename = path.join(base, 'attempts.jsonl');
  const durable = await createLiveJournal(filename);
  const corpus = heldOutCorpus().slice(0, 1);
  const frozen = fakeMatrix(corpus);
  const events = [];
  // Use a wrapping sink in this fake test to assert admission ordering and capture the fsync path.
  const journal = { async record(event) { await durable.record(event); events.push(event); } };
  try {
    const result = await runLiveMatrix(frozen, corpus, {
      authorization: fakeAuthorization(frozen), transports: fakeAdapters(events), journal,
    });
    assert.equal(result.liveEvaluationPass, false);
  } finally { await durable.close(); }
  const disk = (await readFile(filename, 'utf8')).trim().split('\n').map(line => JSON.parse(line));
  assert.equal(disk.at(-1).event, 'matrix-finished');
  assert.ok(disk.some(event => event.event === 'attempt-reserved' && Array.isArray(event.messages)));
  assert.ok(disk.some(event => event.event === 'attempt-finished' && typeof event.body === 'string'));
  await assert.rejects(createLiveJournal(filename), /exist|EEXIST/i);
});

test('live CLI freezes and runs an explicitly hashed local fake adapter', async () => {
  const base = await mkdtemp(path.join(TEMP_ROOT, 'fake-live-cli-'));
  const liveCli = fileURLToPath(new URL('../scripts/question-benchmark/live-cli.mjs', import.meta.url));
  const invoke = args => spawnSync(process.execPath, [liveCli, ...args], { encoding: 'utf8' });
  const precommit = path.join(base, 'precommit');
  assert.equal(invoke(['precommit', '--out', precommit]).status, 0);
  const corpus = JSON.parse(await readFile(path.join(precommit, 'corpus.private.json'), 'utf8')).slice(0, 1);
  const corpusPath = path.join(base, 'single-case.json');
  const configPath = path.join(base, 'config.json');
  await writeFile(corpusPath, JSON.stringify(corpus));
  await writeFile(configPath, JSON.stringify({ routes: { policy: fakeRoute('policy'), owner: fakeRoute('owner') },
    budget: { attempts: 10, inputTokens: 20_000, outputTokens: 20_000, costMicros: 10_000 }, trialId: 'fake-cli' }));
  const freeze = path.join(base, 'frozen');
  assert.equal(invoke(['freeze', '--corpus', corpusPath, '--config', configPath, '--out', freeze]).status, 0);
  const matrixPath = path.join(freeze, 'matrix.json');
  const frozen = JSON.parse(await readFile(matrixPath, 'utf8'));
  const adapter = path.join(base, 'fake-adapter.mjs');
  const adapterSource = [
    'export function createTransports() {',
    '  const make = role => ({ kind: "injected-fake", async invoke(req) {',
    '    const payload = JSON.parse(req.messages[1].content);',
    '    const answer = role === "policy"',
    '      ? { nodeId: payload.candidates[0].nodeId, prompt: "Keep these originals?", proposedStatus: "migrate-now" }',
    '      : { questionId: payload.question.questionId, nodeId: payload.question.nodeId, action: "answer", status: "migrate-now" };',
    '    return { status: "ok", model: req.model, route: req.route, configHash: req.configHash,',
    '      body: JSON.stringify(answer), usage: { inputTokens: 30, outputTokens: 10, costMicros: 1 } };',
    '  } });',
    '  return { policy: make("policy"), owner: make("owner") };',
    '}',
  ].join('\n');
  await writeFile(adapter, adapterSource);
  const authorizationPath = path.join(base, 'authorization.json');
  await writeFile(authorizationPath, JSON.stringify({ ...fakeAuthorization(frozen), adapterSha256: '0'.repeat(64) }));
  const resultOut = path.join(base, 'run');
  const run = ['run', '--corpus', corpusPath, '--matrix', matrixPath, '--authorization', authorizationPath,
    '--adapter', adapter, '--out', resultOut];
  assert.match(invoke(run).stderr, /adapter hash/);
  await writeFile(authorizationPath, JSON.stringify({ ...fakeAuthorization(frozen),
    adapterSha256: createHash('sha256').update(adapterSource).digest('hex') }));
  const result = invoke(run);
  assert.equal(result.status, 0, result.stderr);
  const report = JSON.parse(await readFile(path.join(resultOut, 'result.json'), 'utf8'));
  assert.equal(report.completeDeclaredMatrix, true);
  assert.equal(report.liveEvaluationPass, false);
  const inspection = invoke(['inspect', '--journal', path.join(resultOut, 'attempts.jsonl')]);
  assert.equal(inspection.status, 0, inspection.stderr);
  assert.equal(JSON.parse(inspection.stdout).uncertainReservations, 0);
});
