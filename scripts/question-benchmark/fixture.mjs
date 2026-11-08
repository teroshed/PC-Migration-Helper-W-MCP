import { createHash } from 'node:crypto';
import { lstat, mkdir, opendir, realpath, writeFile } from 'node:fs/promises';
import path from 'node:path';

export const FIXTURE_FORMAT = 'migration-question-fixture/v1';
export const ORACLE_FORMAT = 'migration-question-oracle/v1';
export const STATUSES = ['migrate-now', 'last-moment', 'redownload', 'erase'];
const CATEGORIES = ['ordinary', 'rebuild-candidate', 'protected-candidate'];
const GAP_REASONS = ['unreadable', 'link', 'depth-limit', 'unsupported'];
const FILE_TYPES = ['document', 'image', 'video', 'audio', 'source', 'archive', 'installer', 'other'];
const REBUILD_NAMES = new Set(['node_modules', '.venv', '.npm-cache', '.pnpm-store',
  '__pycache__', '.pytest_cache', '.ruff_cache', 'buildcache', 'rendercache']);
const PROTECTED_NAMES = new Set(['patches', 'credentials', 'secrets', 'saves', '.env']);

export function canonical(value) {
  if (value === undefined) return 'null';
  if (Array.isArray(value)) return `[${value.map(canonical).join(',')}]`;
  if (value !== null && typeof value === 'object') {
    return `{${Object.keys(value).filter(key => value[key] !== undefined).sort().map(key => `${JSON.stringify(key)}:${canonical(value[key])}`).join(',')}}`;
  }
  return JSON.stringify(value);
}

export function digest(value) {
  return createHash('sha256').update(canonical(value)).digest('hex');
}

function object(value, label) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw new Error(`Invalid ${label}.`);
  return value;
}

export function safeCount(value, label) {
  if (!Number.isSafeInteger(value) || value < 0) throw new Error(`Invalid ${label}.`);
  return value;
}

function normalizedRelative(value) {
  if (typeof value !== 'string' || !value || value.includes('\0')) throw new Error('Invalid relative entry path.');
  const text = value.replaceAll('\\', '/');
  if (path.posix.isAbsolute(text) || path.win32.isAbsolute(value) || text.includes(':')) {
    throw new Error('Entry paths must be relative to the selected scope.');
  }
  const parts = text.split('/');
  if (parts.some(part => !part || part === '.' || part === '..')) throw new Error('Invalid relative entry path.');
  return parts.join('/');
}

function parentOf(relative) {
  const separator = relative.lastIndexOf('/');
  return separator < 0 ? '' : relative.slice(0, separator);
}

function nameCategory(relative) {
  const basename = relative.slice(relative.lastIndexOf('/') + 1).toLowerCase();
  if (REBUILD_NAMES.has(basename)) return 'rebuild-candidate';
  if (PROTECTED_NAMES.has(basename)) return 'protected-candidate';
  return 'ordinary';
}

function fileType(relative) {
  const extension = path.posix.extname(relative).toLowerCase();
  if (['.pdf', '.doc', '.docx', '.odt', '.txt', '.xlsx', '.csv'].includes(extension)) return 'document';
  if (['.png', '.jpg', '.jpeg', '.webp', '.heic', '.raw'].includes(extension)) return 'image';
  if (['.mp4', '.mkv', '.mov', '.avi'].includes(extension)) return 'video';
  if (['.mp3', '.wav', '.flac', '.ogg'].includes(extension)) return 'audio';
  if (['.js', '.ts', '.tsx', '.py', '.rs', '.c', '.cpp', '.java', '.json'].includes(extension)) return 'source';
  if (['.zip', '.7z', '.tar', '.gz', '.rar'].includes(extension)) return 'archive';
  if (['.exe', '.msi', '.deb', '.rpm', '.apk'].includes(extension)) return 'installer';
  return 'other';
}

function bucketBytes(bytes, sizeMode) {
  if (sizeMode === 'exact' || bytes === 0) return bytes;
  // A lower power-of-two bucket preserves scale without publishing the exact size.
  return 2 ** Math.floor(Math.log2(bytes));
}

/** Only allowlisted metadata fields survive. Source names, notes and contents are discarded. */
export function sanitizeMetadata(input, { split = 'development', sizeMode = 'bucketed', maxEntries = 100_000 } = {}) {
  object(input, 'metadata');
  if (!['development', 'held-out'].includes(split)) throw new Error('Invalid fixture split.');
  if (!['exact', 'bucketed'].includes(sizeMode)) throw new Error('Invalid size mode.');
  if (input.rootCategory !== undefined && !CATEGORIES.includes(input.rootCategory)) throw new Error('Invalid root category.');
  safeCount(maxEntries, 'entry limit');
  if (maxEntries < 1 || !Array.isArray(input.entries) || input.entries.length > maxEntries) {
    throw new Error('Metadata is missing entries or exceeds the declared entry limit.');
  }
  const entries = new Map();
  const folded = new Set();
  const spelling = new Map();
  for (const raw of input.entries) {
    object(raw, 'metadata entry');
    const relative = normalizedRelative(raw.path);
    if (folded.has(relative.toLowerCase())) throw new Error('Duplicate or case-colliding metadata paths.');
    folded.add(relative.toLowerCase());
    for (let current = relative; current; current = parentOf(current)) {
      const previous = spelling.get(current.toLowerCase());
      if (previous !== undefined && previous !== current) throw new Error('Case-colliding metadata ancestors.');
      spelling.set(current.toLowerCase(), current);
    }
    if (!['folder', 'file', 'gap'].includes(raw.kind)) throw new Error('Unsupported metadata entry kind.');
    if (raw.category !== undefined && !CATEGORIES.includes(raw.category)) throw new Error('Invalid category.');
    if (raw.fileType !== undefined && !FILE_TYPES.includes(raw.fileType)) throw new Error('Invalid file type.');
    const bytes = raw.kind === 'file' ? safeCount(raw.logicalBytes, 'logical byte count') : 0;
    if (raw.kind === 'gap' && !GAP_REASONS.includes(raw.reason)) throw new Error('Invalid gap reason.');
    entries.set(relative, {
      relative, kind: raw.kind, category: raw.category ?? nameCategory(relative),
      logicalBytes: bucketBytes(bytes, sizeMode), reason: raw.kind === 'gap' ? raw.reason : undefined,
      fileType: raw.kind === 'file' ? raw.fileType ?? fileType(relative) : undefined,
    });
  }
  for (const relative of [...entries.keys()]) {
    let parent = parentOf(relative);
    while (parent) {
      if (!entries.has(parent)) entries.set(parent, { relative: parent, kind: 'folder', category: nameCategory(parent), logicalBytes: 0 });
      if (entries.get(parent).kind !== 'folder') throw new Error('A file or gap cannot contain entries.');
      parent = parentOf(parent);
    }
  }
  if (entries.size > maxEntries) throw new Error('Implied folders exceed the declared entry limit.');
  const sorted = [...entries.values()].sort((a, b) => a.relative.localeCompare(b.relative, 'en'));
  const identifiers = new Map([['', 'n000000']]);
  for (const [index, entry] of sorted.entries()) identifiers.set(entry.relative, `n${String(index + 1).padStart(6, '0')}`);
  const nodes = [{ id: 'n000000', parentId: null, kind: 'folder', label: 'scope', category: input.rootCategory ?? 'ordinary', logicalBytes: 0 }];
  for (const entry of sorted) {
    const id = identifiers.get(entry.relative);
    const node = {
      id, parentId: identifiers.get(parentOf(entry.relative)), kind: entry.kind,
      label: `${entry.kind}-${id.slice(1)}`, category: entry.category, logicalBytes: entry.logicalBytes,
    };
    if (entry.reason !== undefined) node.reason = entry.reason;
    if (entry.fileType !== undefined) node.fileType = entry.fileType;
    nodes.push(node);
  }
  const fixture = {
    format: FIXTURE_FORMAT, split, sizeMode, representation: 'one-node-per-observed-file',
    rootId: 'n000000', nodes, existingMarks: [],
  };
  validateFixture(fixture);
  return fixture;
}

export function validateFixture(value) {
  const fixture = object(value, 'fixture');
  if (fixture.format !== FIXTURE_FORMAT || !['development', 'held-out'].includes(fixture.split)
      || !['exact', 'bucketed'].includes(fixture.sizeMode) || !Array.isArray(fixture.nodes)
      || !Array.isArray(fixture.existingMarks) || fixture.representation !== 'one-node-per-observed-file') {
    throw new Error('Unsupported fixture.');
  }
  const nodes = new Map();
  let totalBytes = 0;
  const fields = new Set(['format', 'split', 'sizeMode', 'representation', 'rootId', 'nodes', 'existingMarks']);
  if (Object.keys(fixture).some(key => !fields.has(key))) throw new Error('Unexpected fixture field; sanitize the input first.');
  for (const node of fixture.nodes) {
    object(node, 'fixture node');
    if (!/^n[0-9]{6,}$/.test(node.id) || nodes.has(node.id) || !['file', 'folder', 'gap'].includes(node.kind)
        || !CATEGORIES.includes(node.category) || typeof node.label !== 'string'
        || !/^(scope|(folder|file|gap)-[0-9]{6,})$/.test(node.label)) throw new Error('Invalid sanitized fixture node.');
    safeCount(node.logicalBytes, 'fixture byte count');
    totalBytes = safeCount(totalBytes + node.logicalBytes, 'total fixture byte count');
    if (node.kind !== 'file' && node.logicalBytes !== 0) throw new Error('Only file nodes have byte counts.');
    if (node.kind === 'gap' && !GAP_REASONS.includes(node.reason)) throw new Error('Invalid fixture gap.');
    if (node.fileType !== undefined && (node.kind !== 'file' || !FILE_TYPES.includes(node.fileType))) throw new Error('Invalid fixture file type.');
    const allowed = new Set(['id', 'parentId', 'kind', 'label', 'category', 'logicalBytes', 'reason', 'fileType']);
    if (Object.keys(node).some(key => !allowed.has(key))) throw new Error('Unexpected fixture field; sanitize the input first.');
    nodes.set(node.id, node);
  }
  const root = nodes.get(fixture.rootId);
  if (!root || root.parentId !== null || root.kind !== 'folder') throw new Error('Missing fixture root.');
  const depths = new Map([[root.id, 0]]);
  for (const node of nodes.values()) {
    let current = node;
    const chain = [];
    const visiting = new Set();
    while (!depths.has(current.id)) {
      if (visiting.has(current.id)) throw new Error('Fixture contains a cycle.');
      visiting.add(current.id);
      chain.push(current);
      current = nodes.get(current.parentId);
      if (!current || current.kind !== 'folder') throw new Error('Fixture has an invalid parent.');
    }
    let depth = depths.get(current.id);
    for (const member of chain.reverse()) depths.set(member.id, ++depth);
  }
  const marked = new Set();
  for (const mark of fixture.existingMarks) {
    if (!nodes.has(mark.nodeId) || nodes.get(mark.nodeId).kind === 'gap' || marked.has(mark.nodeId)
        || !STATUSES.includes(mark.status) || Object.keys(mark).some(key => !['nodeId', 'status'].includes(key))) throw new Error('Invalid existing mark.');
    marked.add(mark.nodeId);
  }
  return fixture;
}

function comparable(value) {
  const absolute = path.resolve(value);
  return process.platform === 'win32' ? absolute.toLowerCase() : absolute;
}

function within(value, root) {
  const relative = path.relative(comparable(root), comparable(value));
  return !relative || (!path.isAbsolute(relative) && relative !== '..' && !relative.startsWith(`..${path.sep}`));
}

export async function assertNoLinkedAncestors(value) {
  const absolute = path.resolve(value);
  let cursor = path.parse(absolute).root;
  for (const part of absolute.slice(cursor.length).split(path.sep).filter(Boolean)) {
    cursor = path.join(cursor, part);
    let stat;
    try { stat = await lstat(cursor); } catch (error) {
      if (error.code === 'ENOENT') return;
      throw new Error('Cannot inspect a path ancestor.');
    }
    if (stat.isSymbolicLink()) throw new Error('Linked or junction path ancestors are not allowed.');
  }
}

export async function assertDisjoint(inputPath, outputDirectory) {
  if (!path.isAbsolute(inputPath) || !path.isAbsolute(outputDirectory)) throw new Error('Input and output paths must be absolute.');
  await assertNoLinkedAncestors(inputPath);
  await assertNoLinkedAncestors(outputDirectory);
  if (within(outputDirectory, inputPath) || within(inputPath, outputDirectory)) {
    throw new Error('Input and output must be disjoint; neither may contain the other.');
  }
}

/** Metadata-only opt-in capture. Links become gaps; directory identity changes abort publication. */
export async function captureMetadata(sourceDirectory, outputDirectory, {
  allowMetadataScan = false, maxEntries = 100_000, maxDepth = 64,
} = {}) {
  if (!allowMetadataScan) throw new Error('Directory capture requires explicit --allow-metadata-scan.');
  safeCount(maxEntries, 'entry limit');
  safeCount(maxDepth, 'depth limit');
  await assertDisjoint(sourceDirectory, outputDirectory);
  const source = path.resolve(sourceDirectory);
  const rootStat = await lstat(source);
  if (!rootStat.isDirectory() || rootStat.isSymbolicLink()) throw new Error('Select an ordinary directory.');
  const entries = [];
  const stack = [{ absolute: source, relative: '', depth: 0 }];
  const checkedDirectories = [];
  function add(entry) {
    if (entries.length >= maxEntries) throw new Error('Metadata capture reached its declared entry limit; no fixture was published.');
    entries.push(entry);
  }
  while (stack.length) {
    const current = stack.pop();
    const before = await lstat(current.absolute);
    if (before.isSymbolicLink() || !before.isDirectory()
        || comparable(await realpath(current.absolute)) !== comparable(current.absolute)) {
      throw new Error('A directory identity changed during capture; no fixture was published.');
    }
    checkedDirectories.push({ absolute: current.absolute, stat: before });
    let directory;
    try { directory = await opendir(current.absolute); } catch {
      if (!current.relative) throw new Error('The selected directory cannot be enumerated.');
      const previous = entries.find(entry => entry.path === current.relative);
      previous.kind = 'gap';
      previous.reason = 'unreadable';
      continue;
    }
    for await (const entry of directory) {
      const absolute = path.join(current.absolute, entry.name);
      const relative = current.relative ? `${current.relative}/${entry.name}` : entry.name;
      let stat;
      try { stat = await lstat(absolute); } catch {
        add({ path: relative, kind: 'gap', reason: 'unreadable' });
        continue;
      }
      if (stat.isSymbolicLink()) {
        add({ path: relative, kind: 'gap', reason: 'link' });
      } else if (stat.isDirectory()) {
        if (current.depth >= maxDepth) add({ path: relative, kind: 'gap', reason: 'depth-limit' });
        else {
          add({ path: relative, kind: 'folder' });
          stack.push({ absolute, relative, depth: current.depth + 1 });
        }
      } else if (stat.isFile()) add({ path: relative, kind: 'file', logicalBytes: stat.size });
      else add({ path: relative, kind: 'gap', reason: 'unsupported' });
    }
  }
  for (const directory of checkedDirectories) {
    const after = await lstat(directory.absolute);
    if (after.isSymbolicLink() || after.dev !== directory.stat.dev || after.ino !== directory.stat.ino
        || after.mtimeMs !== directory.stat.mtimeMs
        || comparable(await realpath(directory.absolute)) !== comparable(directory.absolute)) {
      throw new Error('Directory membership changed during capture; no fixture was published.');
    }
  }
  return { entries, rootCategory: nameCategory(source.replaceAll(path.sep, '/')) };
}

function makeRandom(seed) {
  if (!Number.isInteger(seed) || seed < 0 || seed > 0xffffffff) throw new Error('Seed must be an unsigned 32-bit integer.');
  let state = seed;
  return () => {
    state = (Math.imul(1664525, state) + 1013904223) >>> 0;
    return state / 0x100000000;
  };
}

/** Virtual choices are assigned independently of filenames and never rerolled at question time. */
export function generateOracle(fixture, {
  seed = 1, keepProbability = 0.7, unknownChoice = 'keep', assignments = [], knowledge = {}, faultInjection = false,
} = {}) {
  validateFixture(fixture);
  if (!Number.isFinite(keepProbability) || keepProbability < 0 || keepProbability > 1) throw new Error('Invalid keep probability.');
  const random = makeRandom(seed);
  const overrides = new Map();
  for (const item of assignments) {
    if (overrides.has(item.nodeId)) throw new Error('Duplicate intent assignment.');
    overrides.set(item.nodeId, item.status);
  }
  const files = fixture.nodes.filter(node => node.kind === 'file');
  if ([...overrides.keys()].some(id => !files.some(file => file.id === id))) throw new Error('Intent assignment references an unknown file.');
  const oracle = {
    format: ORACLE_FORMAT, fixtureHash: digest(fixture), seed,
    meaning: 'synthetic-owner-intent',
    assignments: files.map(file => ({ nodeId: file.id,
      status: overrides.get(file.id) ?? (random() < keepProbability ? 'last-moment' : 'redownload') })),
    knowledge: {
      recognizedFiles: knowledge.recognizedFiles ?? [],
      recognizedGroups: knowledge.recognizedGroups ?? [],
      mixedGroups: knowledge.mixedGroups ?? [],
      verifiedRebuildGroups: knowledge.verifiedRebuildGroups ?? [],
      deferredGroups: knowledge.deferredGroups ?? [],
      unknownChoice,
      acceptsNameHints: knowledge.acceptsNameHints ?? false,
    },
  };
  validateOracle(fixture, oracle, { faultInjection });
  return oracle;
}

export function validateOracle(fixture, value, { faultInjection = false } = {}) {
  validateFixture(fixture);
  const oracle = object(value, 'oracle');
  if (oracle.format !== ORACLE_FORMAT || oracle.fixtureHash !== digest(fixture)
      || oracle.meaning !== 'synthetic-owner-intent' || !Array.isArray(oracle.assignments)) {
    throw new Error('Oracle does not match this committed fixture.');
  }
  makeRandom(oracle.seed);
  if (Object.keys(oracle).some(key => !['format', 'fixtureHash', 'seed', 'meaning', 'assignments', 'knowledge'].includes(key))) throw new Error('Unexpected oracle field.');
  const nodes = new Map(fixture.nodes.map(node => [node.id, node]));
  const files = fixture.nodes.filter(node => node.kind === 'file');
  const assigned = new Map();
  for (const item of oracle.assignments) {
    if (nodes.get(item.nodeId)?.kind !== 'file' || assigned.has(item.nodeId) || !STATUSES.includes(item.status)
        || Object.keys(item).some(key => !['nodeId', 'status'].includes(key))) {
      throw new Error('Invalid or duplicate oracle intent.');
    }
    assigned.set(item.nodeId, item.status);
  }
  if (assigned.size !== files.length) throw new Error('Every observed file needs one precommitted intent.');
  const knowledge = object(oracle.knowledge, 'owner knowledge');
  if (Object.keys(knowledge).some(key => !['recognizedFiles', 'recognizedGroups', 'mixedGroups', 'verifiedRebuildGroups', 'deferredGroups', 'unknownChoice', 'acceptsNameHints'].includes(key))) throw new Error('Unexpected owner knowledge field.');
  for (const field of ['recognizedFiles', 'recognizedGroups', 'mixedGroups', 'verifiedRebuildGroups', 'deferredGroups']) {
    if (!Array.isArray(knowledge[field]) || new Set(knowledge[field]).size !== knowledge[field].length
        || knowledge[field].some(id => !nodes.has(id) || nodes.get(id).kind === 'gap'
          || field === 'recognizedFiles' && nodes.get(id).kind !== 'file')) throw new Error('Invalid owner knowledge.');
  }
  if (!['keep', 'defer'].includes(knowledge.unknownChoice) || typeof knowledge.acceptsNameHints !== 'boolean') throw new Error('Invalid unknown-answer policy.');
  for (const group of knowledge.verifiedRebuildGroups) {
    if (faultInjection) continue;
    for (const file of files) {
      let cursor = file;
      while (cursor && cursor.id !== group) cursor = nodes.get(cursor.parentId);
      if (cursor && ['migrate-now', 'last-moment'].includes(assigned.get(file.id))) {
        throw new Error('A truthful rebuild confirmation conflicts with keep intent; use a separate fault-injection run.');
      }
    }
  }
  return oracle;
}

export function commitment(fixture, oracle, options = {}) {
  validateOracle(fixture, oracle, options);
  return {
    format: 'migration-question-commitment/v1', fixtureHash: digest(fixture), oracleHash: digest(oracle),
    split: fixture.split, mode: options.faultInjection ? 'wrong-owner-answer-fault' : 'ordinary',
  };
}

export function verifyCommitment(fixture, oracle, record) {
  const expected = commitment(fixture, oracle, { faultInjection: record.mode === 'wrong-owner-answer-fault' });
  if (canonical(expected) !== canonical(record)) throw new Error('Fixture or oracle changed after commitment.');
}

/** A new directory is required. Files use exclusive creation and are never replaced. */
export async function writeBundle(outputDirectory, files, inputPaths = []) {
  if (!path.isAbsolute(outputDirectory)) throw new Error('Output directory must be absolute.');
  await assertNoLinkedAncestors(outputDirectory);
  for (const input of inputPaths) await assertDisjoint(input, outputDirectory);
  try { await lstat(outputDirectory); throw new Error('Output directory already exists. Choose a new directory.'); }
  catch (error) { if (error.code !== 'ENOENT') throw error; }
  for (const name of Object.keys(files)) {
    if (!/^[a-z0-9][a-z0-9.-]*\.json$/.test(name)) throw new Error('Invalid bundle filename.');
  }
  await mkdir(path.dirname(outputDirectory), { recursive: true });
  await assertNoLinkedAncestors(path.dirname(outputDirectory));
  await mkdir(outputDirectory);
  for (const [name, value] of Object.entries(files)) {
    await writeFile(path.join(outputDirectory, name), JSON.stringify(value, null, 2) + '\n', { flag: 'wx' });
  }
}

/** A physical shape fixture uses new empty placeholders. Logical sizes remain metadata. */
export async function materializeFixture(fixture, outputDirectory, {
  maxFiles = 10_000, placeholderBytes = 0, inputPaths = [],
} = {}) {
  validateFixture(fixture);
  safeCount(maxFiles, 'materialized file limit');
  safeCount(placeholderBytes, 'placeholder size');
  if (placeholderBytes > 4096) throw new Error('Placeholder size is limited to 4096 bytes per file.');
  const files = fixture.nodes.filter(node => node.kind === 'file');
  if (files.length > maxFiles || files.length * placeholderBytes > 64 * 1024 * 1024) {
    throw new Error('Physical dummy exceeds the declared file or 64 MiB placeholder limit.');
  }
  const receipt = {
    format: 'migration-question-physical-dummy/v1', fixtureHash: digest(fixture),
    files: files.length, unknownScopes: fixture.nodes.filter(node => node.kind === 'gap').length,
    placeholderBytesPerFile: placeholderBytes, actualFileBytes: files.length * placeholderBytes,
    declaredLogicalBytes: files.reduce((sum, file) => sum + file.logicalBytes, 0),
    evidence: 'Synthetic directory shape with fresh placeholders. Gaps remain in metadata; no source contents or permissions are reproduced.',
  };
  await writeBundle(outputDirectory, { 'fixture.json': fixture, 'dummy-plan.json': { ...receipt, state: 'planned' } }, inputPaths);
  const byId = new Map(fixture.nodes.map(node => [node.id, node]));
  function relative(node) {
    const labels = [];
    for (let current = node; current.parentId !== null; current = byId.get(current.parentId)) labels.unshift(current.label);
    return labels;
  }
  const folders = fixture.nodes.filter(node => node.kind === 'folder')
    .sort((a, b) => relative(a).length - relative(b).length);
  for (const folder of folders) await mkdir(path.join(outputDirectory, 'tree', ...relative(folder)), { recursive: true });
  const placeholder = Buffer.alloc(placeholderBytes);
  for (const file of files) await writeFile(path.join(outputDirectory, 'tree', ...relative(file)), placeholder, { flag: 'wx' });
  const completed = { ...receipt, state: 'completed' };
  await writeFile(path.join(outputDirectory, 'dummy-receipt.json'), JSON.stringify(completed, null, 2) + '\n', { flag: 'wx' });
  return completed;
}
