import { record } from './model.ts';

function text(value: unknown): string {
  if (typeof value !== 'string') throw new Error('Mirror evidence contains an invalid text field.');
  return value;
}
function digest(value: unknown): string {
  if (typeof value !== 'string' || !/^[a-f0-9]{64}$/.test(value)) throw new Error('Mirror evidence identity is invalid.');
  return value;
}
function count(value: unknown): number {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0) throw new Error('Mirror evidence contains an invalid count.');
  return value;
}
function nullableText(value: unknown): string | null {
  return typeof value === 'string' ? value : null;
}
function observedCount(value: unknown): number | null {
  return value === null ? null : count(value);
}
function array(value: unknown): unknown[] {
  if (!Array.isArray(value)) throw new Error('Mirror evidence contains an invalid list.');
  return value;
}

function endpoint(value: unknown) {
  const data = record(value);
  return { root: text(data.root), volumeRoot: text(data.volumeRoot), volumeId: text(data.volumeId), rootIdentity: text(data.rootIdentity) };
}
export function parseMirrorBinding(value: unknown) {
  const data = record(value);
  return { bindingId: digest(data.bindingId), source: endpoint(data.source), mirror: endpoint(data.mirror) };
}
export type MirrorBinding = ReturnType<typeof parseMirrorBinding>;

function counts(value: unknown) {
  const data = record(value);
  return {
    equal: count(data.equal), directories: count(data.directories), missing: count(data.missing),
    different: count(data.different), extra: count(data.extra), changedDuringRead: count(data.changedDuringRead),
    unreadable: count(data.unreadable), unsupported: count(data.unsupported), coverageGaps: count(data.coverageGaps),
  };
}
export type MirrorCounts = ReturnType<typeof counts>;

function receipt(value: unknown) {
  const data = record(value);
  return {
    bindingId: digest(data.bindingId), manifestDigest: digest(data.manifestDigest), finishedAt: text(data.finishedAt),
    checkedFiles: count(data.checkedFiles), checkedBytes: count(data.checkedBytes), counts: counts(data.counts),
    coverageComplete: data.coverageComplete === true, sourceMatches: data.sourceMatches === true,
  };
}

function membership(value: unknown) {
  const data = record(value);
  return {
    bindingId: digest(data.bindingId), manifestDigest: digest(data.manifestDigest), membershipDigest: digest(data.membershipDigest),
    coveredFiles: count(data.coveredFiles), coveredDirectories: count(data.coveredDirectories), exceptions: array(data.exceptions).map(text),
  };
}
export type MirrorMembership = ReturnType<typeof membership>;

function comparison(value: unknown) {
  const data = record(value);
  const operation = record(data.operation);
  const outcome = data.outcome ? record(data.outcome) : null;
  const rawProgress = data.progress ? record(data.progress) : null;
  return {
    operationId: text(operation.operationId), bindingId: digest(operation.bindingId), active: data.active === true,
    monitored: data.freshness === 'monitored', interrupted: data.interruptedOrUnavailable === true,
    outcomeProblem: nullableText(data.outcomeProblem),
    outcome: outcome ? {
      phase: text(outcome.phase), problem: nullableText(outcome.problem), monitoringIssue: nullableText(outcome.monitoringIssue),
      receipt: outcome.receipt ? receipt(outcome.receipt) : null,
    } : null,
    progress: rawProgress ? {
      phase: text(rawProgress.phase), checkedFiles: count(rawProgress.checkedFiles), checkedBytes: count(rawProgress.checkedBytes),
      queuedDirectories: count(rawProgress.queuedDirectories), queuedFiles: count(rawProgress.queuedFiles),
    } : null,
  };
}
export function parseMirrorStatus(value: unknown) {
  const data = record(value);
  const coverage = record(data.coverage);
  return {
    revision: count(data.revision), bindings: array(data.bindings).map(parseMirrorBinding),
    bindingCount: count(data.bindingCount), confirmationCount: count(data.confirmationCount),
    coverage: {
      savedManualPendingMarks: count(coverage.savedManualPendingMarks), deferredInterviewScopes: count(coverage.deferredInterviewScopes),
      missing: observedCount(coverage.missing), different: observedCount(coverage.different), unsupported: observedCount(coverage.unsupported),
      unreadable: observedCount(coverage.unreadable), coverageGaps: observedCount(coverage.coverageGaps),
      historical: coverage.comparisonHistorical === true, scope: text(coverage.coverageScope),
    },
    comparison: data.comparison ? comparison(data.comparison) : null,
    confirmations: array(data.confirmations).map(item => {
      const saved = record(item);
      return { operationId: text(saved.operationId), confirmedAt: text(saved.confirmedAt), bindingId: digest(saved.bindingId),
        manifestDigest: digest(saved.manifestDigest), membershipDigest: digest(saved.membershipDigest),
        coveredFiles: count(saved.coveredFiles), coveredDirectories: count(saved.coveredDirectories), exceptionsCount: count(saved.exceptionsCount) };
    }),
  };
}
export type MirrorStatus = ReturnType<typeof parseMirrorStatus>;

function fileDigest(value: unknown) {
  if (!value) return null;
  const data = record(value);
  return { sha256: digest(data.sha256), bytes: count(data.bytes) };
}
export function parseMirrorPage(value: unknown) {
  const data = record(value);
  const page = record(data.page);
  return {
    receipt: receipt(data.receipt), matched: count(page.matched), nextOffset: page.nextOffset === null ? null : count(page.nextOffset),
    entries: array(page.entries).map(item => {
      const entry = record(item);
      return {
        relative: text(entry.relative), verdict: text(entry.verdict), directory: entry.directory === true,
        coverageGap: entry.coverageGap === true, problem: nullableText(entry.problem),
        sourceProblem: nullableText(entry.sourceProblem), mirrorProblem: nullableText(entry.mirrorProblem),
        source: fileDigest(entry.source), mirror: fileDigest(entry.mirror),
      };
    }),
  };
}
export type MirrorPage = ReturnType<typeof parseMirrorPage>;

export function parsePreparedMirror(value: unknown) {
  const data = record(value);
  const validForMs = count(data.validForMs);
  if (validForMs === 0 || validForMs > 60_000) throw new Error('The prepared membership has an invalid expiration.');
  return { revision: count(data.revision), operationId: text(data.operationId), validForMs, membership: membership(data.membership) };
}
export type PreparedMirror = ReturnType<typeof parsePreparedMirror>;

export function parseSavedMirrorExceptions(value: unknown) {
  const data = record(value);
  const confirmation = record(data.confirmation);
  const membership = record(confirmation.membership);
  if (confirmation.historical !== true) throw new Error('Saved mirror confirmations are historical records.');
  return {
    operationId: text(confirmation.operationId), membershipDigest: digest(membership.membershipDigest),
    total: count(data.total), nextOffset: data.nextOffset === null ? null : count(data.nextOffset),
    exceptions: array(data.exceptions).map(text),
  };
}

export function currentPreparedMirror(prepared: PreparedMirror, status: MirrorStatus, expiresAt: number, now: number): boolean {
  const comparison = status.comparison;
  return now < expiresAt && prepared.revision === status.revision && comparison?.monitored === true &&
    comparison.operationId === prepared.operationId && comparison.bindingId === prepared.membership.bindingId &&
    comparison.outcome?.receipt?.manifestDigest === prepared.membership.manifestDigest;
}
