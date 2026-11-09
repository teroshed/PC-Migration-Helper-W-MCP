#!/usr/bin/env node
import { createHash } from 'node:crypto';
import { lstat, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { assertNoLinkedAncestors, writeBundle } from './fixture.mjs';
import { freezeLiveMatrix, heldOutCorpus, holdoutPrecommit, verifyLiveMatrix } from './live-matrix.mjs';
import { createLiveJournal, inspectAttemptEvents, runLiveMatrix, validateLiveAuthorization } from './live-driver.mjs';

const INPUT_LIMIT = 64 * 1024 * 1024;
const HELP = 'Live question benchmark driver\n\n'
  + 'precommit --out <new absolute private directory>\n'
  + 'freeze --corpus <absolute corpus.private.json> --config <absolute config.json> --out <new absolute directory>\n'
  + 'run --corpus <absolute corpus.private.json> --matrix <absolute matrix.json> --authorization <absolute receipt.json> --adapter <absolute trusted .mjs> --out <new absolute directory>\n'
  + 'inspect --journal <absolute attempts.jsonl>\n\n'
  + 'The driver has no built-in provider or credential lookup. Only a hash-authorized injected adapter can dispatch.\n';

function options(args, required) {
  if (args.length !== required.length * 2) throw new Error('Supply every required option exactly once. Use help.');
  const result = new Map();
  for (let index = 0; index < args.length; index += 2) {
    const key = args[index];
    const value = args[index + 1];
    if (!required.includes(key) || result.has(key) || !path.isAbsolute(value)) throw new Error('Options require explicit absolute paths. Use help.');
    result.set(key, value);
  }
  return Object.fromEntries([...result].map(([key, value]) => [key.slice(2), value]));
}

async function readInput(filename) {
  await assertNoLinkedAncestors(filename);
  const metadata = await lstat(filename);
  if (!metadata.isFile() || metadata.size > INPUT_LIMIT) throw new Error('Input must be an ordinary file of at most 64 MiB.');
  return readFile(filename);
}

async function jsonInput(filename) {
  return JSON.parse((await readInput(filename)).toString('utf8'));
}

export async function main(argv) {
  const [command, ...args] = argv;
  if (!command || ['help', '--help'].includes(command)) { process.stdout.write(HELP); return; }
  if (command === 'precommit') {
    const { out } = options(args, ['--out']);
    const corpus = heldOutCorpus();
    await writeBundle(out, { 'corpus.private.json': corpus, 'precommit.json': holdoutPrecommit(corpus) });
    process.stdout.write('Created the private held-out corpus and route-pending precommit.\n');
    return;
  }
  if (command === 'freeze') {
    const { corpus: corpusPath, config: configPath, out } = options(args, ['--corpus', '--config', '--out']);
    const [corpus, config] = await Promise.all([jsonInput(corpusPath), jsonInput(configPath)]);
    await writeBundle(out, { 'matrix.json': freezeLiveMatrix(corpus, config) }, [corpusPath, configPath]);
    process.stdout.write('Froze the exact model, route and budget matrix.\n');
    return;
  }
  if (command === 'inspect') {
    const { journal } = options(args, ['--journal']);
    const text = (await readInput(journal)).toString('utf8');
    const truncatedTail = !text.endsWith('\n');
    const records = text.split('\n').slice(0, -1).map(line => JSON.parse(line));
    process.stdout.write(JSON.stringify({ ...inspectAttemptEvents(records), truncatedTail }, null, 2) + '\n');
    return;
  }
  if (command !== 'run') throw new Error('Unknown command. Use help.');
  const opts = options(args, ['--corpus', '--matrix', '--authorization', '--adapter', '--out']);
  if (path.extname(opts.adapter) !== '.mjs') throw new Error('The trusted adapter must be an explicit .mjs module.');
  const [corpus, frozen, authorization] = await Promise.all([opts.corpus, opts.matrix, opts.authorization].map(jsonInput));
  verifyLiveMatrix(frozen, corpus);
  validateLiveAuthorization(frozen, authorization);
  const adapterBytes = await readInput(opts.adapter);
  const adapterSha256 = createHash('sha256').update(adapterBytes).digest('hex');
  if (authorization.adapterSha256 !== adapterSha256) throw new Error('The trusted adapter hash is outside the authorization receipt.');
  await writeBundle(opts.out, { 'run-plan.json': { manifestHash: frozen.manifestHash, authorization, adapterSha256 } },
    [opts.corpus, opts.matrix, opts.authorization, opts.adapter]);
  const journal = await createLiveJournal(path.join(opts.out, 'attempts.jsonl'));
  try {
    // The caller must choose trusted, side-effect-free module initialization.
    const { createTransports } = await import('data:text/javascript;base64,' + adapterBytes.toString('base64'));
    if (typeof createTransports !== 'function') throw new Error('The adapter must export createTransports().');
    const transports = await createTransports();
    const result = await runLiveMatrix(frozen, corpus, { authorization, transports, journal });
    await writeFile(path.join(opts.out, 'result.json'), JSON.stringify(result, null, 2) + '\n', { flag: 'wx' });
    process.stdout.write('Recorded ' + result.rows.length + ' declared cells for evidence review.\n');
  } finally { await journal.close(); }
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  main(process.argv.slice(2)).catch(error => {
    process.stderr.write(error.message + '\n');
    process.exitCode = 1;
  });
}
