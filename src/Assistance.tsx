import { useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import type { UiControlRef } from './UIBridge';
import { Check, FolderTree, ScanSearch, Sparkles } from 'lucide-react';
import { LABELS, StatusBadge, EMPTY_EFFECTIVE } from './classifications';
import { STATUSES, isStatus, readableBytes, record } from './model';
import type { Operation, Status } from './model';
import './assistance.css';

interface Question {
  id: string;
  path: string;
  kind: 'file' | 'folder';
  files: number;
  bytes: number;
  prompt: string;
  status: Status;
  reason: string;
  source: 'codex' | 'local';
  agentStatus: Status | null;
  manualConfirmation: boolean;
}

const PHASES = ['idle', 'needs-scan', 'scanning', 'other-scan', 'paused', 'question', 'complete'] as const;

interface Interview {
  phase: typeof PHASES[number];
  scope: string | null;
  revision: number;
  question: Question | null;
  files: number;
  gaps: number;
  deferred: number;
  manualPending: number;
  scannedDirectories: number;
  scanProblem: string | null;
}

function count(value: unknown): number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0 ? value : 0;
}

function parseInterview(input: unknown): Interview {
  const data = record(input);
  const phase = PHASES.find(value => value === data.phase);
  if (!phase || typeof data.revision !== 'number' || !Number.isSafeInteger(data.revision)) {
    throw new Error('The app returned an invalid interview state.');
  }
  let question: Question | null = null;
  if (phase === 'question') {
    const item = record(data.question);
    const { id, path, kind, prompt, status, reason, source } = item;
    if (typeof id !== 'string' || typeof path !== 'string' || typeof prompt !== 'string'
      || typeof reason !== 'string' || !isStatus(status) || status === 'attention'
      || (kind !== 'file' && kind !== 'folder') || (source !== 'codex' && source !== 'local')
      || (item.manualConfirmation === true) !== (status === 'manual-confirmed')) {
      throw new Error('The app returned an invalid interview question.');
    }
    question = { id, path, kind, prompt, status, reason, source, files: count(item.files), bytes: count(item.bytes),
      agentStatus: isStatus(item.agentStatus) ? item.agentStatus : null,
      manualConfirmation: item.manualConfirmation === true };
  }
  const scan = data.scan ? record(data.scan) : {};
  return {
    phase, question, revision: data.revision, scope: typeof data.scope === 'string' ? data.scope : null,
    files: count(data.files), gaps: count(data.gaps), deferred: count(data.deferred), manualPending: count(data.manualPending),
    scannedDirectories: count(scan.processed), scanProblem: typeof scan.error === 'string' ? scan.error : null,
  };
}

export function AssistanceInterview({ currentPath, revision, readOnly, onEdit, controlRef }: {
  currentPath: string;
  revision: number;
  readOnly: boolean;
  onEdit: (operation: Operation, reviewedRevision: number) => Promise<void>;
  controlRef: UiControlRef;
}) {
  const [interview, setInterview] = useState<Interview | null>(null);
  const [reading, setReading] = useState(true);
  const [working, setWorking] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const [actionProblem, setActionProblem] = useState<string | null>(null);
  const [reload, setReload] = useState(0);
  const [overrideQuestion, setOverrideQuestion] = useState<string | null>(null);
  const inFlight = useRef(false);
  const heading = useRef<HTMLHeadingElement>(null);
  const override = useRef<HTMLFieldSetElement>(null);
  const question = interview?.question;
  const questionId = question?.id;
  const choosing = questionId !== undefined && overrideQuestion === questionId;
  function chooseOverride(open: boolean): void {
    if (open && (!questionId || readOnly || reading || working || problem)) throw new Error('The current question is not available for override.');
    setOverrideQuestion(open ? questionId ?? null : null);
    if (!open) heading.current?.focus();
  }
  controlRef.current = {
    read: () => ({ interview, reading, working, problem, actionProblem, overrideOpen: choosing }),
    execute: async command => {
      if (command.action !== 'setAssistanceOverride' || command.questionId !== questionId) throw new Error('The interview question changed. Read it again before opening its override.');
      chooseOverride(command.open === true);
    },
  };
  useEffect(() => () => { controlRef.current = null; }, [controlRef]);

  useEffect(() => {
    let active = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    setReading(true);
    async function read(): Promise<void> {
      try {
        const result = parseInterview(await invoke<unknown>('get_assisted_review'));
        if (!active) return;
        setInterview(result);
        setProblem(null);
        if (result.phase === 'scanning' || result.phase === 'other-scan') timer = setTimeout(read, 1200);
      } catch (error) {
        if (active) setProblem(String(error));
      } finally {
        if (active) setReading(false);
      }
    }
    void read();
    return () => { active = false; clearTimeout(timer); };
  }, [revision, reload]);

  useEffect(() => {
    if (questionId) heading.current?.focus();
  }, [questionId]);

  useEffect(() => {
    if (choosing) override.current?.querySelector('button')?.focus();
  }, [choosing]);

  async function perform(work: () => Promise<unknown>): Promise<void> {
    if (inFlight.current) return;
    inFlight.current = true;
    setWorking(true);
    setActionProblem(null);
    try {
      await work();
      setOverrideQuestion(null);
    } catch (error) {
      setActionProblem(String(error));
    } finally {
      inFlight.current = false;
      setWorking(false);
      setReading(true);
      setReload(value => value + 1);
    }
  }

  function edit(operation: Operation): void {
    void perform(() => onEdit(operation, interview?.revision ?? revision));
  }

  function answer(status: Status): void {
    if (question) edit({ type: 'assistAnswer', path: question.path, questionId: question.id, status });
  }

  function scan(mode: 'current' | 'pause' | 'resume'): void {
    if (!interview?.scope) return;
    const path = interview.scope;
    void perform(() => invoke('scan', { mode, path }));
  }

  const locked = readOnly || reading || working;
  const phase = interview?.phase;
  const scope = interview?.scope;
  const canChangeScope = scope?.toLowerCase() !== currentPath.toLowerCase();

  return <div className="assistance-interview" aria-busy={reading || working}>
    <div className="assistance-scope">
      <span>{scope ? 'Review scope' : 'Current folder'}</span>
      <strong title={scope ?? currentPath}>{scope ?? currentPath}</strong>
    </div>
    {problem && <p role="alert" className="assistance-error">{problem}</p>}
    {actionProblem && <p role="alert" className="assistance-error">{actionProblem}</p>}
    {working && <p role="status">Saving your interview choice…</p>}
    {!interview && !problem && <p role="status">Loading the interview…</p>}
    {(phase === 'idle' || problem) && <p>Choose a folder to review. The interview uses inventoried metadata to ask one question at a time.</p>}
    {phase === 'needs-scan' && <p>Run the metadata inventory for this folder first. The interview starts when it finishes.</p>}
    {phase === 'needs-scan' && interview?.scanProblem && <p role="alert" className="assistance-error">{interview.scanProblem}</p>}
    {phase === 'scanning' && interview && <p role="status">Scanning directories… {interview.scannedDirectories.toLocaleString()} processed.</p>}
    {phase === 'other-scan' && <p role="status">Another folder is being inventoried. You can start this scope when that scan stops.</p>}
    {phase === 'paused' && <p>The inventory is paused. Resume it to continue the interview.</p>}
    {question && <section className="assistance-question" aria-labelledby="assistance-question-title">
      <div className="assistance-recommendation">
        <span><Sparkles size={15} aria-hidden="true"/>{question.manualConfirmation
          ? 'Manual backup follow-up' : question.source === 'codex' ? 'With Codex input' : 'Local suggestion'}</span>
        <StatusBadge effective={{ ...EMPTY_EFFECTIVE, status: question.manualConfirmation ? 'manual-pending' : question.status }}/>
      </div>
      <h3 id="assistance-question-title" ref={heading} tabIndex={-1}>{question.prompt}</h3>
      <p className="assistance-path" title={question.path}>{question.path}</p>
      <p>{question.files.toLocaleString()} inventoried {question.files === 1 ? 'file' : 'files'} in scope · {readableBytes(question.bytes)} logical size</p>
      <p className="assistance-reason">{question.reason}</p>
      {question.agentStatus && question.agentStatus !== question.status && <p>
        Codex suggested {LABELS[question.agentStatus]}. Keeping is the default until you explicitly override it.
      </p>}
      {question.kind === 'folder' && <p>{question.manualConfirmation
        ? 'This records your confirmation for the selected path. Review children separately. Explicit child decisions still apply.'
        : 'Explicit child decisions still apply. New entries can inherit this folder’s decision.'}</p>}
      {question.manualConfirmation
        ? <p>Confirm after completing your manual copy. Destination checksums and restore testing are separate evidence.</p>
        : <p>If Auto-copy is on, Migrate now can enter the copy queue.</p>}
      {choosing ? <fieldset ref={override} className="assistance-overrides">
        <legend>Choose a different decision</legend>
        <p>Choose To Redownload only if you can recreate what you need. To be ERASED excludes the item from migration.</p>
        <p>Marked for Manual Backup keeps your copy pending. Choose Confirmed Manual Backup after copying the selected path; review child entries separately.</p>
        {STATUSES.filter(status => status !== 'attention' && status !== question.status).map(status =>
          <button key={status} disabled={locked || Boolean(problem)} onClick={() => answer(status)}>{LABELS[status]}</button>)}
        <button onClick={() => chooseOverride(false)}>Back</button>
      </fieldset> : <div className="assistance-actions">
        <button className="primary" disabled={locked || Boolean(problem)} onClick={() => answer(question.status)}>
          <Check size={16} aria-hidden="true"/>{question.manualConfirmation ? 'Confirm manual backup' : `Approve ${LABELS[question.status]}`}
        </button>
        <button disabled={locked || Boolean(problem)} onClick={() => chooseOverride(true)}>Override</button>
        {question.kind === 'folder' && !question.manualConfirmation && <button disabled={locked || Boolean(problem)}
          onClick={() => edit({ type: 'assist', action: 'split', path: question.path, questionId: question.id })}>
          <FolderTree size={16} aria-hidden="true"/> Review children
        </button>}
        <button disabled={locked || Boolean(problem)}
          onClick={() => edit({ type: 'assist', action: 'defer', path: question.path, questionId: question.id })}>Defer</button>
      </div>}
    </section>}
    {phase === 'complete' && interview && <p role="status">{interview.gaps || interview.deferred || interview.manualPending
      ? 'The interview has no more eligible questions. Pending manual backups, coverage gaps and deferred items still need attention.'
      : 'The interview is complete for this inventory.'}</p>}
    {interview && (phase === 'question' || phase === 'complete') && <p className="assistance-coverage">
      {interview.files.toLocaleString()} files inventoried across this scope
      {interview.gaps > 0 && <> · {interview.gaps.toLocaleString()} coverage gaps</>}
      {interview.deferred > 0 && <> · {interview.deferred.toLocaleString()} deferred</>}
      {interview.manualPending > 0 && <> · {interview.manualPending.toLocaleString()} manual backups pending</>}
    </p>}
    <div className="assistance-actions assistance-scope-actions">
      {(phase === 'idle' || problem || canChangeScope) && <button disabled={locked} onClick={() => edit({ type: 'assist', action: 'scope', path: currentPath })}>
        {scope ? 'Use current folder' : 'Review this folder'}
      </button>}
      {scope && (phase === 'needs-scan' || phase === 'complete' || problem) && <button disabled={locked} onClick={() => scan('current')}>
        <ScanSearch size={16} aria-hidden="true"/>{phase === 'needs-scan' ? 'Scan this folder' : 'Scan again'}
      </button>}
      {phase === 'scanning' && <button disabled={locked} onClick={() => scan('pause')}>Pause scan</button>}
      {phase === 'paused' && <button disabled={locked} onClick={() => scan('resume')}>Resume scan</button>}
      {Boolean(interview?.deferred) && <button disabled={locked} onClick={() => edit({ type: 'assist', action: 'restore' })}>Review deferred items</button>}
      {problem && <button disabled={reading || working} onClick={() => setReload(value => value + 1)}>Refresh interview</button>}
    </div>
    {readOnly && <p>Recover the saved journal before changing interview choices.</p>}
  </div>;
}
