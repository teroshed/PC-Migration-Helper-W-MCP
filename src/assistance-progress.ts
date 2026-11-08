import { record } from './model.ts';

export interface InventoryProgress {
  processed: number;
  pending: number | null;
  active: number;
  workers: number;
  files: number;
  bytes: number;
  gaps: number;
  currentPath: string | null;
  startedAt: string | null;
}

const count = (value: unknown): number =>
  typeof value === 'number' && Number.isSafeInteger(value) && value >= 0 ? value : 0;

export function parseInventoryProgress(input: unknown): InventoryProgress {
  const scan = record(input);
  return {
    processed: count(scan.processed),
    pending: scan.pending === undefined ? null : count(scan.pending),
    active: count(scan.active), workers: count(scan.workers),
    files: count(scan.files) + count(scan.currentFiles),
    bytes: count(scan.bytes) + count(scan.currentBytes),
    gaps: count(scan.gaps),
    currentPath: typeof scan.currentPath === 'string' ? scan.currentPath : null,
    startedAt: typeof scan.startedAt === 'string' && Number.isFinite(Date.parse(scan.startedAt)) ? scan.startedAt : null,
  };
}

// Discovery has no stable denominator. Reuse the audit UI's indeterminate
// semantics rather than interpreting the growing queue as percent complete.
export function inventoryProgressLabel(progress: InventoryProgress): string {
  return `${progress.processed.toLocaleString()} directories processed${progress.pending === null ? ''
    : `, ${progress.pending.toLocaleString()} queued`}${progress.active ? `, ${progress.active} active` : ''}. Final total unknown.`;
}

export function inventoryElapsed(startedAt: string | null, now: number): string | null {
  if (!startedAt) return null;
  const elapsed = Math.max(0, Math.floor((now - Date.parse(startedAt)) / 1000));
  if (!Number.isFinite(elapsed)) return null;
  if (elapsed < 60) return `${elapsed}s`;
  if (elapsed < 3600) return `${Math.floor(elapsed / 60)}m ${elapsed % 60}s`;
  return `${Math.floor(elapsed / 3600)}h ${Math.floor((elapsed % 3600) / 60)}m`;
}
