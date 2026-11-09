import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { ChevronLeft, ChevronRight, Folder, ShieldCheck, TriangleAlert } from 'lucide-react';
import { readableBytes } from './model';
import type { Operation } from './model';
import type { UiControlRef } from './UIBridge';
import {
  currentPreparedMirror, parseMirrorBinding, parseMirrorPage, parseMirrorStatus, parsePreparedMirror, parseSavedMirrorExceptions,
} from './mirror-model';
import type { MirrorBinding, MirrorCounts, MirrorPage, MirrorStatus, PreparedMirror } from './mirror-model';
import './mirror-review.css';

type MirrorMutation = Extract<Operation, { type: 'mirrorBind' | 'mirrorConfirm' }>;
interface Props {
  currentPath: string;
  instanceId: string | undefined;
  enabled: boolean;
  controlRef: UiControlRef;
  onNativeWrite: <T>(operation: () => Promise<T>) => Promise<T>;
  onSave: (operation: MirrorMutation, revision: number, operationId: string) => Promise<unknown>;
}
const MIRROR_DRAFT = 'migration-master.mirror-directory-draft';
const PAGE_LIMIT = 16;
const EXCEPTION_LIMIT = 1000;
const COUNT_LABELS: Record<keyof MirrorCounts, string> = {
  equal: 'Matching files', directories: 'Directories', missing: 'Missing', different: 'Different', extra: 'Mirror-only',
  changedDuringRead: 'Changed while reading', unreadable: 'Unreadable', unsupported: 'Unsupported', coverageGaps: 'Coverage gaps',
};
const VERDICTS: Record<string, string> = { equal: 'Matches', directory: 'Directory', missing: 'Missing from mirror',
  different: 'Different checksum', extra: 'Mirror-only', 'changed-during-read': 'Changed while reading',
  unreadable: 'Unreadable', unsupported: 'Unsupported' };

function rememberMirror(value: string): void {
  try { localStorage.setItem(MIRROR_DRAFT, value); } catch { /* Keep the current draft available. */ }
}
function readMirrorDraft(): string {
  try { return localStorage.getItem(MIRROR_DRAFT) ?? ''; } catch { return ''; }
}
const request = (method: string, payload: Record<string, unknown> = {}) => invoke<unknown>(method, { request: payload });
function dateLabel(value: string): string {
  const date = new Date(value);
  return Number.isFinite(date.valueOf()) ? date.toLocaleString() : value;
}

export function MirrorReview({ currentPath, instanceId, enabled, controlRef, onNativeWrite, onSave }: Props) {
  const [tab, setTab] = useState<'binding' | 'comparison' | 'confirmation'>('binding');
  const [source, setSource] = useState(currentPath);
  const [mirror, setMirror] = useState(readMirrorDraft);
  const [snapshot, setSnapshot] = useState<MirrorStatus | null>(null);
  const [activeBindingId, setActiveBindingId] = useState<string | null>(null);
  const [preview, setPreview] = useState<{ binding: MirrorBinding; revision: number; operationId: string } | null>(null);
  const [prepared, setPrepared] = useState<{ result: PreparedMirror; expiresAt: number; operationId: string } | null>(null);
  const [exceptions, setExceptions] = useState<readonly string[]>([]);
  const [evidence, setEvidence] = useState<MirrorPage | null>(null);
  const [offsets, setOffsets] = useState([0]);
  const [problemsOnly, setProblemsOnly] = useState(true);
  const [readingEvidence, setReadingEvidence] = useState(false);
  const [evidenceProblem, setEvidenceProblem] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const [statusProblem, setStatusProblem] = useState<string | null>(null);
  const [feedback, setFeedback] = useState<string | null>(null);
  const [cancelRequested, setCancelRequested] = useState<string | null>(null);
  const [clock, setClock] = useState(Date.now);
  const live = useRef(false);
  const acting = useRef(false);
  const newestStatus = useRef(0);
  const navigation = useRef<HTMLDivElement>(null);
  const focusStage = useRef<string | null>(null);
  const operation = snapshot?.comparison;
  const receipt = operation?.outcome?.receipt;
  const operationId = operation?.operationId;
  const manifestDigest = receipt?.manifestDigest;
  const offset = offsets.at(-1) ?? 0;
  const binding = snapshot?.bindings.find(item => item.bindingId === activeBindingId) ??
    snapshot?.bindings.find(item => item.bindingId === operation?.bindingId) ?? snapshot?.bindings.at(-1);
  const comparedBinding = snapshot?.bindings.find(item => item.bindingId === operation?.bindingId);
  const currentMembership = prepared && snapshot && currentPreparedMirror(prepared.result, snapshot, prepared.expiresAt, clock);

  useLayoutEffect(() => {
    if (focusStage.current !== tab) return;
    focusStage.current = null;
    navigation.current?.querySelector<HTMLButtonElement>('[aria-pressed=true]')?.focus();
  }, [tab]);

  async function readStatus(): Promise<MirrorStatus> {
    const version = ++newestStatus.current;
    try {
      const next = parseMirrorStatus(await request('mirror_comparison_status'));
      if (live.current && version === newestStatus.current) { setSnapshot(next); setStatusProblem(null); setClock(Date.now()); }
      return next;
    } catch (cause) {
      if (live.current && version === newestStatus.current) setStatusProblem(String(cause));
      throw cause;
    }
  }
  useEffect(() => {
    live.current = true;
    let running = false;
    function poll(): void {
      if (running || acting.current) return;
      running = true;
      void readStatus().catch(() => {}).finally(() => { running = false; });
    }
    poll();
    const interval = setInterval(poll, 1300);
    return () => { live.current = false; newestStatus.current++; clearInterval(interval); };
  }, [instanceId]);

  useEffect(() => {
    setOffsets([0]);
    setExceptions([]);
    setPrepared(null);
  }, [operationId]);
  useEffect(() => {
    if (!operationId || !manifestDigest) { setEvidence(null); setEvidenceProblem(null); return; }
    let active = true;
    setReadingEvidence(true);
    setEvidenceProblem(null);
    void request('read_mirror_evidence', { operationId, offset, limit: PAGE_LIMIT, problemsOnly })
      .then(parseMirrorPage).then(page => {
        if (page.receipt.manifestDigest !== manifestDigest) throw new Error('Mirror evidence changed. Refresh the comparison.');
        if (active) setEvidence(page);
      }).catch(cause => {
        if (active) { setEvidence(null); setEvidenceProblem(String(cause)); }
      }).finally(() => { if (active) setReadingEvidence(false); });
    return () => { active = false; };
  }, [operationId, manifestDigest, offset, problemsOnly]);

  useLayoutEffect(() => {
    controlRef.current = {
      read: () => ({
        bindingId: binding?.bindingId ?? null, operationId: operationId ?? null, freshness: operation?.monitored ? 'monitored' : 'historical-or-unknown',
        tab, busy, problemsOnly, evidenceOffset: offset, exceptionCount: exceptions.length,
        preparedMembershipDigest: prepared?.result.membership.membershipDigest ?? null,
      }),
      execute: async () => { throw new Error('Use the shared mirror operations to act on a reviewed binding or comparison.'); },
    };
    return () => { controlRef.current = null; };
  }, [controlRef, binding?.bindingId, operationId, operation?.monitored, tab, busy, problemsOnly, offset, exceptions.length, prepared]);

  function run(label: string, action: () => Promise<void>): void {
    if (acting.current) return;
    acting.current = true;
    setBusy(label);
    setProblem(null);
    setFeedback(null);
    void action().catch(cause => { if (live.current) setProblem(String(cause)); }).finally(() => {
      acting.current = false;
      if (live.current) setBusy(null);
    });
  }
  function fences(revision: number) {
    if (!enabled || !instanceId) throw new Error('Mirror controls need a current writable review session.');
    return { expectedInstanceId: instanceId, expectedRevision: revision };
  }
  function previewBinding(): void {
    const paths = { source: source.trim(), mirror: mirror.trim() };
    run('Reading selected folder identities…', async () => {
      setPreview(null);
      const observed = await readStatus();
      const binding = parseMirrorBinding(await request('preview_mirror_binding', paths));
      if (live.current) setPreview({ binding, revision: observed.revision, operationId: crypto.randomUUID() });
    });
  }
  function saveBinding(): void {
    if (!preview || !enabled) return;
    run('Saving reviewed binding…', async () => {
      const selected = preview.binding;
      await onSave({ type: 'mirrorBind', source: selected.source.root, mirror: selected.mirror.root,
        expectedBindingId: selected.bindingId, confirmed: true }, preview.revision, preview.operationId);
      if (live.current) {
        setActiveBindingId(selected.bindingId);
        setPreview(null);
        focusStage.current = 'comparison';
        setTab('comparison');
        setFeedback('Saved the two reviewed scope identities.');
      }
      await readStatus();
    });
  }
  function start(): void {
    if (!binding || !snapshot || operation?.active) return;
    const id = crypto.randomUUID();
    run('Submitting the comparison…', async () => {
      const input = { bindingId: binding.bindingId, ...fences(snapshot.revision), operationId: id, confirmed: true };
      try { await onNativeWrite(() => request('start_mirror_comparison', input)); }
      finally { await readStatus().catch(() => {}); }
    });
  }
  function cancel(): void {
    if (!snapshot || !operation?.active) return;
    const id = operation.operationId;
    run('Requesting cancellation…', async () => {
      await request('cancel_mirror_comparison', { ...fences(snapshot.revision), operationId: id });
      if (live.current) setCancelRequested(id);
      await readStatus();
    });
  }
  function prepare(): void {
    if (!snapshot || !operation?.monitored || !evidence || evidence.receipt.manifestDigest !== manifestDigest) return;
    run('Rechecking exact membership…', async () => {
      setPrepared(null);
      const requestedAt = Date.now();
      const result = parsePreparedMirror(await request('prepare_mirror_confirmation', {
        ...fences(snapshot.revision), operationId: operation.operationId, exceptions,
      }));
      if (live.current) {
        setPrepared({ result, expiresAt: requestedAt + result.validForMs, operationId: crypto.randomUUID() });
        focusStage.current = 'confirmation';
        setTab('confirmation');
        setClock(Date.now());
      }
      await readStatus();
    });
  }
  function confirm(): void {
    if (!prepared || !snapshot || !currentPreparedMirror(prepared.result, snapshot, prepared.expiresAt, Date.now())) return;
    run('Saving exact confirmation…', async () => {
      await onSave({ type: 'mirrorConfirm', membershipDigest: prepared.result.membership.membershipDigest, confirmed: true },
        prepared.result.revision, prepared.operationId);
      if (live.current) { setPrepared(null); setFeedback('Saved confirmation for the reviewed membership.'); }
      await readStatus();
    });
  }
  function toggleException(path: string): void {
    if (acting.current) return;
    if (!exceptions.includes(path) && exceptions.length >= EXCEPTION_LIMIT) {
      setProblem('Review up to 1,000 exact exceptions in a single confirmation.');
      return;
    }
    setPrepared(null);
    setExceptions(previous => previous.includes(path) ? previous.filter(item => item !== path) : [...previous, path]);
  }

  const working = busy !== null;
  const cannotWrite = !enabled || !instanceId || working;
  return <div className="mirror-review">
    <div ref={navigation} className="mirror-navigation" role="group" aria-label="Mirror review stages">
      <button type="button" aria-disabled={working} aria-pressed={tab === 'binding'} onClick={() => { if (!working) setTab('binding'); }}>Binding</button>
      <button type="button" aria-disabled={working} aria-pressed={tab === 'comparison'} onClick={() => { if (!working) setTab('comparison'); }}>Comparison</button>
      <button type="button" aria-disabled={working} aria-pressed={tab === 'confirmation'} onClick={() => { if (!working) setTab('confirmation'); }}>Confirmation</button>
    </div>
    {snapshot && <CoverageSummary coverage={snapshot.coverage}/>}
    <div className="mirror-pane" key={tab} aria-label={tab === 'binding' ? 'Binding details' : tab === 'comparison' ? 'Comparison evidence' : 'Confirmation details'}>
    {tab === 'binding' && <>
    <p className="modal-intro">Compare an existing manual mirror with an exact source scope. Both folders are read-only during verification. Saved selected-path manual marks stay separate from confirmed mirror membership.</p>
    <section className="mirror-box" aria-label="Bind source and mirror folders">
      <h3><Folder size={15} aria-hidden="true"/> Bind source and mirror folders</h3>
      <div className="mirror-path-fields">
        <label>Source scope<input value={source} onChange={event => { setSource(event.target.value); setPreview(null); }} disabled={working}/></label>
        <label>Mirror directory<input value={mirror} onChange={event => { setMirror(event.target.value); rememberMirror(event.target.value); setPreview(null); }} disabled={working}/></label>
      </div>
      {preview && <><BindingIdentity binding={preview.binding}/>
        {snapshot?.revision !== preview.revision && <p className="mirror-note">The saved review changed. Review both folder identities again.</p>}
      </>}
    </section>
    {binding && <section className="mirror-box" aria-label="Selected mirror binding"><BindingIdentity binding={binding}/></section>}
    </>}
    {tab === 'comparison' && <>
    {snapshot && snapshot.bindingCount > 1 && <details className="mirror-box"><summary>Saved bindings · {snapshot.bindingCount}</summary>
      <div className="mirror-binding-list">{snapshot.bindings.map(item => <button type="button" key={item.bindingId} aria-pressed={binding?.bindingId === item.bindingId}
        disabled={working} onClick={() => setActiveBindingId(item.bindingId)}>{item.source.root} → {item.mirror.root}</button>)}</div>
      {snapshot.bindingCount > snapshot.bindings.length && <p>Showing {snapshot.bindings.length} saved bindings.</p>}
    </details>}
    {binding ? <section className="mirror-box" aria-label="Selected mirror binding"><BindingIdentity binding={binding}/></section>
      : <p className="modal-intro">Review and save a source and mirror binding before starting a comparison.</p>}
    {operation && <section className="mirror-box" aria-label="Mirror comparison result">
      <h3>{operation.active ? 'Comparing source and mirror' : operation.outcome?.phase === 'complete' ? 'Saved comparison'
        : operation.outcome?.phase === 'canceled' ? 'Comparison canceled' : 'Comparison unavailable'}</h3>
      {comparedBinding?.bindingId !== binding?.bindingId && comparedBinding && <BindingIdentity binding={comparedBinding}/>}
      {operation.active && <>
        {operation.progress && <p>{operation.progress.checkedFiles.toLocaleString()} files checked · {readableBytes(operation.progress.checkedBytes)}
          {' · '}{operation.progress.queuedDirectories.toLocaleString()} directories and {operation.progress.queuedFiles.toLocaleString()} files queued</p>}
      </>}
      {operation.interrupted && <p className="mirror-note">The comparison has no completed outcome. Run a fresh comparison to resume verification.</p>}
      {operation.outcomeProblem && <p className="mirror-error" role="alert">{operation.outcomeProblem}</p>}
      {operation.outcome?.problem && <p className="mirror-error" role="alert">{operation.outcome.problem}</p>}
      {receipt && <>
        <p className={`mirror-freshness${operation.monitored ? ' monitored' : ''}`}>
          {operation.monitored ? 'Monitoring continuous since this comparison' : 'Dated comparison · current state unknown'}
        </p>
        <p>Observed {dateLabel(receipt.finishedAt)} · {receipt.checkedFiles.toLocaleString()} files checked · {readableBytes(receipt.checkedBytes)}</p>
        {operation.outcome?.monitoringIssue && <p className="mirror-note">{operation.outcome.monitoringIssue}</p>}
        <p>{receipt.coverageComplete ? 'Observed comparison coverage complete' : 'Observed comparison has coverage gaps'}
          {' · '}{receipt.sourceMatches ? 'Observed source contents match' : 'Review differences before confirming'}</p>
        <dl className="mirror-counts">{(Object.keys(COUNT_LABELS) as Array<keyof MirrorCounts>).map(key =>
          <div key={key}><dt>{COUNT_LABELS[key]}</dt><dd>{receipt.counts[key].toLocaleString()}</dd></div>)}</dl>
        <div className="mirror-evidence-head">
          <label><input type="checkbox" checked={problemsOnly} disabled={working} onChange={event => {
            setProblemsOnly(event.target.checked); setOffsets([0]);
          }}/> Differences and gaps only</label>
          <span>{evidence ? `${evidence.matched.toLocaleString()} observed entries` : 'Reading evidence…'}</span>
        </div>
        {evidenceProblem && <p className="mirror-error" role="alert">{evidenceProblem}</p>}
        {evidence && <ol className="mirror-evidence" aria-label="Observed mirror evidence" aria-busy={readingEvidence}>
          {evidence.entries.map((entry, index) => <li key={entry.relative + ':' + index}>
            <div><strong>{entry.relative || 'Selected scope'}</strong><span>{VERDICTS[entry.verdict] ?? entry.verdict}{entry.coverageGap && ' · coverage gap'}</span></div>
            {entry.problem && <p>{entry.problem}</p>}
            {entry.sourceProblem && <p>Source · {entry.sourceProblem}</p>}
            {entry.mirrorProblem && <p>Mirror · {entry.mirrorProblem}</p>}
            {(entry.source || entry.mirror) && <details><summary>SHA-256 details</summary>
              {entry.source && <p>Source · {readableBytes(entry.source.bytes)} · <code>{entry.source.sha256}</code></p>}
              {entry.mirror && <p>Mirror · {readableBytes(entry.mirror.bytes)} · <code>{entry.mirror.sha256}</code></p>}
            </details>}
            {entry.relative && <label><input type="checkbox" checked={exceptions.includes(entry.relative)} disabled={working || readingEvidence}
              onChange={() => toggleException(entry.relative)}/> Exclude {entry.relative} from confirmation</label>}
          </li>)}
          {evidence.entries.length === 0 && <li>No entries match this evidence filter.</li>}
        </ol>}
        <div className="mirror-evidence-pages">
          <button type="button" disabled={offsets.length === 1 || readingEvidence || working} onClick={() => setOffsets(previous => previous.slice(0, -1))}><ChevronLeft size={14}/> Previous evidence</button>
          <button type="button" disabled={evidence?.nextOffset == null || readingEvidence || working} onClick={() => {
            if (evidence?.nextOffset != null) setOffsets(previous => [...previous, evidence.nextOffset!]);
          }}>Next evidence <ChevronRight size={14}/></button>
        </div>
        <details className="mirror-exceptions" open={exceptions.length > 0 || undefined}><summary>Exact exceptions · {exceptions.length}</summary>
          <p>Folder exceptions leave the whole subtree outside the confirmation. Differences and gaps still remain in the comparison record.</p>
          <ul>{exceptions.map(path => <li key={path}>{path}<button type="button" disabled={working} onClick={() => toggleException(path)}>Remove exception</button></li>)}</ul>
        </details>
      </>}
    </section>}
    </>}
    {tab === 'confirmation' && <>
    {prepared ? <section className="mirror-box mirror-prepared" aria-label="Prepared exact confirmation">
      <h3><ShieldCheck size={16} aria-hidden="true"/> Exact membership for confirmation</h3>
      <p>{prepared.result.membership.coveredFiles.toLocaleString()} matching files · {prepared.result.membership.coveredDirectories.toLocaleString()} directories</p>
      <p>Membership digest <code>{prepared.result.membership.membershipDigest}</code></p>
      <p>{prepared.result.membership.exceptions.length} reviewed exceptions</p>
      <ul>{prepared.result.membership.exceptions.map(path => <li key={path}>{path}</li>)}</ul>
      <p>This records your confirmation for the listed membership. Later additions and excluded branches require their own review.</p>
      {!currentMembership && <p className="mirror-error">The membership is expired or no longer current. Prepare it again after review.</p>}
    </section> : <p className="modal-intro">Review the comparison and exact exceptions, then prepare membership for confirmation.</p>}
    {snapshot && snapshot.confirmationCount > 0 && <details className="mirror-box">
      <summary>Saved membership confirmations · {snapshot.confirmationCount}</summary>
      <ul className="mirror-confirmation-list">{snapshot.confirmations.map(saved => <li key={saved.operationId}>
        <strong>Confirmed {dateLabel(saved.confirmedAt)}</strong>
        <p>{saved.coveredFiles.toLocaleString()} files · {saved.coveredDirectories.toLocaleString()} directories · {saved.exceptionsCount} exceptions</p>
        <code>{saved.membershipDigest}</code>
        {saved.exceptionsCount > 0 && <SavedExceptions operationId={saved.operationId} membershipDigest={saved.membershipDigest}/>}
      </li>)}</ul>
      {snapshot.confirmationCount > snapshot.confirmations.length && <p>Showing {snapshot.confirmations.length} saved confirmations.</p>}
    </details>}
    </>}
    </div>
    {busy && <p role="status">{busy}</p>}
    {feedback && <p role="status">{feedback}</p>}
    {(problem || statusProblem) && <p className="mirror-error" role="alert"><TriangleAlert size={16} aria-hidden="true"/>{problem || statusProblem}</p>}
    <div className="mirror-actions">
      {tab === 'binding' && <>
        <button type="button" disabled={working || !source.trim() || !mirror.trim()} onClick={previewBinding}>Review folder identities</button>
        {preview && <button type="button" className="primary" disabled={cannotWrite || snapshot?.revision !== preview.revision}
          onClick={saveBinding}>Bind reviewed folders</button>}
      </>}
      {tab === 'comparison' && <>
        {operation?.active ? <button type="button" disabled={cannotWrite || cancelRequested === operationId} onClick={cancel}>
          {cancelRequested === operationId ? 'Cancellation requested' : 'Cancel comparison'}
        </button> : <button type="button" className="primary" disabled={cannotWrite || !binding || !snapshot || !!statusProblem}
          onClick={start}>Start SHA-256 comparison</button>}
        {receipt && <button type="button" disabled={cannotWrite || !operation?.monitored || !evidence || readingEvidence || !!evidenceProblem || !!statusProblem}
          onClick={prepare}>Prepare exact confirmation</button>}
      </>}
      {tab === 'confirmation' && <button type="button" className="primary" disabled={cannotWrite || !currentMembership || !!statusProblem}
        onClick={confirm}>Confirm reviewed membership</button>}
    </div>
  </div>;
}

function SavedExceptions({ operationId, membershipDigest }: { operationId: string; membershipDigest: string }) {
  const [open, setOpen] = useState(false);
  const [offsets, setOffsets] = useState([0]);
  const [page, setPage] = useState<ReturnType<typeof parseSavedMirrorExceptions> | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const offset = offsets.at(-1) ?? 0;
  useEffect(() => {
    if (!open) return;
    let current = true;
    setPage(null);
    setProblem(null);
    void request('read_mirror_confirmation', { operationId, offset, limit: PAGE_LIMIT })
      .then(parseSavedMirrorExceptions).then(result => {
        if (result.operationId !== operationId || result.membershipDigest !== membershipDigest) {
          throw new Error('Saved membership identity changed. Open its latest record.');
        }
        if (current) setPage(result);
      }).catch(cause => { if (current) setProblem(String(cause)); });
    return () => { current = false; };
  }, [open, offset, operationId, membershipDigest]);
  return <details onToggle={event => setOpen(event.currentTarget.open)}>
    <summary>Read exact saved exceptions</summary>
    {open && (problem ? <p className="mirror-error" role="alert">{problem}</p> : page ? <>
      <ul>{page.exceptions.map(path => <li key={path}>{path}</li>)}</ul>
      <div className="mirror-evidence-pages">
        <button type="button" disabled={offsets.length === 1} onClick={() => setOffsets(previous => previous.slice(0, -1))}>Previous exceptions</button>
        <button type="button" disabled={page.nextOffset === null} onClick={() => {
          const next = page.nextOffset;
          if (next !== null) setOffsets(previous => [...previous, next]);
        }}>Next exceptions</button>
      </div>
    </> : <p role="status">Reading saved exceptions…</p>)}
  </details>;
}

function CoverageSummary({ coverage }: { coverage: MirrorStatus['coverage'] }) {
  const labels = ['Missing', 'Different', 'Unsupported', 'Unreadable', 'Coverage gaps'];
  const observed = [coverage.missing, coverage.different, coverage.unsupported, coverage.unreadable, coverage.coverageGaps];
  return <details className="mirror-coverage">
    <summary>Coverage · {coverage.savedManualPendingMarks.toLocaleString()} saved manual pending · {coverage.deferredInterviewScopes.toLocaleString()} deferred scopes</summary>
    <div>
      <p>Saved counts cover review records. Comparison counts describe the selected evidence.</p>
      <p>{coverage.historical ? 'Dated comparison counts' : 'Observed comparison counts'}</p>
      <dl className="mirror-counts">{labels.map((label, index) => <div key={label}>
        <dt>{label}</dt><dd>{observed[index]?.toLocaleString() ?? 'Unknown'}</dd>
      </div>)}</dl>
    </div>
  </details>;
}

function BindingIdentity({ binding }: { binding: MirrorBinding }) {
  return <dl className="mirror-identities">
    <div><dt>Source</dt><dd>{binding.source.root}<small>Volume {binding.source.volumeId}</small><small>Directory identity {binding.source.rootIdentity}</small></dd></div>
    <div><dt>Mirror</dt><dd>{binding.mirror.root}<small>Volume {binding.mirror.volumeId}</small><small>Directory identity {binding.mirror.rootIdentity}</small></dd></div>
  </dl>;
}
