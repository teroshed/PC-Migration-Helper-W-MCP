import { commitment, generateOracle, validateFixture } from './fixture.mjs';
import { heldOutGate, POLICIES, rankResults, replay } from './replay.mjs';

/** Entirely invented structures. Intents are constructed before any policy runs. */
function scenario(name, build, owner = {}) {
  const nodes = [];
  const intentions = [];
  const id = () => `n${String(nodes.length).padStart(6, '0')}`;
  function folder(parentId = null, category = 'ordinary') {
    const nodeId = id();
    nodes.push({ id: nodeId, parentId, kind: 'folder', category,
      label: parentId === null ? 'scope' : `folder-${nodeId.slice(1)}`, logicalBytes: 0 });
    return nodeId;
  }
  function file(parentId, status, logicalBytes = 1024, category = 'ordinary') {
    const nodeId = id();
    nodes.push({ id: nodeId, parentId, kind: 'file', category, label: `file-${nodeId.slice(1)}`, logicalBytes });
    intentions.push({ nodeId, status });
    return nodeId;
  }
  function gap(parentId) {
    const nodeId = id();
    nodes.push({ id: nodeId, parentId, kind: 'gap', category: 'ordinary', label: `gap-${nodeId.slice(1)}`, logicalBytes: 0, reason: 'unreadable' });
    return nodeId;
  }
  const root = folder(null, owner.rootCategory ?? 'ordinary');
  const knowledge = { recognizedFiles: [], recognizedGroups: [], mixedGroups: [],
    verifiedRebuildGroups: [], deferredGroups: [], acceptsNameHints: true };
  const existingMarks = [];
  build({ root, folder, file, gap, knowledge, existingMarks });
  const fixture = { format: 'migration-question-fixture/v1', split: 'development', sizeMode: 'exact',
    representation: 'one-node-per-observed-file', rootId: root, nodes, existingMarks };
  validateFixture(fixture);
  const oracle = generateOracle(fixture, { seed: 0x5a17c3d1, assignments: intentions,
    knowledge, unknownChoice: owner.unknownChoice ?? 'keep' });
  return { name, fixture, oracle, committed: commitment(fixture, oracle) };
}

export function builtInCases() {
  return [
    scenario('uniform-originals', ({ root, file, knowledge }) => {
      for (let index = 0; index < 1000; index++) file(root, 'migrate-now', 4096);
      knowledge.recognizedGroups.push(root);
    }),
    scenario('confirmed-rebuild', ({ root, file, knowledge }) => {
      for (let index = 0; index < 1024; index++) file(root, 'redownload', 8192);
      knowledge.verifiedRebuildGroups.push(root);
    }, { rootCategory: 'rebuild-candidate' }),
    scenario('hidden-original', ({ root, folder, file }) => {
      const installed = folder(root);
      for (let index = 0; index < 1024; index++) file(installed, 'redownload', 8 * 1024 * 1024);
      file(folder(root), 'migrate-now', 8192);
    }, { rootCategory: 'rebuild-candidate' }),
    scenario('protected-local-patch', ({ root, folder, file, knowledge }) => {
      const cache = folder(root, 'rebuild-candidate');
      const patches = folder(cache, 'protected-candidate');
      file(patches, 'last-moment', 8192);
      knowledge.recognizedGroups.push(patches);
      for (let index = 0; index < 100; index++) file(cache, 'redownload', 4096);
    }),
    scenario('mixed-gaps-and-defer', ({ root, folder, file, gap, knowledge, existingMarks }) => {
      const readable = folder(root);
      knowledge.mixedGroups.push(readable);
      for (let index = 0; index < 32; index++) {
        knowledge.recognizedFiles.push(file(readable, index % 2 ? 'redownload' : 'migrate-now'));
      }
      const held = folder(root);
      for (let index = 0; index < 4; index++) file(held, 'migrate-now');
      knowledge.deferredGroups.push(held);
      const ignored = folder(root);
      file(ignored, 'redownload');
      existingMarks.push({ nodeId: ignored, status: 'redownload' });
      for (let index = 0; index < 3; index++) gap(root);
    }),
    scenario('unknown-owner', ({ root, folder, file }) => {
      const child = folder(root);
      file(child, 'migrate-now');
      file(child, 'redownload');
    }, { unknownChoice: 'defer' }),
  ];
}

export function runBuiltInSuite() {
  const cases = builtInCases().map(item => {
    const runs = POLICIES.map(policy => replay(item.fixture, item.oracle, item.committed, { policy }));
    return { ...item, runs, rankedPolicies: rankResults(runs).map(run => run.policy) };
  });
  const hidden = cases.find(item => item.name === 'hidden-original');
  const mistakenOracle = structuredClone(hidden.oracle);
  mistakenOracle.knowledge.verifiedRebuildGroups = [hidden.fixture.rootId];
  const faultCommitment = commitment(hidden.fixture, mistakenOracle, { faultInjection: true });
  const fault = replay(hidden.fixture, mistakenOracle, faultCommitment);
  const selected = cases.map(item => item.runs.find(run => run.policy === 'confirm-or-keep'));
  return {
    format: 'migration-question-suite/v1', evaluation: 'local deterministic development harness',
    liveEvaluationExecuted: false, cases,
    faultInjection: { fixture: hidden.fixture, oracle: mistakenOracle, committed: faultCommitment, run: fault },
    gate: heldOutGate(selected),
  };
}
