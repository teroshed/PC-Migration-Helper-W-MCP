import { useEffect, useRef, useState } from 'react';
import type { ReactNode } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { CircleHelp, Hash, Pause, Play, TriangleAlert } from 'lucide-react';
import { allocatedState, auditProgress, auditRefreshKey, parseAuditStatus } from './audit-model';
import type { AuditStats, AuditStatus } from './audit-model';
import { readableBytes } from './model';
import { OverflowIndicators } from './OverflowIndicators';

export function allocatedLabel(stats: AuditStats | undefined): string {
  const allocation = allocatedState(stats);
  if (allocation.kind === 'unknown' || allocation.bytes === null) return 'unknown';
  return allocation.kind === 'partial' ? `≥ ${readableBytes(allocation.bytes)} · partial` : readableBytes(allocation.bytes);
}
export function auditDescription(stats: AuditStats | undefined, logicalFallback?: number): string {
  const logical = stats?.logicalBytes ?? logicalFallback;
  return [
    `Size on disk: ${allocatedLabel(stats)}`,
    logical === undefined ? 'Logical size not yet counted' : `Logical size: ${readableBytes(logical)}`,
    auditProgress(stats).label,
    stats ? `SHA-256: ${stats.hashedLeaves.toLocaleString()} hashed; ${stats.failedLeaves.toLocaleString()} failed${stats.sha256 ? `\nLast recorded checksum: ${stats.sha256}` : ''}${stats.verifiedAt ? `\nRecorded: ${stats.verifiedAt}` : ''}${stats.hashState ? `\nChecksum state: ${stats.hashState}` : ''}` : 'SHA-256 not yet checked',
    stats?.allocationUnknown ? `${stats.allocationUnknown.toLocaleString()} leaf allocations unavailable` : null,
    stats?.problem,
    stats?.scopeNotes,
  ].filter(Boolean).join('\n');
}
export function AuditProgress({ stats, mode = 'count', children, size = 27 }: { stats?: AuditStats; mode?: 'count' | 'hash'; children?: ReactNode; size?: number }) {
  const progress = auditProgress(stats, mode);
  const activeUnknown = progress.fraction === null && (stats?.phase === 'discovering' || stats?.phase === 'hashing');
  return <span className={`audit-progress ${progress.fraction === null ? 'audit-progress-unknown' : ''} ${activeUnknown ? 'audit-progress-active' : ''} ${stats?.problem || stats?.failedLeaves || stats?.coverageGaps ? 'audit-progress-gap' : ''}`} style={{ width: size, height: size }} role="progressbar" aria-label={mode === 'hash' ? 'Source SHA-256 progress' : 'Leaf count progress'} aria-valuemin={progress.total === null ? undefined : 0} aria-valuemax={progress.total === null || progress.total === 0 ? undefined : progress.total} aria-valuenow={progress.total === null || progress.total === 0 ? undefined : Math.min(progress.processed, progress.total)} aria-valuetext={progress.label} title={progress.label}>
    <svg viewBox="0 0 32 32" aria-hidden="true"><circle className="audit-progress-track" cx="16" cy="16" r="13"/><circle className="audit-progress-value" cx="16" cy="16" r="13" pathLength="100" strokeDasharray={progress.fraction === null ? '4 6' : `${progress.fraction * 100} 100`}/></svg><span className="audit-progress-center" aria-hidden="true">{children ?? <Hash size={Math.max(12, size * .45)}/>}</span>
  </span>;
}

export function useAuditStatus(enabled: boolean, onStatsChanged: () => boolean): { status: AuditStatus | null; problem: string | null; busy: boolean; control: (action: 'start' | 'pause' | 'resume', scope: 'all' | 'here', path: string) => Promise<void>; refresh: () => Promise<void> } {
  const [status, setStatus] = useState<AuditStatus | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const changed = useRef(onStatsChanged);
  changed.current = onStatsChanged;
  const refreshRef = useRef<() => Promise<void>>(async () => {});
  useEffect(() => {
    if (!enabled) return;
    let active = true;
    let inFlight = false;
    let previousKey: string | null = null;
    let pendingRefresh = false;
    async function refresh(): Promise<void> {
      if (inFlight) return;
      inFlight = true;
      try {
        const next = parseAuditStatus(await invoke<unknown>('audit_status'));
        if (!active) return;
        const key = auditRefreshKey(next);
        if (previousKey !== null && key !== previousKey) pendingRefresh = true;
        previousKey = key;
        setStatus(previous => previous && JSON.stringify(previous) === JSON.stringify(next) ? previous : next);
        setProblem(null);
        if (pendingRefresh && changed.current()) pendingRefresh = false;
      } catch (cause) {
        if (active) setProblem(`Counting and SHA-256 are unavailable in this app session: ${String(cause)}`);
      } finally { inFlight = false; }
    }
    refreshRef.current = refresh;
    void refresh();
    const timer = setInterval(() => { void refresh(); }, 2000);
    return () => { active = false; clearInterval(timer); refreshRef.current = async () => {}; };
  }, [enabled]);
  async function control(action: 'start' | 'pause' | 'resume', scope: 'all' | 'here', path: string): Promise<void> {
    if (busy) return;
    setBusy(true);
    try {
      await invoke('audit_control', { action, scope, path });
      await refreshRef.current();
    } catch (cause) { setProblem(String(cause)); }
    finally { setBusy(false); }
  }
  return { status, problem, busy, control, refresh: () => refreshRef.current() };
}

export function AuditControls({ status, problem, busy, path, onControl, onDetails }: { status: AuditStatus | null; problem: string | null; busy: boolean; path: string; onControl: (action: 'start' | 'pause' | 'resume', scope: 'all' | 'here', path: string) => Promise<void>; onDetails: () => void }) {
  const active = status?.phase === 'discovering' || status?.phase === 'hashing';
  const progress = auditProgress(status ?? undefined, status?.totalKnown ? 'hash' : 'count');
  const label = !status ? 'Count & hash not available' : status.phase === 'idle' ? 'Ready for a source audit' : status.phase === 'discovering' ? `${status.discoveredLeaves.toLocaleString()} leaves found · total unknown` : status.phase === 'hashing' ? `${status.hashedLeaves.toLocaleString()} / ${status.totalKnown ? status.discoveredLeaves.toLocaleString() : '?'} SHA-256` : status.phase === 'paused' ? status.reconciliationRequired ? 'Paused · fresh count needed' : `Paused · ${progress.processed.toLocaleString()} processed` : status.phase === 'complete' ? `${status.hashedLeaves.toLocaleString()} hashed${status.failedLeaves ? ` · ${status.failedLeaves.toLocaleString()} failed` : status.coverageGaps ? ' · coverage gaps' : ''}` : 'Audit stopped · see details';
  return <div className="audit-controls"><div className="audit-controls-heading"><AuditProgress stats={status ?? undefined} mode={status?.totalKnown ? 'hash' : 'count'} size={25}/><div><strong>Count & hash</strong><span>Source SHA-256</span></div></div><p className="audit-controls-status" title={status?.currentPath ?? progress.label}>{busy ? 'Updating audit…' : label}</p><p className="audit-controls-size" title={status ? auditDescription(status) : 'Start an audit to count leaf files and allocated sizes.'}>Disk {allocatedLabel(status ?? undefined)}</p><div className="audit-controls-actions">{active ? <button disabled={busy} onClick={() => void onControl('pause', 'all', path)}><Pause size={13}/>Pause</button> : status?.phase === 'paused' || status?.phase === 'error' ? <><button disabled={busy} onClick={() => void onControl('resume', 'all', path)}><Play size={13}/>Resume</button>{(status.reconciliationRequired || status.phase === 'error') && <button disabled={busy} title="Start a fresh source count and SHA-256 audit across all review roots" onClick={() => void onControl('start', 'all', path)}>Fresh all</button>}</> : <><button disabled={!status || busy || Boolean(problem)} aria-label="Count size on disk and SHA-256 all review roots" title="Count allocated size and SHA-256 every reachable file in all review roots" onClick={() => void onControl('start', 'all', path)}><Play size={13}/>All roots</button><button disabled={!status || busy || Boolean(problem)} aria-label="Count size on disk and SHA-256 this subtree" title="Count allocated size and SHA-256 every reachable file in this subtree" onClick={() => void onControl('start', 'here', path)}>Here</button></>}</div><button className={`audit-details-button ${problem || status?.problem || status?.coverageGaps || status?.reconciliationRequired ? 'has-gaps' : ''}`} onClick={onDetails}>{problem || status?.problem || status?.coverageGaps || status?.reconciliationRequired ? <TriangleAlert size={13}/> : <CircleHelp size={13}/>}<span>{problem || status?.problem ? 'Audit issue · see details' : status?.reconciliationRequired ? 'Fresh audit needed' : status?.coverageGaps ? `${status.coverageGaps.toLocaleString()} coverage gaps` : 'Coverage & audit details'}</span></button></div>;
}

export function AuditDetails({ stats, logicalFallback }: { stats?: AuditStats; logicalFallback?: number }) {
  const count = auditProgress(stats);
  return <section className="audit-detail-card"><h3>Source size & checksums</h3><dl><div><dt>Size on disk</dt><dd>{allocatedLabel(stats)}</dd></div><div><dt>Logical bytes</dt><dd>{stats ? readableBytes(stats.logicalBytes) : logicalFallback === undefined ? 'Not counted' : readableBytes(logicalFallback)}</dd></div><div><dt>Leaves counted</dt><dd>{stats ? `${stats.countedLeaves.toLocaleString()} / ${stats.totalKnown ? stats.discoveredLeaves.toLocaleString() : 'total unknown'}` : 'Not counted'}</dd></div><div><dt>SHA-256 checked</dt><dd>{stats ? `${stats.hashedLeaves.toLocaleString()}${stats.totalKnown ? ` / ${stats.discoveredLeaves.toLocaleString()}` : ''}` : 'Not checked'}</dd></div>{Boolean(stats?.failedLeaves) && <div><dt>Failed leaves</dt><dd>{stats?.failedLeaves.toLocaleString()}</dd></div>}{Boolean(stats?.allocationUnknown) && <div><dt>Unknown allocation</dt><dd>{stats?.allocationUnknown.toLocaleString()} leaves</dd></div>}</dl>{stats?.sha256 && <label className="audit-checksum">Last recorded source SHA-256<input readOnly value={stats.sha256} aria-label="Last recorded source SHA-256 checksum"/>{stats.verifiedAt && <small>Recorded {new Date(stats.verifiedAt).toLocaleString()}</small>}{stats.hashState && <small>Checksum state: {stats.hashState}</small>}{stats.cached && <small>Recorded earlier; current contents are not yet confirmed by a new checksum.</small>}</label>}{stats?.problem && <p className="audit-detail-problem">{stats.problem}</p>}{stats?.scopeNotes && <p className="audit-scope-note">{scopeExplanation(stats.scopeNotes)}</p>}<p>{count.label} Source checksums do not compare or certify an external backup. Reported sizes remain partial when coverage or allocation is unknown.</p></section>;
}

function scopeExplanation(note: string): string {
  return note.replace('the unnamed file data stream only', 'the main contents of each file only').replace('Alternate data streams', 'Additional named file streams').replace('ACLs', 'access permissions').replace('directory allocation', 'the space used by folders themselves').replace('restore usability', 'restoration testing').replace('application-consistent snapshots', 'consistent snapshots of running applications').replace('Allocated bytes count each directory entry; hardlinks are not deduplicated.', 'Size totals count each directory entry, so hard links can count the same stored data more than once.');
}

function HashCheckpoints({ status }: { status: AuditStatus }) {
  return <section className="audit-detail-card"><h3>Saved hash checkpoints</h3>
    <dl><div><dt>Completed files saved</dt><dd>{status.savedHashFiles.toLocaleString()}</dd></div>
      <div><dt>Queued file hashes</dt><dd>{status.pendingHashFiles.toLocaleString()}</dd></div>
      <div><dt>Currently hashing</dt><dd>{status.activeHashFiles.toLocaleString()}</dd></div></dl>
    <p>Completed SHA-256 results are saved locally. Resume continues unfinished files; a file interrupted mid-hash restarts from its beginning.</p>
    <p>Saved hashes retain their verification time. A fresh comparison is still needed after an unobserved change interval. Source hashes alone do not verify a backup.</p>
  </section>;
}

export function AuditCoverageDetails({ status }: { status: AuditStatus | null }) {
  const gapsRef = useRef<HTMLDivElement>(null);
  const exclusionsRef = useRef<HTMLDivElement>(null);
  if (!status) return null;
  return <section className="audit-coverage-details"><HashCheckpoints status={status}/>{status.reconciliationRequired && <div className="audit-reconciliation"><TriangleAlert size={17}/><p>Changes may have been missed while the app was closed or monitoring was interrupted. Resume can finish queued work, but a fresh count and SHA-256 pass is needed to recheck folders already visited. Start a fresh pass from Count & hash.</p></div>}{status.gapSamples.length > 0 && <><h3>Source audit gaps · {status.coverageGaps.toLocaleString()} recorded</h3><p>Showing up to 50 sampled paths from the current audit. Unknown source content is not treated as counted or verified.</p><div className="audit-sample-frame"><div ref={gapsRef} className="audit-gap-samples">{status.gapSamples.map((gap, index) => <article key={`${gap.path}:${index}`}><strong>{gap.path || 'Whole audit scope'}</strong><p>{gap.problem}</p><small>{gap.kind.replaceAll('_', ' ')}{gap.observedAt ? ` · ${new Date(gap.observedAt).toLocaleString()}` : ''}</small></article>)}</div><OverflowIndicators targetRef={gapsRef} axis="vertical" refreshKey={status.gapSamples}/></div></>}{status.exclusions.length > 0 && <><h3>App files intentionally omitted</h3><p>The app omits its own active journal and working folders from this source audit to avoid reacting to its own writes. These are intentional exclusions; protect the decision journal separately in your eventual external backup.</p><div className="audit-sample-frame"><div ref={exclusionsRef} className="audit-exclusion-list">{status.exclusions.map(path => <p key={path}>{path}</p>)}</div><OverflowIndicators targetRef={exclusionsRef} axis="vertical" refreshKey={status.exclusions}/></div></>}</section>;
}
