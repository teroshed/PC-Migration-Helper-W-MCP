import { useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { Copy, ListOrdered, Pause, TriangleAlert } from 'lucide-react';
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
  status: QueueStatus;
}

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

function snapshot(value: unknown): QueueSnapshot {
  const queue = record(value);
  const destination = record(queue.destination);
  const status = record(queue.status);
  const migrateNow = items(queue.migrateNow);
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

export function CopyQueue({ revision, enabled, controlRef, onNativeWrite }: {
  revision: number; enabled: boolean; controlRef: UiControlRef;
  onNativeWrite: <T>(operation: () => Promise<T>) => Promise<T>;
}) {
  const [queue, setQueue] = useState<QueueSnapshot | null>(null);
  const [auto, setAuto] = useState(false);
  const [details, setDetails] = useState(false);
  const [busy, setBusy] = useState(false);
  const [issue, setIssue] = useState('');
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
        if (armedDestination.current && (!next.destination.available || next.destination.id !== armedDestination.current || next.sourceVolumeSignature !== armedSources.current)) {
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
  const ready = enabled && Boolean(queue?.destination.available && destinationId) && !copying && !busy;

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
    if (!auto || !ready || !pendingCount || !queueSignature || destinationId !== armedDestination.current) return;
    const key = `${destinationId}:${queueSignature}`;
    if (attempted.current === key) return;
    const timer = setTimeout(() => {
      if (armedDestination.current !== destinationId || attempted.current === key) return;
      attempted.current = key;
      void control('start', 'ordered').catch(() => {});
    }, 1400);
    return () => clearTimeout(timer);
  }, [auto, ready, pendingCount, queueSignature, destinationId, queueRevision]);

  async function toggleAuto(checked: boolean, reviewed?: UiRequest): Promise<void> {
    if (checked && !enabled) throw new Error('Recover the review data before enabling Auto-copy.');
    let next: QueueSnapshot | null = null;
    if (checked) {
      await onNativeWrite(async () => {
        next = snapshot(await invoke<unknown>('copy_queue_snapshot'));
        await invoke('validate_auto_copy', { request: reviewed ?? fences(next) });
        if (!next.destination.available) throw new Error(next.destination.problem || 'The copy destination is unavailable.');
        armedDestination.current = next.destination.id;
        armedSources.current = next.sourceVolumeSignature;
      });
    } else { armedDestination.current = ''; armedSources.current = ''; }
    attempted.current = '';
    setAuto(checked);
  }
  controlRef.current = {
    read: () => ({ autoCopy: auto, detailsOpen: details, busy, issue, queue }),
    execute: async (command, reviewed) => {
      if (command.action === 'setAutoCopy') await toggleAuto(command.enabled === true, reviewed);
      else if (command.action === 'setCopyDetails') setDetails(command.open === true);
      else throw new Error('Unsupported copy presentation action');
    },
  };
  useEffect(() => () => { controlRef.current = null; }, [controlRef]);

  if (!queue && !issue) return null;
  const status = queue?.status;
  const marked = queue?.migrateNowCount ?? 0;
  const firstIssue = status?.issues.find(item => item.problem);
  const copyProblem = issue || status?.problem || firstIssue?.problem;
  return <section className="copy-queue" aria-label="Migration copy queue" onKeyDown={event => {
    if (details && event.key === 'Escape') {
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
          <input type="checkbox" checked={auto} disabled={!enabled || !queue?.destination.available} onChange={event => { void toggleAuto(event.target.checked).catch(error => setIssue(String(error))); }}/>Auto-copy
        </label>
        {copying && <button type="button" onClick={() => { void toggleAuto(false); void control('pause').catch(() => {}); }}><Pause size={15}/>Pause</button>}
      </div>
    </div>
    {copyProblem && <p className="copy-queue-problem" role="status"><TriangleAlert size={15}/>{copyProblem}</p>}
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
            <p key={`${item.path}:${index}`}>{item.path} · {item.problem}</p>
          )}</>}
        </div>
        <OverflowIndicators targetRef={listRef} axis="vertical" refreshKey={queue}/>
      </div>
    </div>}
  </section>;
}
