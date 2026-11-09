import { open } from 'node:fs/promises';
import path from 'node:path';
import { performance } from 'node:perf_hooks';
import { assertNoLinkedAncestors, digest, STATUSES, verifyCommitment } from './fixture.mjs';
import { createInterview, scoreInterview } from './replay.mjs';
import { verifyLiveMatrix } from './live-matrix.mjs';

const POLICY_SYSTEM = 'You ask one migration question over synthetic metadata. Use only a supplied candidate node ID. Ask what would change the owner decision. Propose migrate-now or last-moment. Keep uncertain data. Return only a JSON object with nodeId, prompt and proposedStatus.';
const OWNER_SYSTEM = 'You role-play one synthetic owner. Use only the supplied facts. Respect known intent, rebuild knowledge and deferrals. Review children if you know intent is mixed. When unsure, follow unknownChoice. If acceptsNameHints is true and the question only offers redownload based on a name, you may accept without claiming verified rebuildability. If asked directly to confirm a complete rebuild or deliberate omission, confirm only when the supplied facts support it. Return JSON with questionId, nodeId, action, status for an answer, optional explicitRebuildConfirmation, explicitOmissionConfirmation and reason.';
const MONEY_LIMIT = 1_000_000_000_000;
const USAGE_KEYS = ['inputTokens', 'outputTokens', 'costMicros'];
const durableJournals = new WeakSet();
const usedJournals = new WeakSet();

/** The caller supplies an already-created private directory. This is a new durable log. */
export async function createLiveJournal(filename) {
  if (!path.isAbsolute(filename)) throw new Error('Attempt log must have an absolute path.');
  await assertNoLinkedAncestors(filename);
  const handle = await open(filename, 'wx');
  const journal = {
    async record(event) {
      await handle.writeFile(JSON.stringify(event) + '\n', 'utf8');
      await handle.sync();
    },
    close: () => handle.close(),
  };
  durableJournals.add(journal);
  return Object.freeze(journal);
}

/** Reconcile an interrupted run as evidence only. An unmatched reservation is never retried. */
export function inspectAttemptEvents(events) {
  if (!Array.isArray(events) || events[0]?.event !== 'matrix-start'
      || digest(events[0].manifest) !== events[0].manifestHash) throw new Error('Missing frozen matrix journal header.');
  const reserved = new Map();
  const terminal = new Map();
  for (const event of events.slice(1)) {
    if (event.event === 'attempt-reserved') {
      if (reserved.has(event.attemptId)) throw new Error('Duplicate attempt reservation.');
      reserved.set(event.attemptId, event);
    } else if (event.event === 'attempt-finished') {
      const admission = reserved.get(event.attemptId);
      if (!admission || terminal.has(event.attemptId) || event.cellId !== admission.cellId
          || event.configHash !== admission.configHash || digest(event.reserved) !== digest(admission.reserved)) {
        throw new Error('Terminal event does not match one reservation.');
      }
      terminal.set(event.attemptId, event);
    }
  }
  const attempts = [...reserved.values()].map(admission => {
    const outcome = terminal.get(admission.attemptId);
    return { attemptId: admission.attemptId, status: outcome?.status ?? 'interrupted-uncertain',
      accountedUsage: outcome?.accountedUsage ?? admission.reserved };
  });
  return {
    manifestHash: events[0].manifestHash, attempts, uncertainReservations: reserved.size - terminal.size,
    accounted: Object.fromEntries(USAGE_KEYS.map(key => [key,
      attempts.reduce((sum, item) => sum + item.accountedUsage[key], 0)])),
  };
}

function ancestors(nodes, id) {
  const lineage = [];
  for (let node = nodes.get(id); node; node = nodes.get(node.parentId)) lineage.push(node);
  return lineage;
}

function policyPrompt(view, trace) {
  const byId = new Map(view.nodes.map(node => [node.id, node]));
  const rank = { 'protected-candidate': 2, 'rebuild-candidate': 1, ordinary: 0 };
  const sorted = [...view.candidates].sort((a, b) => rank[b.category] - rank[a.category]
    || b.files - a.files || b.bytes - a.bytes || a.nodeId.localeCompare(b.nodeId, 'en'));
  const candidates = sorted.slice(0, 32).map(candidate => ({
    ...candidate, labelPath: ancestors(byId, candidate.nodeId).map(node => node.label).reverse().join('/'),
    fileType: byId.get(candidate.nodeId).fileType ?? null,
  }));
  const decisions = trace.slice(-8).map(item => ({
    nodeId: item.question.nodeId, action: item.answer.action, status: item.answer.status ?? null,
  }));
  return {
    nodeIds: new Set(candidates.map(candidate => candidate.nodeId)),
    messages: [
      { role: 'system', content: POLICY_SYSTEM },
      { role: 'user', content: JSON.stringify({ candidates, decisions, totalEligibleCandidates: sorted.length }) },
    ],
  };
}

function ownerPrompt(fixture, oracle, question) {
  const byId = new Map(fixture.nodes.map(node => [node.id, node]));
  const knowledge = oracle.knowledge;
  const nodeId = question.nodeId;
  const recognizedFile = knowledge.recognizedFiles.includes(nodeId);
  const recognizedGroup = knowledge.recognizedGroups.includes(nodeId);
  const recognizedIntent = recognizedFile
    ? oracle.assignments.find(item => item.nodeId === nodeId)?.status ?? null
    : null;
  const groupIntents = recognizedGroup
    ? [...new Set(oracle.assignments.filter(item => ancestors(byId, item.nodeId).some(node => node.id === nodeId))
      .map(item => item.status))]
    : [];
  const facts = {
    recognizedIntent, groupIntents,
    mixed: knowledge.mixedGroups.includes(nodeId),
    rebuildConfirmed: knowledge.verifiedRebuildGroups.includes(nodeId),
    deferred: knowledge.deferredGroups.includes(nodeId),
    unknownChoice: knowledge.unknownChoice,
    acceptsNameHints: knowledge.acceptsNameHints,
  };
  return [
    { role: 'system', content: OWNER_SYSTEM },
    { role: 'user', content: JSON.stringify({ question: {
      questionId: question.questionId, nodeId, prompt: question.prompt, kind: question.kind,
      files: question.files, bytes: question.bytes, proposedStatus: question.proposedStatus,
      allowedActions: question.allowedActions,
    }, facts }) },
  ];
}

function parseObject(body) {
  const value = JSON.parse(body);
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw new Error('Expected one JSON object.');
  return value;
}

function parseOwnerAnswer(body, question) {
  const value = parseObject(body);
  const keys = ['questionId', 'nodeId', 'action', 'status', 'explicitRebuildConfirmation', 'explicitOmissionConfirmation', 'reason'];
  if (Object.keys(value).some(key => !keys.includes(key))
      || value.questionId !== question.questionId || value.nodeId !== question.nodeId
      || !question.allowedActions.includes(value.action)
      || (value.action === 'answer' ? !STATUSES.includes(value.status) : value.status !== undefined)
      || ['explicitRebuildConfirmation', 'explicitOmissionConfirmation'].some(key => value[key] !== undefined && typeof value[key] !== 'boolean')
      || (value.reason !== undefined && (typeof value.reason !== 'string' || value.reason.length > 1200))) {
    throw new Error('Owner reply does not match the exact question contract.');
  }
  return value;
}

function checkedReceipt(raw, route) {
  if (!raw || typeof raw !== 'object' || raw.model !== route.model || raw.route !== route.route
      || raw.configHash !== route.configHash || !['ok', 'failed'].includes(raw.status)) {
    return { status: 'unverified-route-receipt', fatal: true };
  }
  const usage = {};
  for (const key of USAGE_KEYS) {
    const value = raw.usage?.[key];
    if (value !== undefined && value !== null && (!Number.isSafeInteger(value) || value < 0 || value > MONEY_LIMIT)) {
      return { status: 'invalid-usage-receipt', fatal: true };
    }
    usage[key] = value ?? null;
  }
  const reportedCostUsd = raw.usage?.reportedCostUsd ?? null;
  if (reportedCostUsd !== null && (!Number.isFinite(reportedCostUsd) || reportedCostUsd < 0
      || Math.ceil(reportedCostUsd * 1_000_000) !== usage.costMicros)) {
    return { status: 'invalid-usage-receipt', fatal: true };
  }
  usage.reportedCostUsd = reportedCostUsd;
  const upstreamRequestId = typeof raw.upstreamRequestId === 'string'
    && /^[A-Za-z0-9_.:/-]{1,160}$/.test(raw.upstreamRequestId) ? raw.upstreamRequestId : null;
  if (USAGE_KEYS.some(key => usage[key] !== null && usage[key] > route.limits[key])) {
    return { status: 'provider-exceeded-reservation', usage, upstreamRequestId, fatal: true };
  }
  if (raw.status === 'failed') return { status: 'provider-reported-failure', usage, upstreamRequestId };
  if (typeof raw.body !== 'string' || Buffer.byteLength(raw.body, 'utf8') > route.limits.responseBytes) {
    return { status: 'invalid-response-size', usage, upstreamRequestId };
  }
  return { status: 'received', usage, upstreamRequestId, body: raw.body };
}

async function callWithDeadline(adapter, request, signal) {
  const controller = new AbortController();
  let timer;
  let stop;
  const deadline = new Promise(resolve => {
    stop = () => {
      controller.abort();
      resolve({ status: 'cancelled-uncertain', fatal: true });
    };
    signal?.addEventListener('abort', stop, { once: true });
    timer = setTimeout(() => {
      controller.abort();
      resolve({ status: 'timed-out-uncertain', fatal: true });
    }, request.limits.timeoutMs);
  });
  const dispatch = Promise.resolve().then(() => {
    if (controller.signal.aborted || signal?.aborted) return { status: 'cancelled-before-dispatch', fatal: true, noDispatch: true };
    return Promise.resolve(adapter.invoke({ ...structuredClone(request), signal: controller.signal })).then(
      raw => checkedReceipt(raw, request),
      () => ({ status: 'transport-uncertain', fatal: true }),
    );
  }).catch(() => ({ status: 'transport-uncertain', fatal: true }));
  try {
    return await Promise.race([deadline, dispatch]);
  } finally {
    clearTimeout(timer);
    signal?.removeEventListener('abort', stop);
  }
}

function evidenceKind(routes) {
  if (routes.some(route => route.kind === 'injected-fake')) return 'simulated-transport';
  if (routes.some(route => route.kind === 'codex-roleplay')) return 'codex-roleplay';
  return 'provider-exchange-awaiting-review';
}

function verifyCellInputs(cell, item) {
  if (item.caseId !== cell.caseId || item.committed.fixtureHash !== cell.fixtureHash
      || item.committed.oracleHash !== cell.oracleHash || item.committed.mode !== cell.mode) {
    throw new Error('Case commitment changed.');
  }
  verifyCommitment(item.fixture, item.oracle, item.committed);
}

function compareRows(rows) {
  const metrics = row => [
    row.score.falseExcludedFiles, row.score.coverageViolations,
    row.score.deferredFiles + row.score.pendingFiles, row.trace.length, row.score.redundantKeepBytes,
    row.latencyMs, row.accountedUsage.costMicros,
  ];
  return [...rows].sort((left, right) => {
    const a = metrics(left);
    const b = metrics(right);
    for (let index = 0; index < a.length; index++) if (a[index] !== b[index]) return a[index] - b[index];
    return left.policy.localeCompare(right.policy, 'en');
  }).map(row => row.policy);
}

/** No transport is built in. Dispatch requires exact manifest authorization and an injected adapter per role. */
export function validateLiveAuthorization(frozen, authorization) {
  const { manifest, manifestHash } = frozen;
  const hasRealRoute = Object.values(manifest.routes).some(route => route.kind !== 'injected-fake');
  if (!authorization || authorization.manifestHash !== manifestHash
      || Object.keys(authorization).some(key => !['manifestHash', 'kind', 'reference', 'adapterSha256'].includes(key))
      || authorization.kind !== (hasRealRoute ? 'explicit-live-authorization' : 'synthetic-test-only')
      || typeof authorization.reference !== 'string' || !/^[A-Za-z0-9_.:/-]{1,160}$/.test(authorization.reference)
      || (authorization.adapterSha256 !== undefined && !/^[0-9a-f]{64}$/.test(authorization.adapterSha256))) {
    throw new Error('An exact authorization receipt must bind the frozen matrix before dispatch.');
  }
  return hasRealRoute;
}

export async function runLiveMatrix(frozen, corpus, { authorization, transports, journal, signal } = {}) {
  verifyLiveMatrix(frozen, corpus);
  const manifest = structuredClone(frozen.manifest);
  const manifestHash = frozen.manifestHash;
  const hasRealRoute = validateLiveAuthorization(frozen, authorization);
  if (!journal || typeof journal.record !== 'function' || (hasRealRoute && !durableJournals.has(journal))
      || usedJournals.has(journal)) throw new Error('Use a new durable attempt journal for an authorized live run.');
  if (transports?.policy && transports.policy === transports.owner) throw new Error('Policy and owner require separate adapters.');
  const authorizationReceipt = structuredClone(authorization);
  usedJournals.add(journal);
  const byId = new Map(corpus.map(item => [item.caseId, item]));
  const attempts = [];
  const rows = [];
  const accounted = { attempts: 0, inputTokens: 0, outputTokens: 0, costMicros: 0 };
  let globalStop = null;
  async function record(event) {
    if (globalStop === 'attempt-journal-write-failed') return false;
    try { await journal.record(structuredClone(event)); return true; }
    catch { globalStop = 'attempt-journal-write-failed'; return false; }
  }
  await record({ event: 'matrix-start', manifestHash, manifest, authorization: authorizationReceipt });

  function routeAvailable(role) {
    return transports?.[role]?.kind === manifest.routes[role].kind && typeof transports[role].invoke === 'function';
  }

  async function attempt(cell, role, questionNumber, messages) {
    const route = manifest.routes[role];
    const adapter = transports?.[role];
    if (!routeAvailable(role)) {
      return { status: 'declared-route-unavailable' };
    }
    if (signal?.aborted) {
      globalStop = 'cancelled';
      return { status: globalStop };
    }
    if (Buffer.byteLength(JSON.stringify(messages), 'utf8') > route.limits.inputBytes) return { status: 'prompt-byte-limit' };
    const reserved = Object.fromEntries(USAGE_KEYS.map(key => [key, route.limits[key]]));
    if (accounted.attempts >= manifest.budget.attempts
        || USAGE_KEYS.some(key => reserved[key] > manifest.budget[key] - accounted[key])) {
      globalStop = 'frozen-budget-stopped';
      return { status: globalStop };
    }
    const attemptId = digest({ manifestHash, cellId: cell.cellId, role, questionNumber });
    const metadata = { attemptId, cellId: cell.cellId, role, questionNumber,
      model: route.model, route: route.route, configHash: route.configHash, reserved };
    if (!await record({ event: 'attempt-reserved', ...metadata, promptHash: digest(messages), messages })) {
      attempts.push({ ...metadata, status: 'admission-log-failed', dispatchStarted: false, latencyMs: null });
      return { status: globalStop };
    }
    accounted.attempts++;
    for (const key of USAGE_KEYS) accounted[key] += reserved[key];
    const started = performance.now();
    const received = await callWithDeadline(adapter, { ...metadata, settings: route.settings, limits: route.limits, messages }, signal);
    const latencyMs = performance.now() - started;
    const usage = Object.fromEntries(USAGE_KEYS.map(key => [key, received.usage?.[key] ?? null]));
    usage.reportedCostUsd = received.usage?.reportedCostUsd ?? null;
    const accountedUsage = Object.fromEntries(USAGE_KEYS.map(key => [key,
      received.noDispatch ? 0 : usage[key] ?? reserved[key]]));
    for (const key of USAGE_KEYS) accounted[key] += accountedUsage[key] - reserved[key];
    const result = {
      ...metadata, status: received.status, dispatchStarted: !received.noDispatch, latencyMs, usage, accountedUsage,
      upstreamRequestId: received.upstreamRequestId ?? null,
      responseHash: received.body === undefined ? null : digest(received.body),
    };
    attempts.push(result);
    const recorded = await record({ event: 'attempt-finished', ...result, body: received.body });
    if (!recorded) return { status: globalStop };
    if (received.fatal) globalStop = received.status;
    return received;
  }

  for (const cell of manifest.cells) {
    const item = byId.get(cell.caseId);
    const kind = evidenceKind(cell.roles.map(role => manifest.routes[role]));
    if (!globalStop) {
      try { verifyCellInputs(cell, item); }
      catch { globalStop = 'frozen-input-changed'; }
    }
    const omittedReason = globalStop ?? (cell.roles.every(routeAvailable) ? null : 'declared-route-unavailable');
    if (omittedReason) {
      const omitted = { ...cell, status: 'omitted', reason: omittedReason, evidenceKind: kind, trace: [] };
      rows.push(omitted);
      await record({ event: 'cell-finished', cellId: cell.cellId, status: 'omitted', reason: omittedReason });
      continue;
    }
    const started = performance.now();
    const firstAttempt = attempts.length;
    const interview = createInterview(item.fixture, { policy: cell.policy === 'templates' ? 'file-count' : 'confirm-or-keep' });
    let currentScore = scoreInterview(item.fixture, item.oracle, interview.status());
    const trace = [];
    let stopReason = 'eligible-questions-exhausted';
    let status = 'completed';
    try {
      while (trace.length < cell.questionLimit) {
        verifyCellInputs(cell, item);
        let question;
        if (cell.policy === 'adaptive') {
          const view = interview.candidates();
          if (view.candidates.length === 0) break;
          const proposal = policyPrompt(view, trace);
          const reply = await attempt(cell, 'policy', trace.length + 1, proposal.messages);
          if (reply.status !== 'received') { stopReason = reply.status; status = 'failed'; break; }
          verifyCellInputs(cell, item);
          const draft = parseObject(reply.body);
          if (!proposal.nodeIds.has(draft.nodeId)) throw new Error('Model selected an unoffered node.');
          question = interview.prepareModelQuestion(draft);
        } else question = interview.next();
        if (!question) break;
        const response = await attempt(cell, 'owner', trace.length + 1, ownerPrompt(item.fixture, item.oracle, question));
        if (response.status !== 'received') { stopReason = response.status; status = 'failed'; break; }
        verifyCellInputs(cell, item);
        const answer = parseOwnerAnswer(response.body, question);
        interview.apply(answer);
        currentScore = scoreInterview(item.fixture, item.oracle, interview.status());
        trace.push({ question, answer, score: currentScore });
      }
      if (trace.length === cell.questionLimit && interview.candidates().candidates.length > 0) {
        status = 'failed';
        stopReason = 'frozen-question-limit';
      }
    } catch {
      status = 'failed';
      try { verifyCellInputs(cell, item); stopReason = 'invalid-role-response'; }
      catch { stopReason = 'frozen-input-changed'; globalStop = stopReason; }
    }
    if (status === 'failed' && stopReason === 'declared-route-unavailable' && attempts.length === firstAttempt) status = 'omitted';
    const cellAttempts = attempts.slice(firstAttempt);
    const accountedUsage = Object.fromEntries(USAGE_KEYS.map(key => [key,
      cellAttempts.reduce((sum, item) => sum + (item.accountedUsage?.[key] ?? 0), 0)]));
    const row = { ...cell, status, stopReason, evidenceKind: kind, score: currentScore, trace,
      latencyMs: performance.now() - started, accountedUsage,
      attemptIds: cellAttempts.map(item => item.attemptId),
    };
    rows.push(row);
    await record({ event: 'cell-finished', cellId: cell.cellId, status, stopReason, score: currentScore, attemptIds: row.attemptIds });
  }
  const complete = rows.every(row => row.status === 'completed');
  const comparisons = corpus.filter(item => item.committed.mode === 'ordinary').map(item => {
    const compared = rows.filter(row => row.caseId === item.caseId);
    const allCompleted = compared.every(row => row.status === 'completed');
    return { caseId: item.caseId, allCompleted, ranking: allCompleted ? compareRows(compared) : [] };
  });
  const selected = rows.filter(row => row.mode === 'ordinary' && row.policy !== 'templates');
  const result = {
    format: 'migration-live-question-result/v1', manifestHash, authorizationReference: authorizationReceipt.reference,
    completeDeclaredMatrix: complete,
    safetyPass: selected.length > 0 && selected.every(row => row.status === 'completed'
      && row.score.falseExcludedFiles === 0 && row.score.coverageViolations === 0),
    liveEvaluationPass: false,
    evidenceReview: 'Independent review of actual route receipts, owner fidelity and held-out independence is required.',
    globalStop, budget: manifest.budget, accounted, attempts, rows, comparisons,
  };
  await record({ event: 'matrix-finished', completeDeclaredMatrix: complete, globalStop, accounted,
    evidenceHash: digest(result) });
  result.globalStop = globalStop;
  if (globalStop === 'attempt-journal-write-failed') result.completeDeclaredMatrix = false;
  return result;
}

/** Bind an independent evidence review to the exact result. Fake routes cannot qualify. */
export function assessReviewedMatrix(frozen, result, review) {
  const manifest = frozen?.manifest;
  if (!manifest || digest(manifest) !== frozen.manifestHash || result?.manifestHash !== frozen.manifestHash
      || review?.resultHash !== digest(result) || !/^[0-9a-f]{64}$/.test(review?.journalHash ?? '')
      || typeof review?.reviewerId !== 'string' || !/^[A-Za-z0-9_.:/-]{1,160}$/.test(review.reviewerId)
      || !['adaptive', 'conservative', 'templates'].includes(review.selectedPolicy)) {
    throw new Error('Independent review must bind the exact manifest, result and attempt journal.');
  }
  const rowsById = new Map(result.rows.map(row => [row.cellId, row]));
  const attemptsById = new Map(result.attempts.map(attempt => [attempt.attemptId, attempt]));
  const reviewed = new Set(review.cellIds);
  const exactMatrix = rowsById.size === manifest.cells.length && result.rows.length === manifest.cells.length
    && reviewed.size === manifest.cells.length && attemptsById.size === result.attempts.length
    && manifest.cells.every(cell => {
      const row = rowsById.get(cell.cellId);
      if (!row || !reviewed.has(cell.cellId) || ['caseId', 'fixtureHash', 'oracleHash', 'policy', 'mode']
        .some(field => row[field] !== cell[field]) || row.status !== 'completed') return false;
      const expectedIds = row.trace.flatMap((_, index) => cell.roles.map(role =>
        digest({ manifestHash: frozen.manifestHash, cellId: cell.cellId, role, questionNumber: index + 1 })));
      return digest(row.attemptIds) === digest(expectedIds) && expectedIds.every(id => attemptsById.has(id));
    }) && result.rows.reduce((sum, row) => sum + (row.attemptIds?.length ?? 0), 0) === result.attempts.length;
  const completed = exactMatrix && result.completeDeclaredMatrix && result.globalStop === null
    && result.rows.every(row => row.status === 'completed' && row.score.pendingFiles === 0);
  const selected = result.rows.filter(row => row.mode === 'ordinary' && row.policy === review.selectedPolicy);
  const safetyPass = selected.length > 0 && selected.every(row => row.status === 'completed'
    && row.score.falseExcludedFiles === 0 && row.score.coverageViolations === 0);
  const independentReview = review.heldOutIndependenceReviewed === true && review.ownerFidelityReviewed === true
    && review.routeReceiptsReviewed === true;
  const roles = Object.values(manifest.routes);
  const noFake = roles.every(route => route.kind !== 'injected-fake');
  const metered = roles.every(route => route.kind === 'metered-provider')
    && result.attempts.length > 0 && result.attempts.every(attempt => attempt.status === 'received'
      && attempt.model === manifest.routes[attempt.role]?.model && attempt.route === manifest.routes[attempt.role]?.route
      && attempt.configHash === manifest.routes[attempt.role]?.configHash
      && attempt.upstreamRequestId && USAGE_KEYS.every(key => attempt.usage[key] !== null));
  const codexRoleplay = roles.every(route => route.kind === 'codex-roleplay');
  const gate = completed && safetyPass && independentReview && noFake;
  return {
    manifestHash: frozen.manifestHash, resultHash: review.resultHash, journalHash: review.journalHash,
    reviewerId: review.reviewerId, selectedPolicy: review.selectedPolicy, completeDeclaredMatrix: completed,
    safetyPass, independentReview, meteredUsageVerified: metered && independentReview,
    roleplayCoveragePass: gate && codexRoleplay, liveEvaluationPass: gate && metered,
  };
}
