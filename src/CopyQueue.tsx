import { useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { Copy, ListOrdered, Pause, TriangleAlert, X } from 'lucide-react';
import { record } from './model';
import { OverflowIndicators } from './OverflowIndicators';
import type { UiControlRef, UiRequest } from './UIBridge';

type CopyMode = 'ordered' | 'all' | 'last-moment';

interface QueueItem {
  path: string;
  state: string;
}

interface QueueStatus {
  jobId: string;
  phase: string;
  currentPath: string;
  copied: number;
  skipped: number;
  failed: number;
  problem: string;
  issues: { path: string; problem: string }[];
}

interface QueueSnapshot {
  instanceId: string;
  sourceVolumeSignature: string;
  revision: number;
  destination: { root: string; id: string; available: boolean; problem: string };
  migrateNow: QueueItem[];
  lastMoment: QueueItem[];
  migrateNowCount: number;
  lastMomentCount: number;
  pendingMigrateNow: number;
  queueSignature: string;
  latestOperation: { operationId: string; phase: string; error: string } | null;
  copyAuthority: CopyAuthority;
  status: QueueStatus;
}
interface CopyEffect {
  operationId: string;
  jobId: string;
  phase: string;
  action: string;
  destinationId: string;
  requestedAt: string;
  committedRevision: number | null;
}
interface CopyAuthority {
  phase: string;
  ready: boolean;
  ledgerDigest: string;
  historyCount: number;
  unreconciledCount: number;
  unreconciled: CopyEffect[];
  problem: string;
}

interface ReviewedConflict {
  sourcePath: string;
  existingPath: string;
  alternatePath: string;
  alternateName: string;
  conflictFingerprint: string;
  queue: QueueSnapshot;
  operationId: string;
}
class SeparateCopyRefusal extends Error {}

function string(value: unknown): string {
  return typeof value === 'string' ? value : '';
}

function count(value: unknown): number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0 ? value : 0;
}

function items(value: unknown): QueueItem[] {
  if (!Array.isArray(value)) return [];
  return value.map(record).flatMap(item => {
    const path = string(item.path);
    return path ? [{ path, state: string(item.state) }] : [];
  });
}

function copyEffect(value: unknown): CopyEffect {
  const effect = record(value);
  return {
    operationId: string(effect.operationId), jobId: string(effect.jobId), phase: string(effect.phase),
    action: string(effect.action), destinationId: string(effect.destinationId), requestedAt: string(effect.requestedAt),
    committedRevision: typeof effect.committedRevision === 'number' ? count(effect.committedRevision) : null,
  };
}

function copyAuthority(value: unknown): CopyAuthority {
  const authority = value == null ? {} : record(value);
  const phase = string(authority.phase) || 'unavailable';
  return {
    phase, ready: (phase === 'ready' || phase === 'uninitialized') && authority.ready === true,
    ledgerDigest: string(authority.ledgerDigest),
    historyCount: count(authority.historyCount), unreconciledCount: count(authority.unreconciledCount),
    unreconciled: Array.isArray(authority.unreconciled) ? authority.unreconciled.map(copyEffect) : [],
    problem: string(authority.problem) || (value == null ? 'Copy authority status is unavailable.' : ''),
  };
}

function snapshot(value: unknown): QueueSnapshot {
  const queue = record(value);
  const destination = record(queue.destination);
  const status = record(queue.status);
  const migrateNow = items(queue.migrateNow);
  const latest = queue.latestOperation ? record(queue.latestOperation) : null;
  return {
    instanceId: string(queue.instanceId),
    sourceVolumeSignature: string(queue.sourceVolumeSignature),
    revision: count(queue.revision),
    destination: {
      root: string(destination.root),
      id: string(destination.id),
      available: destination.available === true,
      problem: string(destination.problem),
    },
    migrateNow,
    lastMoment: items(queue.lastMoment),
    migrateNowCount: count(queue.migrateNowCount),
    lastMomentCount: count(queue.lastMomentCount),
    pendingMigrateNow: count(queue.pendingMigrateNow),
    queueSignature: string(queue.queueSignature),
    latestOperation: latest ? { operationId: string(latest.operationId), phase: string(latest.phase), error: string(latest.error) } : null,
    copyAuthority: copyAuthority(queue.copyAuthority),
    status: {
      jobId: string(status.jobId),
      phase: string(status.phase),
      currentPath: string(status.currentPath),
      copied: count(status.copied),
      skipped: count(status.skipped),
      failed: count(status.failed),
      problem: string(status.problem),
      issues: Array.isArray(status.issues) ? status.issues.slice(0, 50).map(record).map(item => ({
        path: string(item.path),
        problem: string(item.message),
      })) : [],
    },
  };
}

function sameCopyReview(first: QueueSnapshot, second: QueueSnapshot): boolean {
  return first.revision === second.revision && first.instanceId === second.instanceId &&
    first.queueSignature === second.queueSignature && first.sourceVolumeSignature === second.sourceVolumeSignature &&
    first.destination.id === second.destination.id && second.destination.available;
}

function isDifferentFile(problem: string): boolean {
  return problem.includes('Destination already contains a different file.');
}

export function CopyQueue({ revision, enabled, controlRef, onNativeWrite }: {
  revision: number; enabled: boolean; controlRef: UiControlRef;
  onNativeWrite: <T>(operation: () => Promise<T>) => Promise<T>;
}) {
  const [queue, setQueue] = useState<QueueSnapshot | null>(null);
  const [auto, setAuto] = useState(false);
  const [details, setDetails] = useState(false);
  const [busy, setBusy] = useState(false);
  const [issue, setIssue] = useState('');
  const [separateSource, setSeparateSource] = useState<string | null>(null);
  const [authorityReview, setAuthorityReview] = useState<QueueSnapshot | null>(null);
  const read = useRef<() => void>(() => {});
  const request = useRef(false);
  const armedDestination = useRef('');
  const armedSources = useRef('');
  const attempted = useRef('');
  const listRef = useRef<HTMLDivElement>(null);
  const countRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    let active = true;
    let pending = false;
    const refresh = async () => {
      if (pending) return;
      pending = true;
      try {
        const next = snapshot(await invoke<unknown>('copy_queue_snapshot'));
        if (!active) return;
        if (armedDestination.current && !next.copyAuthority.ready) {
          armedDestination.current = '';
          armedSources.current = '';
          setAuto(false);
          setIssue('Auto-copy paused while recorded copy effects need review.');
        } else if (armedDestination.current && (!next.destination.available || next.destination.id !== armedDestination.current || next.sourceVolumeSignature !== armedSources.current)) {
          armedDestination.current = '';
          setAuto(false);
          setIssue('Auto-copy paused because a source or destination changed. Check the drives before enabling it again.');
        }
        if (next.status.phase === 'paused' && armedDestination.current) {
          armedDestination.current = '';
          setAuto(false);
        }
        setQueue(next);
      } catch (error) {
        if (active && enabled) {
          armedDestination.current = '';
          setAuto(false);
          setIssue(String(error));
        }
      } finally {
        pending = false;
      }
    };
    read.current = () => { void refresh(); };
    read.current();
    const timer = setInterval(read.current, 1500);
    return () => {
      active = false;
      clearInterval(timer);
      read.current = () => {};
    };
  }, [enabled]);

  useEffect(() => read.current(), [revision]);

  const phase = queue?.status.phase ?? '';
  const copying = phase === 'planning' || phase === 'running' || phase === 'copying' || phase === 'stopping';
  const destinationId = queue?.destination.id ?? '';
  const ready = enabled && Boolean(queue?.copyAuthority.ready && queue.destination.available && destinationId) && !copying && !busy;

  async function previewSeparate(sourcePath: string, alternateName: string): Promise<ReviewedConflict> {
    const plan = record(await invoke('preview_copy_conflict', { request: { sourcePath, alternateName } }));
    const reviewed = snapshot(await invoke('copy_queue_snapshot'));
    if (reviewed.revision !== plan.revision || reviewed.destination.id !== plan.destinationId || !reviewed.destination.available) {
      throw new Error('The copy queue or destination changed. Review the conflict again.');
    }
    const { sourcePath: checkedSource, existingPath, alternatePath, alternateName: checkedName, conflictFingerprint } = plan;
    if (typeof checkedSource !== 'string' || typeof existingPath !== 'string' || typeof alternatePath !== 'string' ||
        typeof checkedName !== 'string' || typeof conflictFingerprint !== 'string' || !/^[a-f0-9]{64}$/.test(conflictFingerprint)) {
      throw new Error('The reviewed conflict is incomplete.');
    }
    return { sourcePath: checkedSource, existingPath, alternatePath, alternateName: checkedName,
      conflictFingerprint, queue: reviewed, operationId: crypto.randomUUID() };
  }

  async function startSeparate(reviewed: ReviewedConflict, retry: boolean): Promise<void> {
    if (request.current || !enabled) throw new Error('The copy controls are busy or unavailable.');
    request.current = true;
    setBusy(true);
    setIssue('');
    try {
      await onNativeWrite(async () => {
        const next = snapshot(await invoke('copy_queue_snapshot'));
        if (!next.copyAuthority.ready && !retry) throw new SeparateCopyRefusal('Review recorded copy effects before starting a separate copy.');
        if (!retry && !sameCopyReview(reviewed.queue, next)) throw new SeparateCopyRefusal('The reviewed copy queue changed. Review the conflict again.');
        const result = record(await invoke('control_copy_queue', { request: {
          ...fences(reviewed.queue), action: 'startSeparate', operationId: reviewed.operationId,
          sourcePath: reviewed.sourcePath, alternateName: reviewed.alternateName,
          expectedConflictFingerprint: reviewed.conflictFingerprint,
        } }));
        const receipt = record(result.receipt);
        if (receipt.phase === 'failed') throw new SeparateCopyRefusal(string(receipt.error) || 'The separate copy did not start.');
        if (receipt.phase !== 'accepted') throw new Error('The separate copy acceptance could not be confirmed.');
      });
    } catch (cause) {
      const observed = await invoke('copy_queue_snapshot').then(snapshot).catch(() => null);
      const receipt = observed?.latestOperation;
      if (receipt?.operationId !== reviewed.operationId || receipt.phase !== 'accepted') throw cause;
    } finally {
      request.current = false;
      setBusy(false);
      read.current();
    }
    setSeparateSource(null);
  }

  function fences(next: QueueSnapshot): Record<string, unknown> {
    return { expectedInstanceId: next.instanceId, expectedRevision: next.revision,
      expectedQueueSignature: next.queueSignature, expectedSourceVolumeSignature: next.sourceVolumeSignature,
      expectedDestinationId: next.destination.id, confirmed: true };
  }
  async function control(action: 'start' | 'pause' | 'resume', mode?: CopyMode, scope?: string): Promise<void> {
    if (request.current) throw new Error('Another copy control is pending.');
    if (!enabled) throw new Error('Recover the review data before copying.');
    request.current = true;
    setBusy(true);
    setIssue('');
    try {
      await onNativeWrite(async () => {
        const next = snapshot(await invoke<unknown>('copy_queue_snapshot'));
        if (action !== 'pause' && !next.copyAuthority.ready) throw new Error('Review recorded copy effects before starting another pass.');
        if (auto && (next.destination.id !== armedDestination.current || next.sourceVolumeSignature !== armedSources.current)) {
          throw new Error('Auto-copy stopped because the reviewed drives changed.');
        }
        const response = record(await invoke('control_copy_queue', { request: {
          ...fences(next), action, operationId: crypto.randomUUID(),
          ...(action === 'start' ? { mode, ...(scope ? { scope } : {}) } : { expectedJobId: next.status.jobId }),
        } }));
        if (record(response.receipt).phase === 'failed') throw new Error(string(record(response.receipt).error) || 'The copy run could not start. Its receipt was retained.');
      });
      read.current();
    } catch (error) {
      if (String(error).includes('The review changed.')) attempted.current = '';
      setIssue(String(error));
      armedDestination.current = ''; armedSources.current = ''; setAuto(false);
      read.current();
      throw error;
    } finally {
      request.current = false;
      setBusy(false);
    }
  }

  const queueSignature = queue?.queueSignature ?? '';
  const pendingCount = queue?.pendingMigrateNow ?? 0;
  const queueRevision = queue?.revision ?? 0;
  useEffect(() => {
    if (!auto || !ready || separateSource || !pendingCount || !queueSignature || destinationId !== armedDestination.current) return;
    const key = `${destinationId}:${queueSignature}`;
    if (attempted.current === key) return;
    const timer = setTimeout(() => {
      if (armedDestination.current !== destinationId || attempted.current === key) return;
      attempted.current = key;
      void control('start', 'ordered').catch(() => {});
    }, 1400);
    return () => clearTimeout(timer);
  }, [auto, ready, separateSource, pendingCount, queueSignature, destinationId, queueRevision]);

  async function toggleAuto(checked: boolean, reviewed?: UiRequest): Promise<void> {
    if (checked && !enabled) throw new Error('Recover the review data before enabling Auto-copy.');
    let next: QueueSnapshot | null = null;
    if (checked) {
      await onNativeWrite(async () => {
        next = snapshot(await invoke<unknown>('copy_queue_snapshot'));
        if (!next.copyAuthority.ready) throw new Error('Review recorded copy effects before enabling Auto-copy.');
        await invoke('validate_auto_copy', { request: reviewed ?? fences(next) });
        if (!next.destination.available) throw new Error(next.destination.problem || 'The copy destination is unavailable.');
        armedDestination.current = next.destination.id;
        armedSources.current = next.sourceVolumeSignature;
      });
    } else { armedDestination.current = ''; armedSources.current = ''; }
    attempted.current = '';
    setAuto(checked);
  }
  async function reconcileHistory(reviewed: QueueSnapshot, operationId: string): Promise<void> {
    if (request.current || !enabled) throw new Error('Copy control is busy or unavailable.');
    request.current = true;
    setBusy(true);
    try {
      await onNativeWrite(() => invoke('reconcile_copy_history', { request: {
        ...fences(reviewed), operationId, expectedLedgerDigest: reviewed.copyAuthority.ledgerDigest,
      } }));
      const next = snapshot(await invoke('copy_queue_snapshot'));
      setQueue(next);
      if (!next.copyAuthority.ready) throw new Error(next.copyAuthority.problem || 'Copy admission still requires review.');
      setIssue('');
      setAuthorityReview(null);
    } finally {
      request.current = false;
      setBusy(false);
      read.current();
    }
  }
  controlRef.current = {
    read: () => ({ autoCopy: auto, detailsOpen: details, busy, issue, queue,
      authorityReviewOpen: authorityReview !== null,
      separateReview: separateSource ? { sourcePath: separateSource.slice(0, 2048), pathTruncated: separateSource.length > 2048 } : null }),
    execute: async (command, reviewed) => {
      if (command.action === 'setAutoCopy') await toggleAuto(command.enabled === true, reviewed);
      else if (command.action === 'setCopyDetails') {
        setDetails(command.open === true);
        if (!command.open && !request.current) { setSeparateSource(null); setAuthorityReview(null); }
      }
      else throw new Error('Unsupported copy presentation action');
    },
  };
  useEffect(() => () => { controlRef.current = null; }, [controlRef]);

  if (!queue && !issue) return null;
  const status = queue?.status;
  const marked = queue?.migrateNowCount ?? 0;
  const authority = queue?.copyAuthority;
  const firstIssue = status?.issues.find(item => item.problem);
  const copyProblem = issue || status?.problem || firstIssue?.problem;
  return <section className="copy-queue" aria-label="Migration copy queue" onKeyDown={event => {
    if (details && !separateSource && !authorityReview && event.key === 'Escape') {
      event.preventDefault();
      event.stopPropagation();
      setDetails(false);
      countRef.current?.focus();
    }
  }}>
    <div className="copy-queue-toolbar">
      <button type="button" ref={countRef} className="copy-queue-count" aria-expanded={details} onClick={() => setDetails(!details)}>
        <Copy size={16}/><strong>{marked.toLocaleString()} entries marked for migration now</strong>
      </button>
      <div className="copy-queue-actions">
        <button type="button" disabled={!ready || !marked} onClick={() => void control('start', 'ordered').catch(() => {})}><ListOrdered size={15}/>Start Copying In Order</button>
        <button type="button" disabled={!ready || !marked} onClick={() => void control('start', 'all').catch(() => {})}><Copy size={15}/>Copy everything</button>
        <label className="copy-queue-auto" title="Copy new Migrate now marks after a short pause. Applies for this window session.">
          <input type="checkbox" checked={auto} disabled={!enabled || !queue?.destination.available || !authority?.ready} onChange={event => { void toggleAuto(event.target.checked).catch(error => setIssue(String(error))); }}/>Auto-copy
        </label>
        {copying && <button type="button" onClick={() => { void toggleAuto(false); void control('pause').catch(() => {}); }}><Pause size={15}/>Pause</button>}
      </div>
    </div>
    {authority && !authority.ready && <div className="copy-authority-notice">
      <TriangleAlert size={15} aria-hidden="true"/>
      <div role="status"><strong>{authority.phase === 'review-required' ? 'Copy history needs review' : 'New copy passes unavailable'}</strong>
        {authority.problem && <p>{authority.problem}</p>}
      </div>
      {authority.phase === 'review-required' && <button type="button" aria-haspopup="dialog"
        disabled={!enabled || copying || busy || !queue?.destination.available}
        onClick={() => setAuthorityReview(queue)}>Review copy effects</button>}
    </div>}
    {copyProblem && <div className="copy-queue-problem" role="status"><TriangleAlert size={15}/><span>{copyProblem}</span>
      {firstIssue && isDifferentFile(firstIssue.problem) && <button type="button" aria-haspopup="dialog" disabled={!ready}
        onClick={() => setSeparateSource(firstIssue.path)}>Review separate copy</button>}
    </div>}
    {phase === 'paused' && !copyProblem && <p className="copy-queue-status" role="status">Copy paused. You can start another pass or close the window.</p>}
    {copying && <p className="copy-queue-status" role="status">{phase === 'planning' ? 'Preparing the copy queue…' : `${status?.copied ?? 0} copied · ${status?.skipped ?? 0} already present · ${status?.failed ?? 0} issues`}{status?.currentPath && <span title={status.currentPath}>{status.currentPath}</span>}</p>}
    {details && <div className="copy-queue-popover">
      <div className="copy-queue-destination">
        <strong>{queue?.destination.available ? `Destination ${queue.destination.root}` : 'Destination unavailable'}</strong>
        <p>{queue?.destination.problem || 'Copies retain the path below the source drive. Existing files with different contents are left in place.'}</p>
        <p>A copy run uses the marks saved when it starts. Auto-copy starts off each time the window opens, and Pause turns it off.</p>
        {status && phase !== 'idle' && <p>{status.copied} copied · {status.skipped} already present · {status.failed} issues</p>}
      </div>
      <div className="copy-queue-list-frame">
        <div ref={listRef} className="copy-queue-list" tabIndex={0} aria-label="Marked copy entries">
          <h3>Migrate now</h3>
          {queue?.migrateNow.map(item => <div className="copy-queue-item" key={item.path}><span title={item.path}>{item.path}</span><small>{item.state}</small></div>)}
          {!marked && <p>Mark an entry Migrate now to add it here.</p>}
          {queue && marked > queue.migrateNow.length && <p>Showing {queue.migrateNow.length} of {marked.toLocaleString()} marked entries.</p>}
          <h3>Migrate Last Moment</h3>
          {queue?.lastMoment.map(item => <div className="copy-queue-item" key={item.path}><span title={item.path}>{item.path}</span><button type="button" disabled={!ready} onClick={() => void control('start', 'last-moment', item.path).catch(() => {})}>Copy last moment</button></div>)}
          {!queue?.lastMomentCount && <p>No last-moment entries.</p>}
          {Boolean(status?.issues.length) && <><h3>Copy issues</h3>{status?.issues.map((item, index) =>
            <div className="copy-issue" key={`${item.path}:${index}`}><p>{item.path} · {item.problem}</p>
              {isDifferentFile(item.problem) && <button type="button" aria-haspopup="dialog" disabled={!ready}
                onClick={() => setSeparateSource(item.path)}>Review separate copy</button>}</div>
          )}</>}
        </div>
        <OverflowIndicators targetRef={listRef} axis="vertical" refreshKey={queue}/>
      </div>
    </div>}
    {separateSource && <SeparateCopyReview key={separateSource} sourcePath={separateSource} currentQueue={queue}
      enabled={enabled && !copying && authority?.ready === true} onPreview={previewSeparate} onApply={startSeparate} onClose={() => setSeparateSource(null)}/>}
    {authorityReview && <CopyAuthorityReview key={`${authorityReview.copyAuthority.ledgerDigest}:${authorityReview.revision}:${authorityReview.destination.id}:${authorityReview.sourceVolumeSignature}:${authorityReview.queueSignature}`}
      reviewed={authorityReview} current={queue} enabled={enabled && !copying} onReconcile={reconcileHistory}
      onRefresh={() => setAuthorityReview(queue?.copyAuthority.phase === 'review-required' ? queue : null)}
      onClose={() => setAuthorityReview(null)} fallbackFocus={() => countRef.current?.focus()}/>}
  </section>;
}

function CopyAuthorityReview({ reviewed, current, enabled, onReconcile, onRefresh, onClose, fallbackFocus }: {
  reviewed: QueueSnapshot; current: QueueSnapshot | null; enabled: boolean;
  onReconcile: (reviewed: QueueSnapshot, operationId: string) => Promise<void>; onRefresh: () => void;
  onClose: () => void; fallbackFocus: () => void;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const scroll = useRef<HTMLDivElement>(null);
  const operationId = useRef(crypto.randomUUID());
  const request = useRef(false);
  const [offsets, setOffsets] = useState([0]);
  const [page, setPage] = useState<{ entries: CopyEffect[]; total: number; nextOffset: number | null } | null>(null);
  const [seen, setSeen] = useState<ReadonlyMap<number, number>>(() => new Map());
  const [acknowledged, setAcknowledged] = useState(false);
  const [applying, setApplying] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const offset = offsets.at(-1) ?? 0;
  const authority = reviewed.copyAuthority;
  const unchanged = current && current.copyAuthority.ledgerDigest === authority.ledgerDigest && sameCopyReview(reviewed, current);
  const seenCount = [...seen.values()].reduce((total, size) => total + size, 0);
  const allSeen = page && page.total === seenCount;

  useEffect(() => {
    const surface = dialog.current;
    const invoker = document.activeElement;
    surface?.showModal();
    return () => {
      surface?.close();
      if (invoker instanceof HTMLElement && invoker.isConnected && !invoker.matches(':disabled')) invoker.focus();
      else fallbackFocus();
    };
  }, []);
  useEffect(() => {
    let active = true;
    setPage(null);
    setProblem(null);
    void invoke('read_copy_effect_history', { request: {
      offset, limit: 16, expectedLedgerDigest: authority.ledgerDigest,
    } }).then(value => {
      const result = record(value);
      if (result.ledgerDigest !== authority.ledgerDigest || result.total !== authority.unreconciledCount ||
          !Array.isArray(result.entries) || (result.nextOffset !== null && typeof result.nextOffset !== 'number')) {
        throw new Error('Recorded copy effects changed. Review the current history.');
      }
      const entries = result.entries.map(copyEffect);
      const consumed = offset + entries.length;
      const nextOffset = consumed < authority.unreconciledCount ? consumed : null;
      if (consumed > authority.unreconciledCount || result.nextOffset !== nextOffset || (nextOffset !== null && !entries.length)) {
        throw new Error('The recorded effects page is incomplete. Review the current history.');
      }
      if (active) {
        setPage({ entries, total: authority.unreconciledCount, nextOffset });
        setSeen(previous => new Map(previous).set(offset, entries.length));
      }
    }).catch(cause => { if (active) setProblem(String(cause)); });
    return () => { active = false; };
  }, [offset, authority.ledgerDigest, authority.unreconciledCount]);

  function apply(): void {
    if (request.current || !enabled || !unchanged || !allSeen || !acknowledged) return;
    request.current = true;
    setApplying(true);
    setProblem(null);
    void onReconcile(reviewed, operationId.current).catch(cause => setProblem(String(cause))).finally(() => {
      request.current = false;
      setApplying(false);
    });
  }

  return <dialog ref={dialog} className="modal copy-authority-review" aria-label="Review recorded copy effects"
    onCancel={event => { event.preventDefault(); if (!applying) onClose(); }}>
    <div className="modal-header"><h2>Review recorded copy effects</h2>
      <button type="button" className="icon-button" aria-label="Close copy effects review" disabled={applying}
        onClick={onClose}><X size={18} aria-hidden="true"/></button>
    </div>
    <div className="copy-authority-frame"><div ref={scroll} className="modal-body">
      <p>Review the recorded copy requests and the current destination. The recorded phases describe requests; inspect destination files separately for completed bytes.</p>
      <dl className="copy-authority-current">
        <div><dt>Current destination</dt><dd>{reviewed.destination.root}</dd></div>
        <div><dt>Destination identity</dt><dd>{reviewed.destination.id}</dd></div>
        <div><dt>Queue</dt><dd>{reviewed.migrateNowCount.toLocaleString()} Migrate now · {reviewed.lastMomentCount.toLocaleString()} Last Moment</dd></div>
        <div><dt>Ledger</dt><dd>{authority.historyCount.toLocaleString()} recorded effects · {authority.unreconciledCount.toLocaleString()} need review</dd></div>
      </dl>
      {page && <ol className="copy-authority-effects" aria-label="Recorded copy effects">
        {page.entries.map(effect => <li key={effect.operationId}>
          <strong>{effect.action || 'Copy request'} · {effect.phase || 'Unknown phase'}</strong>
          <p>Operation {effect.operationId}{effect.jobId && ` · Job ${effect.jobId}`}</p>
          <p>Destination {effect.destinationId || 'Unknown identity'}
            {effect.requestedAt && ` · Requested ${effect.requestedAt}`}
            {effect.committedRevision !== null && ` · Revision ${effect.committedRevision}`}</p>
          {effect.destinationId && effect.destinationId !== reviewed.destination.id &&
            <p className="copy-authority-error">This effect used a different destination identity.</p>}
        </li>)}
        {!page.entries.length && <li>No recorded effects on this page.</li>}
      </ol>}
      {problem && <p className="copy-authority-error" role="alert">{problem}</p>}
      {!unchanged && <p role="status">The reviewed queue, drives or ledger changed. Read the current history again.</p>}
    </div><OverflowIndicators targetRef={scroll} axis="vertical" refreshKey={page}/></div>
    <div className="copy-authority-actions">
      <div className="copy-authority-pages">
        <span role="status">{seenCount.toLocaleString()} of {authority.unreconciledCount.toLocaleString()} effects needing review displayed</span>
        <button type="button" disabled={offsets.length === 1 || applying} onClick={() => setOffsets(previous => previous.slice(0, -1))}>Previous effects</button>
        <button type="button" disabled={page?.nextOffset == null || applying} onClick={() => {
          const next = page?.nextOffset;
          if (next != null) setOffsets(previous => [...previous, next]);
        }}>Next effects</button>
      </div>
      <label><input type="checkbox" disabled={!allSeen || !unchanged || applying} checked={acknowledged}
        onChange={event => setAcknowledged(event.target.checked)}/>
        I reviewed the recorded effects, current destination and queue.
      </label>
      <div className="copy-authority-submit">
        {!unchanged && <button type="button" disabled={applying} onClick={onRefresh}>Review current history</button>}
        <button type="button" className="primary" disabled={!enabled || !allSeen || !unchanged || !acknowledged || applying}
          onClick={apply}>{applying ? 'Saving reconciliation…' : 'Confirm copy history reconciliation'}</button>
      </div>
      <p>Reconciliation records your review. A new copy pass requires its own Copy command.</p>
    </div>
  </dialog>;
}

function SeparateCopyReview({ sourcePath, currentQueue, enabled, onPreview, onApply, onClose }: {
  sourcePath: string; currentQueue: QueueSnapshot | null; enabled: boolean;
  onPreview: (path: string, name: string) => Promise<ReviewedConflict>;
  onApply: (reviewed: ReviewedConflict, retry: boolean) => Promise<void>;
  onClose: () => void;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const nameInput = useRef<HTMLInputElement>(null);
  const request = useRef(false);
  const [name, setName] = useState('');
  const [review, setReview] = useState<ReviewedConflict | null>(null);
  const [attempted, setAttempted] = useState(false);
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState('');
  useEffect(() => {
    const surface = dialog.current;
    const invoker = document.activeElement;
    surface?.showModal();
    nameInput.current?.focus();
    return () => {
      surface?.close();
      if (invoker instanceof HTMLElement && invoker.isConnected) invoker.focus();
    };
  }, []);
  const current = review && currentQueue && sameCopyReview(review.queue, currentQueue);
  function run(action: () => Promise<void>): void {
    if (request.current) return;
    request.current = true;
    setBusy(true);
    setProblem('');
    void action().catch(cause => setProblem(String(cause))).finally(() => { request.current = false; setBusy(false); });
  }
  function preview(): void {
    run(async () => {
      setReview(null);
      setReview(await onPreview(sourcePath, name));
    });
  }
  function apply(): void {
    if (!review || !enabled || !attempted && !current) return;
    run(async () => {
      setAttempted(true);
      try { await onApply(review, attempted); }
      catch (cause) {
        if (cause instanceof SeparateCopyRefusal ||
            /review changed|review the conflict again|alternate filename is already present|alternate file already exists|different file.*left intact/i.test(String(cause))) {
          setAttempted(false);
        }
        throw cause;
      }
    });
  }
  return <dialog ref={dialog} className="modal compact-modal separate-copy-review" aria-label="Review separate copy"
    onCancel={event => { event.preventDefault(); if (!busy) onClose(); }}>
    <div className="modal-header"><h2>Review separate copy</h2>
      <button type="button" className="icon-button" aria-label="Close separate copy review" disabled={busy} onClick={onClose}><X size={18} aria-hidden="true"/></button>
    </div>
    <div className="modal-body">
      <p>Choose a new filename for one eligible source file. The original destination file stays intact.</p>
      <p className="separate-copy-path">Source · {sourcePath}</p>
      <label>Alternate filename<input ref={nameInput} value={name} disabled={busy || attempted} onChange={event => {
        setName(event.target.value); setReview(null); setProblem('');
      }}/></label>
      {review && <dl><div><dt>Existing destination</dt><dd>{review.existingPath}</dd></div>
        <div><dt>New destination</dt><dd>{review.alternatePath}</dd></div></dl>}
      {review && !current && !attempted && <p>The copy queue changed. Review the current conflict again.</p>}
      {problem && <p role="alert">{problem}</p>}
    </div>
    <div className="separate-copy-actions">
      <button type="button" disabled={!enabled || busy || attempted || !name.trim()} onClick={preview}>Review exact destinations</button>
      {review && <button type="button" className="primary" disabled={!enabled || busy || !attempted && !current}
        onClick={apply}>{attempted ? 'Retry the same request' : 'Copy to reviewed alternate name'}</button>}
    </div>
  </dialog>;
}
