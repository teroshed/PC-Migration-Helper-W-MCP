import { parseAuditStats } from './audit-model.ts';
import type { AuditStats, AuditNotification } from './audit-model.ts';
export const STATUSES = [
  'attention', 'migrate-now', 'last-moment', 'redownload', 'erase', 'on-external',
  'manual-pending', 'manual-confirmed',
] as const;
export type Status = typeof STATUSES[number];
export interface PreviewColumn { path: string; page: number; scrollTop: number; width?: number }
export interface ReviewLayout {
  drivesWidth?: number;
  decisionWidth?: number;
  drivesCollapsed?: boolean;
  decisionCollapsed?: boolean;
  toolbar?: 'expanded' | 'compact' | 'path' | 'search';
  fillDepth?: number | null;
}
export interface ViewState {
  path: string;
  selectedPath: string | null;
  search: string;
  filter: string;
  page: number;
  scrollTop: number;
  recentPaths: string[];
  previewColumns: PreviewColumn[];
  previewScrollLeft: number;
  browserColumnWidth?: number;
  layout?: ReviewLayout;
  revealPath: string | null;
}
export interface Decision {
  path: string;
  status: Status;
  notes?: string;
  decidedAt?: string;
  provenance?: unknown;
  verification?: unknown;
  volumeId?: string;
  relativePath?: string;
  [key: string]: unknown;
}
export interface Document {
  schemaVersion: 1;
  revision: number;
  decisions: Record<string, Decision>;
  history: unknown[];
  view: ViewState;
  [key: string]: unknown;
}
export interface Effective {
  status: Status;
  own: boolean;
  inheritedFrom: string | null;
  stale: boolean;
  mixed: number;
  originalStatus: string | null;
  rollup?: { kind: 'unanimous'; status: Status; childCount: number } | { kind: 'mixed'; childCount: number };
}
export interface Entry {
  path: string;
  name: string;
  kind: string;
  size: number;
  modified: string;
  problem: string | null;
  fingerprint: string;
  effective: Effective;
  audit?: AuditStats;
}
export interface Volume { root: string; id: string; label: string; available: boolean; problem: string | null }
export interface GatewayStatus { state: 'running' | 'error' | 'unavailable'; endpoint?: string; instanceId?: string; problem?: string; startedAt?: string }
export interface Boot { document: Document; volumes: Volume[]; dataPath: string; fixture: boolean; problem: string | null; recoveryAvailable: boolean; gateway: GatewayStatus }
export interface Listing { path: string; entries: Entry[]; total: number; attention: number; counts: Record<string, number>; observedAt: string | null }
export interface Details { path: string; effective: Effective; decision: Decision | null; overrides: Decision[]; history: unknown[]; exists: boolean; audit?: AuditStats }
export interface ScanStatus { status?: string; pending: number; processed?: number; files?: number; bytes?: number; currentPath?: string; error?: string }
export interface AppStatus { scan: ScanStatus; gaps: Array<{ path: string; problem: string; observedAt: string }>; decisionCount: number }
export type Operation =
  | { type: 'classify'; path: string; status: Status }
  | { type: 'note'; path: string; notes: string }
  | { type: 'view'; patch: Partial<ViewState> }
  | { type: 'assist'; action: 'scope'; path: string }
  | { type: 'assist'; action: 'split' | 'defer'; path: string; questionId: string }
  | { type: 'assist'; action: 'restore' }
  | { type: 'assistAnswer'; path: string; questionId: string; status: Status }
  | { type: 'undo' };

export function record(value: unknown): Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) throw new Error('The native app returned an invalid response. Your data has not been replaced.');
  return value as Record<string, unknown>;
}
export function isStatus(value: unknown): value is Status { return STATUSES.some(status => status === value); }
export function parseGateway(value: unknown): GatewayStatus {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return { state: 'unavailable', problem: 'This app session does not expose a local MCP gateway.' };
  const gateway = record(value);
  const state = gateway.state === 'running' || gateway.state === 'error' ? gateway.state : 'unavailable';
  return { state, endpoint: typeof gateway.endpoint === 'string' ? gateway.endpoint : undefined, instanceId: typeof gateway.instanceId === 'string' ? gateway.instanceId : undefined, problem: typeof gateway.problem === 'string' ? gateway.problem : undefined, startedAt: typeof gateway.startedAt === 'string' ? gateway.startedAt : undefined };
}
export function parseDocument(value: unknown): Document {
  const v = record(value);
  if (v.schemaVersion !== 1 || typeof v.revision !== 'number' || !Number.isSafeInteger(v.revision) || !Array.isArray(v.history)) throw new Error('Unsupported saved document. Original data is preserved.');
  const decisions = record(v.decisions);
  for (const value of Object.values(decisions)) {
    const d = record(value);
    if (typeof d.path !== 'string' || !isStatus(d.status) || (d.notes !== undefined && typeof d.notes !== 'string') || (d.decidedAt !== undefined && typeof d.decidedAt !== 'string')) throw new Error('A saved decision has an unsupported format.');
  }
  return {
    ...v, schemaVersion: 1, revision: v.revision, decisions: decisions as Record<string, Decision>, history: v.history,
    view: parseView(v.view),
  };
}
export function parseView(value: unknown): ViewState {
  const view = record(value);
  if (typeof view.path !== 'string') throw new Error('The saved review position has an unsupported format.');
  return {
      ...view, path: view.path,
      selectedPath: typeof view.selectedPath === 'string' ? view.selectedPath : null,
      revealPath: typeof view.revealPath === 'string' ? view.revealPath : null,
      search: typeof view.search === 'string' ? view.search : '',
      filter: typeof view.filter === 'string' ? view.filter : 'all',
      page: typeof view.page === 'number' && view.page >= 0 ? Math.floor(view.page) : 0,
      scrollTop: typeof view.scrollTop === 'number' ? view.scrollTop : 0,
      recentPaths: Array.isArray(view.recentPaths) ? view.recentPaths.filter((p): p is string => typeof p === 'string') : [],
      previewColumns: Array.isArray(view.previewColumns) ? view.previewColumns.map(parsePreviewColumn) : [],
      previewScrollLeft: typeof view.previewScrollLeft === 'number' && Number.isFinite(view.previewScrollLeft) ? Math.max(0, view.previewScrollLeft) : 0,
      browserColumnWidth: typeof view.browserColumnWidth === 'number' && Number.isFinite(view.browserColumnWidth) ? Math.max(280, Math.min(800, view.browserColumnWidth)) : undefined,
      layout: parseReviewLayout(view.layout),
  };
}
function parseReviewLayout(value: unknown): ReviewLayout | undefined {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return undefined;
  const layout = record(value);
  return {
    drivesWidth: layoutWidth(layout.drivesWidth, 148),
    decisionWidth: layoutWidth(layout.decisionWidth, 246),
    drivesCollapsed: typeof layout.drivesCollapsed === 'boolean' ? layout.drivesCollapsed : undefined,
    decisionCollapsed: typeof layout.decisionCollapsed === 'boolean' ? layout.decisionCollapsed : undefined,
    toolbar: layout.toolbar === 'expanded' || layout.toolbar === 'compact' || layout.toolbar === 'path'
      || layout.toolbar === 'search' ? layout.toolbar : undefined,
    fillDepth: typeof layout.fillDepth === 'number' && Number.isInteger(layout.fillDepth)
      && layout.fillDepth >= 0 && layout.fillDepth <= 4096 ? layout.fillDepth : null,
  };
}
function layoutWidth(value: unknown, min: number): number | undefined {
  return typeof value === 'number' && Number.isFinite(value) ? Math.max(min, Math.min(520, value)) : undefined;
}
function parsePreviewColumn(value: unknown): PreviewColumn {
  const column = record(value);
  if (typeof column.path !== 'string' || typeof column.page !== 'number' || !Number.isSafeInteger(column.page) || column.page < 0 || typeof column.scrollTop !== 'number' || !Number.isFinite(column.scrollTop) || column.scrollTop < 0) throw new Error('The saved folder preview has an unsupported format.');
  return { path: column.path, page: column.page, scrollTop: column.scrollTop, width: typeof column.width === 'number' && Number.isFinite(column.width) ? Math.max(280, Math.min(800, column.width)) : undefined };
}
export function previewBranch(columns: readonly PreviewColumn[], fromDepth: number, path: string): PreviewColumn[] {
  if (!Number.isSafeInteger(fromDepth) || fromDepth < 0 || fromDepth > columns.length) throw new Error('Invalid preview depth.');
  if (columns[fromDepth]?.path.toLowerCase() === path.toLowerCase()) return [...columns];
  return [...columns.slice(0, fromDepth), { path, page: 0, scrollTop: 0 }];
}
export function closePreviewBranch(columns: readonly PreviewColumn[], depth: number): PreviewColumn[] {
  if (!Number.isSafeInteger(depth) || depth < 1 || depth > columns.length) throw new Error('Invalid preview depth.');
  return columns.slice(0, depth - 1);
}
export function parseEffective(value: unknown): Effective {
  const v = record(value);
  if (!isStatus(v.status) || typeof v.own !== 'boolean' || typeof v.stale !== 'boolean' || typeof v.mixed !== 'number') throw new Error('Invalid native classification response.');
  let rollup: Effective['rollup'];
  if (v.rollup) {
    const summary = record(v.rollup);
    const childCount = summary.childCount;
    if (typeof childCount === 'number' && Number.isSafeInteger(childCount) && childCount > 0) {
      if (summary.kind === 'mixed') rollup = { kind: 'mixed', childCount };
      if (summary.kind === 'unanimous' && isStatus(summary.status)) {
        rollup = { kind: 'unanimous', status: summary.status, childCount };
      }
    }
  }
  return { status: v.status, own: v.own, stale: v.stale, mixed: v.mixed, inheritedFrom: typeof v.inheritedFrom === 'string' ? v.inheritedFrom : null, originalStatus: typeof v.originalStatus === 'string' ? v.originalStatus : null, rollup };
}
export function parseListing(value: unknown): Listing {
  const v = record(value);
  if (typeof v.path !== 'string' || !Array.isArray(v.entries) || typeof v.total !== 'number' || typeof v.attention !== 'number') throw new Error('Invalid native folder response.');
  const entries = v.entries.map(parseEntry);
  const counts: Record<string, number> = {};
  for (const [key, value] of Object.entries(record(v.counts))) if (typeof value === 'number') counts[key] = value;
  return { path: v.path, entries, total: v.total, attention: v.attention, counts, observedAt: typeof v.observedAt === 'string' ? v.observedAt : null };
}
export function parseEntry(value: unknown): Entry {
  const e = record(value);
  if (typeof e.path !== 'string' || typeof e.name !== 'string' || typeof e.kind !== 'string' || typeof e.size !== 'number') throw new Error('Invalid folder entry.');
  return { path: e.path, name: e.name, kind: e.kind, size: e.size, modified: typeof e.modified === 'string' ? e.modified : '', fingerprint: typeof e.fingerprint === 'string' ? e.fingerprint : '', problem: typeof e.problem === 'string' ? e.problem : null, effective: parseEffective(e.effective), audit: parseAuditStats(e.audit) };
}
export function parseDetails(value: unknown): Details {
  const v = record(value);
  if (typeof v.path !== 'string' || !Array.isArray(v.overrides) || !Array.isArray(v.history) || typeof v.exists !== 'boolean') throw new Error('Invalid native details response.');
  const parseDecision = (value: unknown): Decision => {
    const d = record(value);
    if (typeof d.path !== 'string' || !isStatus(d.status) || (d.notes !== undefined && typeof d.notes !== 'string') || (d.decidedAt !== undefined && typeof d.decidedAt !== 'string')) throw new Error('Invalid decision details.');
    return { ...d, path: d.path, status: d.status };
  };
  return { path: v.path, exists: v.exists, effective: parseEffective(v.effective), decision: v.decision === null ? null : parseDecision(v.decision), overrides: v.overrides.map(parseDecision), history: v.history, audit: parseAuditStats(v.audit) };
}

interface PendingMutation { id: string; operation: Operation; resolve: (doc: Document) => void; reject: (error: Error) => void }
export interface QueueState { pending: number; saving: boolean; error: string | null }
export type MutationTransport = (revision: number, id: string, operation: Operation) => Promise<unknown>;

/** One native writer, ordered edits, and retained requests after an uncertain acknowledgement. */
export class MutationQueue {
  private document: Document;
  private pending: PendingMutation[] = [];
  private running = false;
  private externalHold = false;
  private inspectionGeneration = 0;
  private failure: string | null = null;
  private waiters: Array<{ resolve: () => void; reject: (error: Error) => void }> = [];
  private transport: MutationTransport;
  private changed: (document: Document, state: QueueState) => void;
  constructor(document: Document, transport: MutationTransport, changed: (document: Document, state: QueueState) => void) {
    this.document = document;
    this.transport = transport;
    this.changed = changed;
  }
  get state(): QueueState { return { pending: this.pending.length, saving: this.externalHold || this.running || this.pending.length > 0, error: this.failure }; }
  get revision(): number { return this.document.revision; }
  get inspectionVersion(): number { return this.inspectionGeneration; }
  get idle(): boolean { return !this.running && !this.pending.length && !this.failure && !this.externalHold; }
  get retainedOperations(): readonly Operation[] { return this.pending.map(item => item.operation); }
  get retainedRequests(): readonly { id: string; operation: Operation }[] { return this.pending.map(({ id, operation }) => ({ id, operation })); }
  acceptExternal(value: unknown): boolean {
    if (!this.idle) return false;
    const document = parseDocument(value);
    if (document.revision <= this.document.revision) return false;
    this.document = document;
    this.notify();
    return true;
  }
  rebaseAfterReview(value: unknown): boolean {
    if (this.running || this.externalHold || !this.failure || !this.pending.length) return false;
    const document = parseDocument(value);
    if (document.revision < this.document.revision) return false;
    this.document = document;
    // Retain both requests and failure until the explicit retry that follows review.
    this.notify();
    return true;
  }
  holdForExternal(): void {
    if (!this.idle) throw new Error('Finish pending saves before starting another native operation.');
    this.externalHold = true;
    this.notify();
  }
  finishExternal(value: unknown): void {
    if (!this.externalHold || this.running) throw new Error('No serialized native operation is active.');
    const document = parseDocument(value);
    if (document.revision < this.document.revision) throw new Error('The native operation returned an older saved revision.');
    this.document = document;
    this.externalHold = false;
    this.notify();
    void this.drain();
  }
  failExternal(cause: unknown): void {
    this.externalHold = false;
    this.failure = `The native operation needs reconciliation before pending edits can save: ${String(cause)}`;
    for (const waiter of this.waiters.splice(0)) waiter.reject(new Error(this.failure));
    this.notify();
  }
  request(operation: Operation): Promise<Document> {
    const promise = new Promise<Document>((resolve, reject) => {
      this.pending.push({ id: globalThis.crypto.randomUUID(), operation, resolve, reject });
    });
    this.notify();
    void this.drain();
    return promise;
  }
  retry(): void { this.failure = null; void this.drain(); }
  flush(): Promise<void> {
    if (this.failure) return Promise.reject(new Error(this.failure));
    if (!this.pending.length && !this.running && !this.externalHold) return Promise.resolve();
    return new Promise((resolve, reject) => this.waiters.push({ resolve, reject }));
  }
  private notify(): void { this.inspectionGeneration += 1; this.changed(this.document, this.state); }
  private async drain(): Promise<void> {
    if (this.running || this.failure || this.externalHold) return;
    this.running = true;
    this.notify();
    while (this.pending.length) {
      const item = this.pending[0];
      if (!item) break;
      try {
        const response = await this.transport(this.document.revision, item.id, item.operation);
        const next = parseDocument(response);
        const acknowledgedEarlier = next.revision === this.document.revision && Array.isArray(next.recentOperations) && next.recentOperations.includes(item.id);
        if (next.revision < this.document.revision || next.revision === this.document.revision && !acknowledgedEarlier) throw new Error('A stale save response was ignored. Your pending edit is retained; newer saved data was not replaced.');
        this.document = next;
        this.pending.shift();
        item.resolve(this.document);
        this.notify();
      } catch (cause) {
        this.failure = cause instanceof Error ? cause.message : String(cause);
        this.running = false;
        // Do not discard or replay the failed operation. Retry uses the same idempotency key.
        for (const waiter of this.waiters.splice(0)) waiter.reject(new Error(this.failure));
        this.notify();
        return;
      }
    }
    this.running = false;
    this.notify();
    for (const waiter of this.waiters.splice(0)) waiter.resolve();
  }
}

export function parentPath(path: string): string | null {
  const normalized = path.replaceAll('/', '\\').replace(/\\+$/, '');
  if (/^[A-Za-z]:$/.test(normalized)) return null;
  const index = normalized.lastIndexOf('\\');
  if (index < 2) return null;
  return index === 2 ? normalized.slice(0, 3) : normalized.slice(0, index);
}
export function baseName(path: string): string { return path.replace(/\\+$/, '').split('\\').at(-1) || path; }
export function normalizeReviewPath(path: string, roots: readonly string[]): string {
  let normalized = path.replaceAll('/', '\\');
  if (!/^[a-z]:\\/i.test(normalized) || normalized.slice(2).includes(':') || normalized.split('\\').some(part => part === '.' || part === '..' || part.endsWith(' ') || part.endsWith('.'))) throw new Error('Enter an absolute path inside the review drives. Relative segments, alternate streams and ambiguous paths are not supported.');
  if (normalized.length > 3) normalized = normalized.replace(/\\+$/, '');
  const lower = normalized.toLowerCase();
  if (!roots.some(root => { const prefix = root.replace(/\\+$/, '').toLowerCase(); return lower === prefix || lower.startsWith(`${prefix}\\`); })) throw new Error('That path is outside the connected review roots. Your saved review position has not changed.');
  return normalized;
}
export function resolveDecisionPath(decision: Decision, volumes: readonly Volume[]): { path: string | null; explanation: string | null } {
  if (typeof decision.volumeId !== 'string' || typeof decision.relativePath !== 'string') return { path: null, explanation: 'This older record has no usable volume identity. It is preserved here; automatic navigation is unavailable.' };
  const volume = volumes.find(volume => volume.id === decision.volumeId);
  if (!volume?.available) return { path: null, explanation: 'The recorded volume is offline or outside the review roots. Its saved record is preserved; the current disk at that drive letter is not substituted.' };
  const path = decision.relativePath ? `${volume.root.replace(/\\+$/, '')}\\${decision.relativePath}` : volume.root;
  try { return { path: normalizeReviewPath(path, [volume.root]), explanation: null }; }
  catch { return { path: null, explanation: 'The recorded relative path cannot be resolved safely. The original record is preserved.' }; }
}
export function notificationLocation(notification: AuditNotification, volumes: readonly Volume[]): { path: string; selection: string | null } | null {
  if (!notification.navigable || !notification.path) return null;
  const available = volumes.filter(volume => volume.available && (!notification.volumeId || notification.volumeId === volume.id));
  if (!available.length) return null;
  try {
    const path = normalizeReviewPath(notification.path, available.map(volume => volume.root));
    let selection: string | null = null;
    if (notification.originalPath) {
      const original = normalizeReviewPath(notification.originalPath, available.map(volume => volume.root));
      const prefix = path.replace(/\\+$/, '').toLowerCase();
      if (original.toLowerCase() === prefix || original.toLowerCase().startsWith(`${prefix}\\`)) selection = original;
    }
    return { path, selection };
  } catch { return null; }
}
export function readableBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 B';
  const unit = Math.min(4, Math.floor(Math.log(bytes) / Math.log(1000)));
  return `${(bytes / 1000 ** unit).toLocaleString(undefined, { maximumFractionDigits: unit ? 1 : 0 })} ${['B', 'KB', 'MB', 'GB', 'TB'][unit]}`;
}
