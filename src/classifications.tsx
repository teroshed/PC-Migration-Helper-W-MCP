import { ArrowDownToLine, CheckCircle2, CircleHelp, Clock3, Download, FolderCheck, FolderClock, Info, Trash2, TriangleAlert } from 'lucide-react';
import { STATUSES } from './model';
import type { Effective, Status } from './model';

export const LABELS: Record<Status, string> = {
  attention: 'Needs attention', 'migrate-now': 'Migrate now', 'last-moment': 'Migrate Last Moment',
  redownload: 'To Redownload', erase: 'To be ERASED', 'on-external': 'Already on F:',
  'manual-pending': 'Marked for Manual Backup', 'manual-confirmed': 'Confirmed Manual Backup',
};
export const DESCRIPTIONS: Record<Status, string> = {
  attention: 'No migration decision yet', 'migrate-now': 'Pending · ready for an early copy',
  'last-moment': 'Capture again before the final shutdown', redownload: 'Keep the decision to download it again',
  erase: 'Saved decision only · never deletes anything', 'on-external': 'Your manual assertion · not a verified backup',
  'manual-pending': 'Manual copy still pending · confirm after you finish copying',
  'manual-confirmed': 'Your confirmation for this path · review children separately · checksum verification is separate',
};
export const ICONS = {
  attention: CircleHelp, 'migrate-now': ArrowDownToLine, 'last-moment': Clock3,
  redownload: Download, erase: Trash2, 'on-external': CheckCircle2,
  'manual-pending': FolderClock, 'manual-confirmed': FolderCheck,
};
export const EMPTY_EFFECTIVE: Effective = { status: 'attention', own: false, inheritedFrom: null, stale: false, mixed: 0, originalStatus: null };
export function reviewStatusLabel(effective: Effective): string {
  const summary = effective.rollup;
  if (summary?.kind === 'mixed') return 'Mixed child decisions';
  if (summary?.kind === 'unanimous') return `Children · ${LABELS[summary.status]}`;
  if (summary?.kind === 'unknown') {
    if (!summary.coverage?.complete) return 'Child coverage incomplete';
    if (summary.coverage.undecidedCount > 0) return 'Child decisions pending';
    return 'No current child marks';
  }
  return LABELS[effective.status];
}

export function StatusBadge({ effective, compact = false }: { effective: Effective; compact?: boolean }) {
  const rolledUp = effective.rollup;
  const Icon = rolledUp?.kind === 'unknown' && !rolledUp.coverage?.complete ? TriangleAlert
    : rolledUp?.kind === 'mixed' ? CheckCircle2 : ICONS[effective.status];
  return <span className={`status-badge status-${effective.status}${compact ? ' compact' : ''}${rolledUp?.kind === 'mixed' ? ' status-rollup-mixed' : ''}`}
    title={rolledUp?.coverage?.problem ?? (rolledUp ? `Observed direct children · ${rolledUp.childCount.toLocaleString()}` : undefined)}>
    <Icon size={compact ? 13 : 14} aria-hidden="true" />
    <span>{reviewStatusLabel(effective)}{effective.status === 'migrate-now' && !rolledUp ? ' · pending' : ''}</span>
    {effective.mixed > 0 && !rolledUp && <span className="mixed-tag">Mixed</span>}
    {effective.stale && <TriangleAlert size={13} aria-label="Changed since decision" />}
  </span>;
}

export function RollupCoverageDetails({ effective }: { effective: Effective }) {
  const summary = effective.rollup;
  if (!summary) return null;
  const coverage = summary.coverage;
  const warning = summary.kind === 'unknown' && !coverage?.complete;
  const Icon = warning ? TriangleAlert : Info;
  const marks = STATUSES.flatMap(mark => {
    const count = coverage?.statusCounts[mark];
    return count ? [`${LABELS[mark]} ${count.toLocaleString()}`] : [];
  }).join(' · ');
  return <div className={`context-box${warning ? ' warning' : ''}`}>
    <Icon size={16} aria-hidden="true"/>
    <div>
      <p><strong>{coverage?.complete ? 'Observed child coverage complete' : warning ? 'Observed child coverage incomplete' : 'Observed child decisions'}</strong></p>
      {coverage?.problem && <p>{coverage.problem}</p>}
      {coverage && <p>Direct children {summary.childCount.toLocaleString()} · Observed files {coverage.leafCount.toLocaleString()} · Empty folders {coverage.emptyFolderCount.toLocaleString()}</p>}
      {coverage && <p>Undecided {coverage.undecidedCount.toLocaleString()} · Unknown branches {coverage.unknownBranches.toLocaleString()} · Coverage gaps {coverage.coverageGaps.toLocaleString()}</p>}
      {marks && <p>{marks}</p>}
      <p>This summary reflects observed review decisions. Backup verification and copy eligibility are checked separately.</p>
    </div>
  </div>;
}
