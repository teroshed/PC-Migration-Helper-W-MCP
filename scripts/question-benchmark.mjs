#!/usr/bin/env node
import { lstat, readFile } from 'node:fs/promises';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { assertDisjoint, assertNoLinkedAncestors, captureMetadata, commitment, digest,
  generateOracle, materializeFixture, sanitizeMetadata, writeBundle } from './question-benchmark/fixture.mjs';
import { replay } from './question-benchmark/replay.mjs';
import { runBuiltInSuite } from './question-benchmark/cases.mjs';

const HELP = `Local migration question benchmark

snapshot --metadata <absolute metadata.json> --out <new absolute directory> [--split development|held-out]
snapshot --source-dir <absolute directory> --allow-metadata-scan --out <new absolute directory>
commit --fixture <absolute fixture.json> --out <new absolute directory> [--intent-input <absolute intent.json>] [--seed 1]
replay --fixture <absolute fixture.json> --oracle <absolute oracle.json> --commitment <absolute commitment.json> --out <new absolute directory> [--policy confirm-or-keep]
materialize --fixture <absolute fixture.json> --out <new absolute directory> [--max-files 10000] [--placeholder-bytes 0]
suite --out <new absolute directory>

Snapshot options: --size-mode bucketed|exact --max-entries 100000 --max-depth 64
Replay options: --responses <absolute responses.json> --evidence-kind recorded-ai-responses|recorded-human-responses --provenance <absolute receipt.json>
This command has no provider connection. Live evaluation remains pending.
`;

function parseArguments(argv) {
  const [command, ...remaining] = argv;
  const flags = {};
  const booleans = new Set(['allow-metadata-scan', 'fault-injection']);
  for (let index = 0; index < remaining.length; index++) {
    const token = remaining[index];
    if (!token.startsWith('--') || flags[token.slice(2)] !== undefined) throw new Error('Invalid or repeated CLI option.');
    const key = token.slice(2);
    if (booleans.has(key)) flags[key] = true;
    else {
      const value = remaining[++index];
      if (!value || value.startsWith('--')) throw new Error('CLI option needs a value.');
      flags[key] = value;
    }
  }
  return { command, flags };
}

function required(flags, key) {
  const value = flags[key];
  if (typeof value !== 'string') throw new Error(`Missing --${key}.`);
  return value;
}

function number(flags, key, fallback) {
  if (flags[key] === undefined) return fallback;
  if (typeof flags[key] !== 'string' || !/^[0-9]+$/.test(flags[key])) throw new Error(`Invalid --${key}.`);
  const value = Number(flags[key]);
  if (!Number.isSafeInteger(value)) throw new Error(`Invalid --${key}.`);
  return value;
}

async function jsonFile(filename) {
  if (!path.isAbsolute(filename)) throw new Error('Input filenames must be absolute.');
  await assertNoLinkedAncestors(filename);
  const metadata = await lstat(filename);
  if (!metadata.isFile() || metadata.size > 64 * 1024 * 1024) throw new Error('JSON input must be an ordinary file of at most 64 MiB.');
  try { return JSON.parse(await readFile(filename, 'utf8')); }
  catch { throw new Error('Cannot read or parse the selected JSON input.'); }
}

export async function main(argv) {
  const { command, flags } = parseArguments(argv);
  if (!command || command === 'help' || command === '--help') { process.stdout.write(HELP); return; }
  const common = ['out'];
  const supported = {
    snapshot: ['metadata', 'source-dir', 'allow-metadata-scan', 'size-mode', 'split', 'max-entries', 'max-depth'],
    commit: ['fixture', 'intent-input', 'seed', 'fault-injection'],
    replay: ['fixture', 'oracle', 'commitment', 'policy', 'max-questions', 'responses', 'evidence-kind', 'provenance'],
    materialize: ['fixture', 'max-files', 'placeholder-bytes'],
    suite: [],
  };
  if (!supported[command] || Object.keys(flags).some(flag => ![...common, ...supported[command]].includes(flag))) {
    throw new Error('Unknown command or option. Use help.');
  }
  const output = required(flags, 'out');
  if (command === 'snapshot') {
    if (Boolean(flags.metadata) === Boolean(flags['source-dir'])) throw new Error('Choose exactly one metadata file or opt-in source directory.');
    const source = flags.metadata ?? flags['source-dir'];
    await assertDisjoint(source, output);
    const maxEntries = number(flags, 'max-entries', 100_000);
    const input = flags.metadata ? await jsonFile(source) : await captureMetadata(source, output, {
      allowMetadataScan: flags['allow-metadata-scan'] === true,
      maxEntries, maxDepth: number(flags, 'max-depth', 64),
    });
    const fixture = sanitizeMetadata(input, { split: flags.split ?? 'development', sizeMode: flags['size-mode'] ?? 'bucketed', maxEntries });
    await writeBundle(output, { 'fixture.json': fixture, 'capture-receipt.json': {
      fixtureHash: digest(fixture), sourceKind: flags.metadata ? 'provided-metadata' : 'opt-in-directory-metadata',
      names: 'surrogate labels', sizes: fixture.sizeMode, fileContentsRead: false,
      knownFiles: fixture.nodes.filter(node => node.kind === 'file').length,
      unknownScopes: fixture.nodes.filter(node => node.kind === 'gap').length,
    } }, [source]);
  } else if (command === 'commit') {
    const fixturePath = required(flags, 'fixture');
    const fixture = await jsonFile(fixturePath);
    const intent = flags['intent-input'] ? await jsonFile(flags['intent-input']) : {};
    const oracle = generateOracle(fixture, { ...intent, seed: number(flags, 'seed', intent.seed ?? 1),
      faultInjection: flags['fault-injection'] === true });
    const committed = commitment(fixture, oracle, { faultInjection: flags['fault-injection'] === true });
    await writeBundle(output, { 'oracle.json': oracle, 'commitment.json': committed },
      [fixturePath, ...(flags['intent-input'] ? [flags['intent-input']] : [])]);
  } else if (command === 'replay') {
    const fixturePath = required(flags, 'fixture');
    const oraclePath = required(flags, 'oracle');
    const committedPath = required(flags, 'commitment');
    const [fixture, oracle, committed] = await Promise.all([fixturePath, oraclePath, committedPath].map(jsonFile));
    const responses = flags.responses ? await jsonFile(flags.responses) : undefined;
    const provenance = flags.provenance ? await jsonFile(flags.provenance) : undefined;
    const result = replay(fixture, oracle, committed, { policy: flags.policy ?? 'confirm-or-keep',
      maxQuestions: number(flags, 'max-questions', fixture.nodes.length * 2 + 1), responses,
      evidenceKind: flags['evidence-kind'] ?? 'scripted-oracle', provenance });
    await writeBundle(output, { 'replay.json': result }, [fixturePath, oraclePath, committedPath,
      ...(flags.responses ? [flags.responses] : []), ...(flags.provenance ? [flags.provenance] : [])]);
  } else if (command === 'materialize') {
    const fixturePath = required(flags, 'fixture');
    await materializeFixture(await jsonFile(fixturePath), output, { maxFiles: number(flags, 'max-files', 10_000),
      placeholderBytes: number(flags, 'placeholder-bytes', 0), inputPaths: [fixturePath] });
  } else {
    await writeBundle(output, { 'suite.json': runBuiltInSuite() });
  }
  process.stdout.write(`Created ${command} evidence in a new output directory.\n`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  main(process.argv.slice(2)).catch(error => {
    process.stderr.write(`${error.message}\n`);
    process.exitCode = 1;
  });
}
