import { useState } from 'react';
import { AlertTriangle, Folder, File, X } from 'lucide-react';
import { DECISION_CHOICES, DESCRIPTIONS, ICONS, LABELS, decisionChoiceLabel } from './classifications';
import type { GroupTarget, Operation, Status } from './model';
import { selectedFoldersWithDescendants, selectionKey } from './review-selection';
import type { SavedGroup } from './review-selection';
import './selection-review.css';

export type GroupOperation = Extract<Operation, { type: 'bulkClassify' | 'undoGroup' }>;
interface ReviewBase { operationId: string; revision: number }
export type GroupReview =
  | ReviewBase & { mode: 'mark'; rootId: string; signature: string; targets: readonly GroupTarget[] }
  | ReviewBase & { mode: 'undo'; group: SavedGroup };

interface Props {
  review: GroupReview;
  attempted: GroupOperation | null;
  busy: boolean;
  error: string | null;
  onSubmit: (operation: GroupOperation) => void;
  onRefresh: () => void;
  onRemove: (target: GroupTarget) => void;
}

function statusLabel(status: Status | null): string {
  return status ? LABELS[status] : 'No explicit mark';
}

export function SelectionReview({ review, attempted, busy, error, onSubmit, onRefresh, onRemove }: Props) {
  const [status, setStatus] = useState<Status | null>(null);
  const [folderAcknowledged, setFolderAcknowledged] = useState(false);
  const [queueAcknowledged, setQueueAcknowledged] = useState(false);
  const targets = review.mode === 'mark' ? review.targets : review.group.members.map(member => member.target);
  const nested = selectedFoldersWithDescendants(targets);
  const folderEffects = review.mode === 'mark' && targets.some(target => target.kind === 'folder');
  const disabled = busy || attempted !== null;
  const ready = review.mode === 'undo'
    ? queueAcknowledged : status !== null && queueAcknowledged && (!folderEffects || folderAcknowledged);
  function apply(): void {
    if (attempted) { onSubmit(attempted); return; }
    if (!ready || busy) return;
    if (review.mode === 'undo') {
      onSubmit({ type: 'undoGroup', groupId: review.group.id, confirmed: true, acknowledgeQueueEffects: true });
    } else if (status) {
      onSubmit({ type: 'bulkClassify', targets: [...targets], status, confirmed: true,
        ...(folderEffects ? { acknowledgeFolderEffects: true } : {}), acknowledgeQueueEffects: true });
    }
  }
  return <div className="group-review">
    <p className="modal-intro">{review.mode === 'mark'
      ? `Review all ${targets.length} selected entries. They will receive one saved decision together.`
      : `Review the previous decisions for all ${targets.length} group members. Undo applies to the entire group only while every member still matches the saved group result.`}</p>
    {review.mode === 'mark' && <div className="group-status-options" role="group" aria-label="Decision for selected entries">
      {DECISION_CHOICES.map(mark => {
        const Icon = ICONS[mark];
        return <button key={mark} type="button" aria-pressed={mark === status} disabled={disabled}
          title={DESCRIPTIONS[mark]} className={`classification-button status-${mark}${mark === status ? ' selected' : ''}`}
          onClick={() => { setStatus(mark); setFolderAcknowledged(false); setQueueAcknowledged(false); }}>
          <Icon size={15} aria-hidden="true"/><span><strong>{decisionChoiceLabel(mark)}</strong></span>
        </button>;
      })}
    </div>}
    <ol className="group-targets" aria-label="Exact reviewed entries">
      {targets.map((target, index) => {
        const key = selectionKey(target);
        const Icon = target.kind === 'folder' ? Folder : File;
        const member = review.mode === 'undo' ? review.group.members[index] : undefined;
        return <li key={key}>
          <Icon size={15} aria-hidden="true"/>
          <div><span className="group-target-path">{target.path}</span>
            {nested.has(key) && <small>Contains another selected entry</small>}
            {member && <small>{statusLabel(member.after)} → {statusLabel(member.before)}</small>}
          </div>
          {review.mode === 'mark' && <button type="button" disabled={disabled} aria-label={`Remove ${target.path} from selection`}
            className="icon-button" onClick={() => onRemove(target)}><X size={15} aria-hidden="true"/></button>}
        </li>;
      })}
    </ol>
    <div className="group-review-controls">
      {folderEffects && <label>
        <input type="checkbox" checked={folderAcknowledged} disabled={disabled} onChange={event => setFolderAcknowledged(event.target.checked)}/>
        <span>I reviewed the folder effects. Ordinary folder decisions apply to otherwise unmarked descendants. Existing child exceptions stay in place. Manual confirmations and Already on F: apply to each selected path.</span>
      </label>}
      <label>
        <input type="checkbox" checked={queueAcknowledged} disabled={disabled} onChange={event => setQueueAcknowledged(event.target.checked)}/>
        <span>I reviewed the copy queue effects. Changing Migrate now entries can start a pass when Auto-copy is already enabled. An active pass may pause while the queue is rechecked.</span>
      </label>
      {error && <p className="group-review-error" role="alert"><AlertTriangle size={15} aria-hidden="true"/>{error}</p>}
      <div className="group-review-actions">
        {error && !attempted && <button type="button" disabled={busy} onClick={onRefresh}>Review current entries again</button>}
        <button type="button" className="primary" disabled={busy || !attempted && !ready} onClick={apply}>
          {busy ? 'Saving group…' : attempted ? 'Retry the same request'
            : review.mode === 'undo' ? `Undo group of ${targets.length}` : `Mark ${targets.length} selected`}
        </button>
      </div>
    </div>
  </div>;
}
