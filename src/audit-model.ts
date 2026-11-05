export type AuditPhase = 'idle' | 'discovering' | 'hashing' | 'paused' | 'complete' | 'error';
export interface AuditStats {
  phase: AuditPhase;
  discoveredLeaves: number;
  countedLeaves: number;
  hashedLeaves: number;
  failedLeaves: number;
  logicalBytes: number;
  allocatedBytes: number;
  allocationUnknown: number;
  totalKnown: boolean;
  complete: boolean;
  sha256?: string;
  problem?: string;
  verifiedAt?: string;
  hashState?: string;
  coverageGaps?: number;
  scopeNotes?: string;
  cached?: boolean;
}
export interface AuditStatus extends AuditStats {
  workers: number;
  coverageGaps: number;
  notificationsUnread: number;
  currentPath: string | null;
  updatedAt?: string;
  changeSequence?: number;
  gapSamples: Array<{ path: string; problem: string; kind: string; observedAt?: string }>;
  exclusions: string[];
  reconciliationRequired: boolean;
}
export interface AuditNotification {
  id: string;
  kind: string;
  title: string;
  message: string;
  path: string | null;
  originalPath?: string;
  navigable: boolean;
  volumeId?: string;
  relativePath?: string;
  createdAt: string;
  readAt: string | null;
  changeCount: number;
}
export interface NotificationFeed { items: AuditNotification[]; total: number; unread: number }
const PHASES = ['idle', 'discovering', 'hashing', 'paused', 'complete', 'error'];
function object(value: unknown): Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) throw new Error('Invalid native audit response.');
  return value as Record<string, unknown>;
}
function count(value: unknown, field: string, fallback?: number): number {
  if (value === undefined && fallback !== undefined) return fallback;
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0) throw new Error(`Invalid audit ${field}.`);
  return value;
}
export function parseAuditStats(value: unknown): AuditStats | undefined {
  if (value === undefined || value === null) return undefined;
  const stats = object(value);
  if (typeof stats.phase !== 'string' || !PHASES.includes(stats.phase) || typeof stats.totalKnown !== 'boolean') throw new Error('Invalid native audit progress.');
  const phase = stats.phase as AuditPhase;
  return {
    phase,
    discoveredLeaves: count(stats.discoveredLeaves, 'discovered leaves'),
    countedLeaves: count(stats.countedLeaves, 'counted leaves'),
    hashedLeaves: count(stats.hashedLeaves, 'hashed leaves'),
    failedLeaves: count(stats.failedLeaves, 'failed leaves'),
    logicalBytes: count(stats.logicalBytes, 'logical bytes'),
    allocatedBytes: count(stats.allocatedBytes, 'allocated bytes'),
    allocationUnknown: count(stats.allocationUnknown, 'unknown allocations'),
    totalKnown: stats.totalKnown,
    complete: typeof stats.complete === 'boolean' ? stats.complete : phase === 'complete',
    sha256: typeof stats.sha256 === 'string' && /^[a-f0-9]{64}$/i.test(stats.sha256) ? stats.sha256 : undefined,
    problem: typeof stats.problem === 'string' ? stats.problem : typeof stats.error === 'string' ? stats.error : undefined,
    verifiedAt: typeof stats.verifiedAt === 'string' ? stats.verifiedAt : undefined,
    hashState: typeof stats.hashState === 'string' ? stats.hashState : undefined,
    coverageGaps: typeof stats.coverageGaps === 'number' ? count(stats.coverageGaps, 'coverage gaps') : undefined,
    scopeNotes: typeof stats.scopeNotes === 'string' ? stats.scopeNotes : undefined,
    cached: stats.cached === true,
  };
}
export function parseAuditStatus(value: unknown): AuditStatus {
  const native = object(value);
  const stats = parseAuditStats(native);
  if (!stats) throw new Error('Native audit status is unavailable.');
  const gapSamples = Array.isArray(native.gapSamples) ? native.gapSamples.slice(0, 50).map(value => {
    const gap = object(value);
    if (typeof gap.path !== 'string' || typeof gap.problem !== 'string') throw new Error('Invalid source audit coverage sample.');
    return { path: gap.path, problem: gap.problem, kind: typeof gap.kind === 'string' ? gap.kind : 'unavailable', observedAt: typeof gap.observedAt === 'string' ? gap.observedAt : undefined };
  }) : [];
  return { ...stats, workers: count(native.workers, 'workers', 0), coverageGaps: count(native.coverageGaps, 'coverage gaps', 0), notificationsUnread: count(native.notificationsUnread, 'unread notifications', 0), currentPath: typeof native.currentPath === 'string' ? native.currentPath : null, updatedAt: typeof native.updatedAt === 'string' ? native.updatedAt : undefined, changeSequence: typeof native.changeSequence === 'number' ? count(native.changeSequence, 'change sequence') : undefined, gapSamples, exclusions: Array.isArray(native.exclusions) ? native.exclusions.filter((path): path is string => typeof path === 'string') : [], reconciliationRequired: native.reconciliationRequired === true };
}
export function parseNotificationFeed(value: unknown): NotificationFeed {
  const feed = object(value);
  if (!Array.isArray(feed.items)) throw new Error('The native notification list is unavailable.');
  return {
    items: feed.items.map(value => {
      const item = object(value);
      const id = typeof item.id === 'string' ? item.id : typeof item.id === 'number' && Number.isSafeInteger(item.id) ? String(item.id) : null;
      if (!id || typeof item.title !== 'string' || typeof item.message !== 'string' || typeof item.createdAt !== 'string') throw new Error('A native notification has an unsupported format.');
      return { id, kind: typeof item.kind === 'string' ? item.kind : 'information', title: item.title, message: item.message, path: typeof item.path === 'string' ? item.path : null, originalPath: typeof item.originalPath === 'string' ? item.originalPath : undefined, navigable: item.navigable === true, volumeId: typeof item.volumeId === 'string' ? item.volumeId : undefined, relativePath: typeof item.relativePath === 'string' ? item.relativePath : undefined, createdAt: item.createdAt, readAt: typeof item.readAt === 'string' ? item.readAt : null, changeCount: count(item.changeCount, 'notification change count', 1) };
    }),
    total: count(feed.total, 'notification total'),
    unread: count(feed.unread, 'unread notifications', 0),
  };
}
export function auditProgress(stats: AuditStats | undefined, mode: 'count' | 'hash' = 'count'): { fraction: number | null; processed: number; total: number | null; label: string } {
  if (!stats) return { fraction: null, processed: 0, total: null, label: 'Not counted yet; total leaves unknown.' };
  const processed = mode === 'hash' ? stats.hashedLeaves : stats.countedLeaves;
  const verb = mode === 'hash' ? 'SHA-256 hashed' : 'counted';
  if (!stats.totalKnown) return { fraction: null, processed, total: null, label: `${processed.toLocaleString()} leaves ${verb}; ${stats.discoveredLeaves.toLocaleString()} discovered so far, final total unknown.` };
  if (stats.discoveredLeaves === 0) return { fraction: stats.complete && !stats.failedLeaves ? 1 : 0, processed, total: 0, label: stats.complete && !stats.failedLeaves ? 'No leaf files in this completed scope.' : 'No leaf files reported yet; audit is not complete.' };
  const fraction = stats.discoveredLeaves > 0 ? Math.min(1, processed / stats.discoveredLeaves) : stats.complete && !stats.failedLeaves ? 1 : 0;
  return { fraction, processed, total: stats.discoveredLeaves, label: `${processed.toLocaleString()} of ${stats.discoveredLeaves.toLocaleString()} leaves ${verb}${stats.failedLeaves ? `; ${stats.failedLeaves.toLocaleString()} failed` : ''}${stats.coverageGaps ? `; ${stats.coverageGaps.toLocaleString()} coverage gaps remain` : ''}.` };
}
export function allocatedState(stats: AuditStats | undefined): { kind: 'unknown' | 'partial' | 'known'; bytes: number | null } {
  if (!stats) return { kind: 'unknown', bytes: null };
  if (stats.allocationUnknown > 0 && stats.allocatedBytes === 0) return { kind: 'unknown', bytes: null };
  if (stats.allocationUnknown > 0 || !stats.totalKnown || stats.problem || stats.coverageGaps || stats.countedLeaves < stats.discoveredLeaves) return { kind: 'partial', bytes: stats.allocatedBytes };
  return { kind: 'known', bytes: stats.allocatedBytes };
}
export function auditRefreshKey(status: AuditStatus): string {
  const { currentPath, workers, ...relevant } = status;
  return JSON.stringify(relevant);
}
