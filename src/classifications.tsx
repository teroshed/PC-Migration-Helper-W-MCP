import { ArrowDownToLine, CheckCircle2, CircleHelp, Clock3, Download, Trash2, TriangleAlert } from 'lucide-react';
import type { Effective, Status } from './model';

export const LABELS: Record<Status, string> = {
  attention: 'Needs attention', 'migrate-now': 'Migrate now', 'last-moment': 'Migrate Last Moment',
  redownload: 'To Redownload', erase: 'To be ERASED', 'on-external': 'Already on F:',
};
export const DESCRIPTIONS: Record<Status, string> = {
  attention: 'No migration decision yet', 'migrate-now': 'Pending · ready for an early copy',
  'last-moment': 'Capture again before the final shutdown', redownload: 'Keep the decision to download it again',
  erase: 'Saved decision only · never deletes anything', 'on-external': 'Your manual assertion · not a verified backup',
};
export const ICONS = { attention: CircleHelp, 'migrate-now': ArrowDownToLine, 'last-moment': Clock3, redownload: Download, erase: Trash2, 'on-external': CheckCircle2 };
export const EMPTY_EFFECTIVE: Effective = { status: 'attention', own: false, inheritedFrom: null, stale: false, mixed: 0, originalStatus: null };
export function reviewStatusLabel(effective: Effective): string {
  if (effective.rollup?.kind === 'mixed') return 'Mixed child decisions';
  if (effective.rollup?.kind === 'unanimous') return `Children · ${LABELS[effective.rollup.status]}`;
  return LABELS[effective.status];
}

export function StatusBadge({ effective, compact = false }: { effective: Effective; compact?: boolean }) {
  const rolledUp = effective.rollup;
  const Icon = rolledUp?.kind === 'mixed' ? CheckCircle2 : ICONS[effective.status];
  return <span className={`status-badge status-${effective.status}${compact ? ' compact' : ''}${rolledUp?.kind === 'mixed' ? ' status-rollup-mixed' : ''}`}
    title={rolledUp ? `Summary of ${rolledUp.childCount} marked children` : undefined}>
    <Icon size={compact ? 13 : 14} aria-hidden="true" />
    <span>{reviewStatusLabel(effective)}{effective.status === 'migrate-now' && !rolledUp ? ' · pending' : ''}</span>
    {effective.mixed > 0 && !rolledUp && <span className="mixed-tag">Mixed</span>}
    {effective.stale && <TriangleAlert size={13} aria-label="Changed since decision" />}
  </span>;
}
