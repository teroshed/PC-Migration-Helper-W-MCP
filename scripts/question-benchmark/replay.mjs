import { performance } from 'node:perf_hooks';
import { commitment, digest, STATUSES, validateFixture, verifyCommitment } from './fixture.mjs';

export const POLICIES = ['alphabetical', 'file-count', 'exception-first', 'confirm-or-keep'];
const KEEP = new Set(['migrate-now', 'last-moment']);

function buildTree(fixture) {
  const nodes = new Map(fixture.nodes.map(node => [node.id, { ...node, children: [], parent: null }]));
  for (const node of nodes.values()) {
    if (node.parentId !== null) {
      node.parent = nodes.get(node.parentId);
      node.parent.children.push(node);
    }
  }
  return nodes;
}

function ancestors(node) {
  const result = [];
  for (let current = node; current; current = current.parent) result.push(current);
  return result;
}

function nearestMark(node, marks) {
  for (let current = node; current; current = current.parent) {
    if (marks.has(current.id)) return marks.get(current.id);
  }
  return null;
}

function isDeferred(node, deferred) {
  for (let current = node; current; current = current.parent) {
    if (deferred.has(current.id)) return true;
  }
  return false;
}

function visibleState(fixture, nodes, state) {
  const aggregates = new Map();
  const blocked = new Set();
  for (const id of [...state.split, ...state.deferred]) {
    for (const ancestor of ancestors(nodes.get(id))) blocked.add(ancestor.id);
  }
  const order = [...nodes.values()].sort((a, b) => ancestors(b).length - ancestors(a).length);
  for (const node of order) {
    const unresolved = node.kind === 'file' && nearestMark(node, state.marks) === null && !isDeferred(node, state.deferred);
    const aggregate = {
      unresolvedFiles: unresolved ? 1 : 0,
      unresolvedBytes: unresolved ? node.logicalBytes : 0,
      gaps: node.kind === 'gap' ? 1 : 0,
    };
    for (const child of node.children) {
      const nested = aggregates.get(child.id);
      aggregate.unresolvedFiles += nested.unresolvedFiles;
      aggregate.unresolvedBytes += nested.unresolvedBytes;
      aggregate.gaps += nested.gaps;
    }
    aggregates.set(node.id, aggregate);
  }
  const candidates = [];
  for (const node of nodes.values()) {
    const aggregate = aggregates.get(node.id);
    if (node.kind === 'gap' || aggregate.unresolvedFiles === 0 || aggregate.gaps > 0
        || blocked.has(node.id) || isDeferred(node, state.deferred)) continue;
    candidates.push({ nodeId: node.id, label: node.label, kind: node.kind, category: node.category,
      files: aggregate.unresolvedFiles, bytes: aggregate.unresolvedBytes });
  }
  return {
    fixtureHash: digest(fixture), split: fixture.split, rootId: fixture.rootId,
    nodes: structuredClone(fixture.nodes), candidates,
    marks: [...state.marks].map(([nodeId, status]) => ({ nodeId, status })),
    splitGroups: [...state.split], deferredGroups: [...state.deferred],
  };
}

/** This input contains public fixture metadata and previous answers, never oracle intent. */
export function selectCandidate(view, policy) {
  if (!POLICIES.includes(policy)) throw new Error('Unknown question policy.');
  const candidates = [...view.candidates];
  const eligibleIds = new Set(candidates.map(candidate => candidate.nodeId));
  const byId = new Map(view.nodes.map(node => [node.id, node]));
  const pathOf = id => {
    const labels = [];
    for (let node = byId.get(id); node; node = byId.get(node.parentId)) labels.unshift(node.label);
    return labels.join('/');
  };
  const directFrontier = candidates.filter(candidate => {
    const parent = byId.get(candidate.nodeId).parentId;
    for (let node = byId.get(parent); node; node = byId.get(node.parentId)) {
      if (eligibleIds.has(node.id)) return false;
    }
    return true;
  });
  const exceptionPolicy = policy === 'exception-first' || policy === 'confirm-or-keep';
  const pool = exceptionPolicy ? candidates : directFrontier;
  const tier = candidate => candidate.category === 'protected-candidate' ? 2
    : candidate.category === 'rebuild-candidate' ? 1 : 0;
  pool.sort((a, b) => {
    if (exceptionPolicy && tier(a) !== tier(b)) return tier(b) - tier(a);
    if (policy !== 'alphabetical' && a.files !== b.files) return b.files - a.files;
    if (policy !== 'alphabetical' && a.bytes !== b.bytes) return b.bytes - a.bytes;
    return pathOf(a.nodeId).localeCompare(pathOf(b.nodeId), 'en');
  });
  return pool[0]?.nodeId ?? null;
}

function questionFor(view, nodeId, policy) {
  const candidate = view.candidates.find(item => item.nodeId === nodeId);
  if (!candidate) throw new Error('Policy selected a path outside the eligible candidates.');
  const conservative = policy === 'confirm-or-keep';
  const asksRebuildability = conservative && candidate.category === 'rebuild-candidate';
  const proposedStatus = candidate.category === 'rebuild-candidate' && !conservative ? 'redownload' : 'last-moment';
  let prompt = `Keep ${candidate.label} with the final migration?`;
  if (asksRebuildability) prompt = `Can you recreate all of ${candidate.label}, including local edits, from retained inputs? Keep it if you are unsure.`;
  else if (proposedStatus === 'redownload') prompt = `Mark ${candidate.label} To Redownload because its name suggests generated data?`;
  else if (candidate.category === 'protected-candidate') prompt = `Keep ${candidate.label} as an explicit exception before deciding about its parent?`;
  const body = { ...candidate, prompt, proposedStatus, asksRebuildability,
    allowedActions: ['answer', ...(candidate.kind === 'folder' ? ['split'] : []), 'defer'] };
  return { ...body, questionId: digest({ fixtureHash: view.fixtureHash, body,
    marks: view.marks, splitGroups: view.splitGroups, deferredGroups: view.deferredGroups }) };
}

export function createInterview(fixture, { policy = 'confirm-or-keep', chooser } = {}) {
  validateFixture(fixture);
  if (!POLICIES.includes(policy)) throw new Error('Unknown question policy.');
  const nodes = buildTree(fixture);
  const state = { marks: new Map(fixture.existingMarks.map(mark => [mark.nodeId, mark.status])), split: new Set(), deferred: new Set() };
  const fixtureHash = digest(fixture);
  let prepared;
  const checkFixture = () => { if (digest(fixture) !== fixtureHash) throw new Error('Fixture changed during the interview.'); };
  function next() {
    checkFixture();
    if (prepared !== undefined) return structuredClone(prepared);
    const view = visibleState(fixture, nodes, state);
    // Clone across the policy boundary so a policy cannot mutate interview state.
    const selected = chooser ? chooser(structuredClone(view)) : selectCandidate(view, policy);
    prepared = selected === null ? null : questionFor(view, selected, policy);
    return structuredClone(prepared);
  }
  function apply(response) {
    const current = next();
    if (!current || response.questionId !== current.questionId || response.nodeId !== current.nodeId) {
      throw new Error('Response does not match the current question.');
    }
    if (!current.allowedActions.includes(response.action)) throw new Error('Response action is unavailable.');
    if (response.action === 'answer') {
      if (!STATUSES.includes(response.status)) throw new Error('Invalid answer status.');
      const confirmed = response.status === 'erase' ? response.explicitOmissionConfirmation : response.explicitRebuildConfirmation;
      if (policy === 'confirm-or-keep' && !KEEP.has(response.status) && current.kind === 'folder'
          && confirmed !== true) {
        throw new Error('A broad exclusion requires explicit owner confirmation.');
      }
      state.marks.set(current.nodeId, response.status);
    } else if (response.action === 'split') {
      state.split.add(current.nodeId);
    } else state.deferred.add(current.nodeId);
    prepared = undefined;
  }
  function status() {
    checkFixture();
    let resolved = 0;
    let deferred = 0;
    let pending = 0;
    let gaps = 0;
    const effective = [];
    for (const node of nodes.values()) {
      if (node.kind === 'gap') { gaps++; continue; }
      if (node.kind !== 'file') continue;
      const mark = nearestMark(node, state.marks);
      const held = isDeferred(node, state.deferred);
      if (mark !== null) resolved++;
      else if (held) deferred++;
      else pending++;
      effective.push({ nodeId: node.id, status: mark, deferred: mark === null && held });
    }
    return {
      state: pending || deferred || gaps ? 'partial' : 'complete',
      knownFiles: resolved + deferred + pending, resolvedFiles: resolved, deferredFiles: deferred, pendingFiles: pending,
      unknownScopes: gaps, effective,
    };
  }
  return { next, apply, status };
}

function privateOwnerResponder(fixture, oracle) {
  const nodes = buildTree(fixture);
  const intent = new Map(oracle.assignments.map(item => [item.nodeId, item.status]));
  const knowledge = oracle.knowledge;
  function answer(question) {
    const base = { questionId: question.questionId, nodeId: question.nodeId };
    if (knowledge.deferredGroups.includes(question.nodeId)) return { ...base, action: 'defer', reason: 'Precommitted owner deferral.' };
    if (knowledge.mixedGroups.includes(question.nodeId) && question.kind === 'folder') return { ...base, action: 'split', reason: 'The owner recognizes mixed intent here.' };
    if (knowledge.verifiedRebuildGroups.includes(question.nodeId)) return {
      ...base, action: 'answer', status: 'redownload', explicitRebuildConfirmation: true,
      reason: 'The owner confirms the complete reconstruction recipe, including local changes.',
    };
    if (question.kind === 'file' && knowledge.recognizedFiles.includes(question.nodeId)) return {
      ...base, action: 'answer', status: intent.get(question.nodeId), reason: 'The owner recognizes this exact file.',
    };
    if (knowledge.recognizedGroups.includes(question.nodeId)) {
      const statuses = new Set();
      for (const [id, status] of intent) if (ancestors(nodes.get(id)).some(node => node.id === question.nodeId)) statuses.add(status);
      if (statuses.size > 1) return { ...base, action: 'split', reason: 'The owner knows the group contains different decisions.' };
      const status = [...statuses][0];
      return { ...base, action: 'answer', status, explicitRebuildConfirmation: status === 'redownload',
        explicitOmissionConfirmation: status === 'erase', reason: 'The owner knows the whole group.' };
    }
    if (knowledge.acceptsNameHints && question.proposedStatus === 'redownload' && !question.asksRebuildability) return {
      ...base, action: 'answer', status: 'redownload', explicitRebuildConfirmation: false,
      reason: 'The scripted owner accepts the name-based recommendation without a rebuild check.',
    };
    return knowledge.unknownChoice === 'defer'
      ? { ...base, action: 'defer', reason: 'The owner is unsure and defers this scope.' }
      : { ...base, action: 'answer', status: 'last-moment', reason: 'The owner is unsure and prefers redundant retention.' };
  }
  return answer;
}

function score(fixture, oracle, state) {
  const truth = new Map(oracle.assignments.map(item => [item.nodeId, item.status]));
  const files = new Map(fixture.nodes.filter(node => node.kind === 'file').map(node => [node.id, node]));
  let falseExcludedFiles = 0;
  let falseExcludedBytes = 0;
  let redundantKeepBytes = 0;
  let exactFiles = 0;
  for (const observed of state.effective) {
    if (observed.status === null) continue;
    const expected = truth.get(observed.nodeId);
    if (expected === observed.status) exactFiles++;
    if (KEEP.has(expected) && !KEEP.has(observed.status)) {
      falseExcludedFiles++;
      falseExcludedBytes += files.get(observed.nodeId).logicalBytes;
    }
    if (!KEEP.has(expected) && KEEP.has(observed.status)) redundantKeepBytes += files.get(observed.nodeId).logicalBytes;
  }
  const coverageViolations = state.resolvedFiles + state.deferredFiles + state.pendingFiles !== state.knownFiles ? 1 : 0;
  return { ...state, effective: undefined, exactFiles, falseExcludedFiles, falseExcludedBytes, redundantKeepBytes, coverageViolations };
}

/** Replay is local. A supplied response list is evidence to score, never a provider call. */
export function replay(fixture, oracle, committed, {
  policy = 'confirm-or-keep', chooser, maxQuestions = fixture.nodes.length * 2 + 1,
  responses, evidenceKind = 'scripted-oracle', provenance,
} = {}) {
  verifyCommitment(fixture, oracle, committed);
  if (!Number.isSafeInteger(maxQuestions) || maxQuestions < 0) throw new Error('Invalid question limit.');
  if (!['scripted-oracle', 'recorded-ai-responses', 'recorded-human-responses'].includes(evidenceKind)) throw new Error('Invalid response evidence kind.');
  if (responses === undefined && evidenceKind !== 'scripted-oracle') throw new Error('Recorded evidence requires actual response records.');
  if (responses !== undefined && !Array.isArray(responses)) throw new Error('Invalid response records.');
  if (evidenceKind === 'recorded-ai-responses') {
    if (!provenance || ['model', 'route', 'trialId', 'authorizationRef'].some(key =>
      typeof provenance[key] !== 'string' || !provenance[key].trim())
      || ['promptLogHash', 'responseLogHash'].some(key => !/^[0-9a-f]{64}$/.test(provenance[key]))
      || typeof provenance.reviewed !== 'boolean') throw new Error('Recorded AI answers require exact model/route, authorization and log-hash provenance.');
    const allowed = new Set(['model', 'route', 'trialId', 'authorizationRef', 'promptLogHash', 'responseLogHash',
      'reviewed', 'providerLatencyMs', 'costUsd', 'inputTokens', 'outputTokens']);
    if (Object.keys(provenance).some(key => !allowed.has(key))) throw new Error('Unexpected provenance field; keep credentials and raw provider payloads outside the benchmark.');
    for (const key of ['providerLatencyMs', 'costUsd', 'inputTokens', 'outputTokens']) {
      if (provenance[key] !== undefined && (!Number.isFinite(provenance[key]) || provenance[key] < 0
        || key.endsWith('Tokens') && !Number.isSafeInteger(provenance[key]))) throw new Error('Invalid provider measurement.');
    }
  }
  const interview = createInterview(fixture, { policy, chooser });
  const owner = privateOwnerResponder(fixture, oracle);
  const started = performance.now();
  const trace = [];
  let stopReason = 'eligible-questions-exhausted';
  while (trace.length < maxQuestions) {
    const question = interview.next();
    if (!question) break;
    if (responses !== undefined && trace.length >= responses.length) { stopReason = 'recorded-responses-exhausted'; break; }
    const response = responses === undefined ? owner(question) : responses[trace.length];
    interview.apply(response);
    verifyCommitment(fixture, oracle, committed);
    trace.push({ questionNumber: trace.length + 1, question, response, score: score(fixture, oracle, interview.status()) });
  }
  if (trace.length === maxQuestions && interview.next()) stopReason = 'question-limit';
  if (responses && trace.length < responses.length) throw new Error('Response records remain after the interview ended.');
  const final = score(fixture, oracle, interview.status());
  const result = {
    format: 'migration-question-replay/v1', policy, fixtureHash: committed.fixtureHash, oracleHash: committed.oracleHash,
    split: fixture.split, mode: committed.mode, evidenceKind, stopReason,
    liveProviderExecutedByHarness: false, provenance, questions: trace.length, durationMs: performance.now() - started,
    score: final, trace,
  };
  // Keep elapsed time out of the logical replay hash so exact deterministic replays can be compared.
  result.logicHash = digest({ ...result, durationMs: 0 });
  return result;
}

export function rankResults(results) {
  if (!Array.isArray(results) || results.some(result => result.mode !== 'ordinary')) throw new Error('Fault injection must be reported separately from policy ranking.');
  if (new Set(results.map(result => `${result.fixtureHash}|${result.oracleHash}`)).size > 1) throw new Error('Rank only runs over the same committed fixture and oracle.');
  const loss = result => [result.score.falseExcludedFiles, result.score.coverageViolations,
    result.score.deferredFiles + result.score.pendingFiles, result.questions, result.score.redundantKeepBytes,
    result.provenance?.providerLatencyMs ?? result.durationMs, result.provenance?.costUsd ?? Number.POSITIVE_INFINITY];
  return [...results].sort((a, b) => {
    const left = loss(a);
    const right = loss(b);
    for (let index = 0; index < left.length; index++) if (left[index] !== right[index]) return left[index] - right[index];
    return a.policy.localeCompare(b.policy, 'en');
  });
}

export function heldOutGate(results, { requiredCells = [] } = {}) {
  if (!Array.isArray(requiredCells) || requiredCells.some(cell => !/^[0-9a-f]{64}$/.test(cell))
      || new Set(requiredCells).size !== requiredCells.length) throw new Error('Declare unique evaluation cell hashes.');
  const completedCells = new Set(results.filter(result => result.split === 'held-out'
    && result.mode === 'ordinary' && result.evidenceKind === 'recorded-ai-responses'
    && result.provenance?.reviewed === true
    && result.stopReason === 'eligible-questions-exhausted' && result.score.pendingFiles === 0)
    .map(evaluationCellId));
  const missingCells = requiredCells.filter(cell => !completedCells.has(cell));
  const ordinary = results.filter(result => result.mode === 'ordinary');
  return {
    declaredCells: requiredCells.length, missingCells,
    safetyPass: ordinary.length > 0 && ordinary.every(result => result.score.falseExcludedFiles === 0 && result.score.coverageViolations === 0),
    liveEvaluationPass: requiredCells.length > 0 && missingCells.length === 0,
    note: 'Recorded AI answers require independent provenance review. This harness never invokes a provider.',
  };
}

export function evaluationCellId(result) {
  return digest({ fixtureHash: result.fixtureHash, oracleHash: result.oracleHash, policy: result.policy,
    model: result.provenance?.model ?? null, route: result.provenance?.route ?? null,
    trialId: result.provenance?.trialId ?? null });
}

export { commitment };
