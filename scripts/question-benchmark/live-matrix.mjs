import { commitment, digest, generateOracle, safeCount, sanitizeMetadata, verifyCommitment } from './fixture.mjs';

export const LIVE_POLICIES = ['templates', 'adaptive', 'conservative'];
export const ROUTE_KINDS = ['injected-fake', 'metered-provider', 'codex-roleplay'];
const MAX_ACCOUNTED = 1_000_000_000_000;

function opaqueLabel(value, label) {
  if (typeof value !== 'string' || !/^[A-Za-z0-9_.:/-]{1,160}$/.test(value)) throw new Error('Invalid ' + label + '.');
  return value;
}

function onlyFields(value, allowed, label) {
  if (!value || typeof value !== 'object' || Array.isArray(value)
      || Object.keys(value).some(key => !allowed.includes(key))) throw new Error('Invalid ' + label + ' fields.');
}

function positive(value, label, ceiling) {
  if (safeCount(value, label) === 0 || value > ceiling) throw new Error('Invalid ' + label + '.');
  return value;
}

function routeConfig(route) {
  onlyFields(route, ['kind', 'model', 'route', 'settings', 'limits'], 'route');
  if (!ROUTE_KINDS.includes(route.kind)) throw new Error('Unknown route adapter kind.');
  opaqueLabel(route.model, 'exact model ID');
  opaqueLabel(route.route, 'exact route label');
  onlyFields(route.settings, ['contextMode', 'temperature', 'topP', 'seed', 'reasoningEffort', 'revision'], 'route settings');
  if (route.settings.contextMode !== 'stateless') throw new Error('Each role requires an explicitly isolated request context.');
  for (const key of ['reasoningEffort', 'revision']) {
    if (route.settings[key] !== undefined) opaqueLabel(route.settings[key], key);
  }
  for (const key of ['temperature', 'topP']) {
    if (route.settings[key] !== undefined && (!Number.isFinite(route.settings[key])
        || route.settings[key] < 0 || route.settings[key] > (key === 'temperature' ? 2 : 1))) throw new Error('Invalid ' + key + '.');
  }
  if (route.settings.seed !== undefined) safeCount(route.settings.seed, 'provider seed');
  onlyFields(route.limits, ['inputTokens', 'outputTokens', 'costMicros', 'timeoutMs', 'inputBytes', 'responseBytes'], 'attempt limits');
  positive(route.limits.inputTokens, 'attempt input token limit', 1_000_000);
  positive(route.limits.outputTokens, 'attempt output token limit', 256_000);
  positive(route.limits.timeoutMs, 'attempt timeout', 900_000);
  positive(route.limits.inputBytes, 'attempt input byte limit', 128 * 1024);
  positive(route.limits.responseBytes, 'attempt response byte limit', 64 * 1024);
  safeCount(route.limits.costMicros, 'attempt cost ceiling');
  if (route.limits.costMicros > MAX_ACCOUNTED) throw new Error('Attempt cost ceiling is out of range.');
  if (route.kind === 'codex-roleplay' && route.limits.costMicros !== 0) {
    throw new Error('A Codex role-play route accounts for direct API spend only and requires a zero direct-charge ceiling.');
  }
  return { ...structuredClone(route), configHash: digest(route) };
}

function declaredCases(corpus) {
  if (!Array.isArray(corpus) || corpus.length === 0 || corpus.length > 64) throw new Error('Declare between 1 and 64 committed cases.');
  const seen = new Set();
  return corpus.map(item => {
    opaqueLabel(item.caseId, 'case ID');
    if (seen.has(item.caseId)) throw new Error('Duplicate case ID.');
    seen.add(item.caseId);
    verifyCommitment(item.fixture, item.oracle, item.committed);
    if (item.fixture.split !== 'held-out') throw new Error('Live matrix cases must be explicitly held out.');
    return { caseId: item.caseId, fixtureHash: item.committed.fixtureHash, oracleHash: item.committed.oracleHash,
      mode: item.committed.mode, questionLimit: positive(item.questionLimit, 'per-cell question cap', 1000) };
  });
}

/** Lock identities, case hashes, order, route settings and budget before dispatch. */
export function freezeLiveMatrix(corpus, { routes, budget, trialId }) {
  opaqueLabel(trialId, 'trial ID');
  onlyFields(routes, ['policy', 'owner'], 'role routes');
  const frozenRoutes = { policy: routeConfig(routes.policy), owner: routeConfig(routes.owner) };
  onlyFields(budget, ['attempts', 'inputTokens', 'outputTokens', 'costMicros'], 'matrix budget');
  for (const [key, value] of Object.entries(budget)) {
    if (safeCount(value, 'matrix ' + key + ' ceiling') > MAX_ACCOUNTED) throw new Error('Matrix budget ceiling is out of range.');
  }
  if (['attempts', 'inputTokens', 'outputTokens', 'costMicros'].some(key => budget[key] === undefined)) {
    throw new Error('Declare all matrix budget ceilings.');
  }
  const cases = declaredCases(corpus);
  const cells = cases.flatMap(item => LIVE_POLICIES.map(policy => {
    const roles = policy === 'adaptive' ? ['policy', 'owner'] : ['owner'];
    const spec = { ...item, policy, trialId, roles, routeHashes: roles.map(role => frozenRoutes[role].configHash) };
    return { ...spec, cellId: digest(spec) };
  }));
  const manifest = { format: 'migration-live-question-matrix/v1', trialId, routes: frozenRoutes,
    budget: structuredClone(budget), cells,
    plannedAttemptCeiling: cells.reduce((sum, cell) => sum + cell.questionLimit * cell.roles.length, 0) };
  return { manifest, manifestHash: digest(manifest) };
}

export function verifyLiveMatrix(frozen, corpus) {
  if (!frozen || digest(frozen.manifest) !== frozen.manifestHash) throw new Error('Live matrix changed after commitment.');
  const { routes, budget, trialId } = frozen.manifest;
  const { configHash: policyHash, ...policy } = routes.policy;
  const { configHash: ownerHash, ...owner } = routes.owner;
  const expected = freezeLiveMatrix(corpus, { routes: { policy, owner }, budget, trialId });
  if (policyHash !== expected.manifest.routes.policy.configHash || ownerHash !== expected.manifest.routes.owner.configHash
      || frozen.manifestHash !== expected.manifestHash) throw new Error('Live matrix does not match the frozen corpus or route settings.');
}

/** Invented shapes for a future held-out run. Policy fitting must use other cases. */
export function heldOutCorpus() {
  function make(caseId, specs, knowledgeFor, { questionLimit = 8, rootCategory = 'ordinary', fault = false } = {}) {
    const entries = specs.map(spec => ({ path: spec.path, kind: spec.kind ?? 'file', logicalBytes: spec.bytes ?? 1024,
      ...(spec.kind === 'gap' ? { reason: 'unreadable' } : {}) }));
    const fixture = sanitizeMetadata({ entries, rootCategory }, { split: 'held-out', sizeMode: 'exact' });
    const sorted = [...specs].sort((a, b) => a.path.localeCompare(b.path, 'en'));
    if (fixture.nodes.length !== sorted.length + 1) throw new Error('Held-out case must declare every folder explicitly.');
    const ids = new Map(sorted.map((spec, index) => [spec.path, fixture.nodes[index + 1].id]));
    const assignments = sorted.filter(spec => spec.intent).map(spec => ({ nodeId: ids.get(spec.path), status: spec.intent }));
    const owner = knowledgeFor(ids, fixture.rootId);
    const oracle = generateOracle(fixture, { seed: 0x672f94a1, assignments, knowledge: owner.knowledge,
      unknownChoice: owner.unknownChoice ?? 'keep', faultInjection: fault });
    return { caseId, questionLimit, fixture, oracle, committed: commitment(fixture, oracle, { faultInjection: fault }) };
  }
  const files = (directory, count, intent, bytes = 1024) => Array.from({ length: count }, (_, index) => ({
    path: (directory ? directory + '/' : '') + 'entry-' + index + '.bin', bytes, intent,
  }));
  const known = (field, root) => ({ knowledge: { [field]: [root] } });
  const hidden = [
    { path: 'objects', kind: 'folder' },
    ...files('objects', 11, 'redownload', 16 * 1024 * 1024),
    { path: 'opaque.bin', intent: 'migrate-now', bytes: 8192 },
  ];
  return [
    make('case-01', files('', 6, 'migrate-now'), (_, root) => known('recognizedGroups', root)),
    make('case-02', files('', 9, 'redownload'), (_, root) => known('verifiedRebuildGroups', root),
      { rootCategory: 'rebuild-candidate' }),
    make('case-03', hidden, () => ({ knowledge: { acceptsNameHints: true } }), { rootCategory: 'rebuild-candidate' }),
    make('case-04', [
      { path: 'node_modules', kind: 'folder' },
      { path: 'node_modules/patches', kind: 'folder' },
      ...files('node_modules/patches', 2, 'last-moment'),
      ...files('node_modules', 6, 'redownload', 8 * 1024 * 1024),
    ], ids => ({ knowledge: { recognizedGroups: [ids.get('node_modules/patches')], acceptsNameHints: true } })),
    make('case-05', Array.from({ length: 6 }, (_, index) => ({
      path: 'item-' + index + '.bin', intent: index % 2 ? 'redownload' : 'migrate-now',
    })), (ids, root) => ({ knowledge: { mixedGroups: [root], recognizedFiles: [...ids.values()] } })),
    make('case-06', [
      { path: 'deferred', kind: 'folder' },
      ...files('deferred', 2, 'last-moment'),
      { path: 'readable', kind: 'folder' },
      ...files('readable', 4, 'migrate-now'),
      { path: 'unavailable', kind: 'gap' },
    ], ids => ({ knowledge: { recognizedGroups: [ids.get('readable')], deferredGroups: [ids.get('deferred')] } })),
    make('fault-01', hidden, (_, root) => known('verifiedRebuildGroups', root),
      { rootCategory: 'rebuild-candidate', fault: true }),
  ];
}

/** The corpus and policy cells can be frozen while exact live routes await authorization. */
export function holdoutPrecommit(corpus) {
  const cases = declaredCases(corpus);
  const precommit = {
    format: 'migration-live-holdout-precommit/v1', cases, policies: LIVE_POLICIES,
    plannedCells: cases.length * LIVE_POLICIES.length,
    plannedAttemptCeiling: cases.reduce((sum, item) => sum + 4 * item.questionLimit, 0),
    routeBinding: 'pending-explicit-authorization',
    provenance: 'Invented corpus frozen before planned calls. Independent held-out authorship requires separate review.',
  };
  return { ...precommit, precommitHash: digest(precommit) };
}
