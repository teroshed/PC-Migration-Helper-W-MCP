import { useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import type { UiControlRef } from './UIBridge';
import { Check, FolderTree, ScanSearch, Sparkles } from 'lucide-react';
import { LABELS, StatusBadge, EMPTY_EFFECTIVE } from './classifications';
import { STATUSES, isStatus, readableBytes, record } from './model';
import type { Operation, Status } from './model';
import { inventoryElapsed, inventoryProgressLabel, parseInventoryProgress } from './assistance-progress';
import type { InventoryProgress } from './assistance-progress';
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
  adaptive: boolean;
  recommendation: AdaptiveRecommendation | null;
}

const PHASES = ['idle', 'needs-scan', 'scanning', 'other-scan', 'paused', 'question', 'complete'] as const;
const ADAPTIVE_PHASES = ['local', 'waiting-inventory', 'waiting-model', 'question', 'revalidate', 'limit', 'unavailable'] as const;
const RECOMMENDATIONS = ['keep', 'review-children', 'defer', 'ask-rebuildability'] as const;
type AdaptiveRecommendation = typeof RECOMMENDATIONS[number];
type AdaptivePhase = typeof ADAPTIVE_PHASES[number];
interface AdaptiveInterview {
  mode: 'local' | 'external';
  phase: AdaptivePhase;
  remainingQuestions: number;
  questionLimit: number;
  sessionId: string | null;
  connectedAt: string | null;
  usage: string;
  retainedQuestion: { prompt: string; reason: string; path: string; id: string } | null;
}
const LOCAL_MODE: AdaptiveInterview = { mode: 'local', phase: 'local', remainingQuestions: 0, questionLimit: 0,
  sessionId: null, connectedAt: null, usage: 'External client usage is not measured by this app.', retainedQuestion: null };
const MAX_EXTERNAL_QUESTIONS = 128;

interface Interview {
  phase: typeof PHASES[number];
  scope: string | null;
  revision: number;
  question: Question | null;
  files: number;
  gaps: number;
  deferred: number;
  manualPending: number;
  progress: InventoryProgress;
  scanProblem: string | null;
  adaptive: AdaptiveInterview;
}

function count(value: unknown): number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0 ? value : 0;
}

function parseAdaptive(value: unknown): AdaptiveInterview {
  if (value == null) return LOCAL_MODE;
  const data = record(value);
  const phase = ADAPTIVE_PHASES.find(item => item === data.phase);
  if (!phase || (data.mode !== 'local' && data.mode !== 'external')) throw new Error('The app returned an invalid adaptive interview state.');
  const retained = data.retainedQuestion ? record(data.retainedQuestion) : null;
  const retainedQuestion = retained && typeof retained.prompt === 'string' && typeof retained.reason === 'string' &&
    typeof retained.path === 'string' && typeof retained.id === 'string'
    ? { prompt: retained.prompt, reason: retained.reason, path: retained.path, id: retained.id } : null;
  return {
    mode: data.mode, phase, remainingQuestions: count(data.remainingQuestions), questionLimit: count(data.questionLimit),
    sessionId: typeof data.sessionId === 'string' ? data.sessionId : null,
    connectedAt: typeof data.connectedAt === 'string' ? data.connectedAt : null,
    usage: typeof data.usage === 'string' && data.usage ? data.usage : LOCAL_MODE.usage, retainedQuestion,
  };
}

function parseInterview(input: unknown): Interview {
  const data = record(input);
  const phase = PHASES.find(value => value === data.phase);
  if (!phase || typeof data.revision !== 'number' || !Number.isSafeInteger(data.revision)) {
    throw new Error('The app returned an invalid interview state.');
  }
  const adaptive = parseAdaptive(data.adaptive);
  let question: Question | null = null;
  if (phase === 'question' && (adaptive.mode === 'local' || adaptive.phase === 'question')) {
    const item = record(data.question);
    const { id, path, kind, prompt, status, reason, source } = item;
    if (typeof id !== 'string' || typeof path !== 'string' || typeof prompt !== 'string'
      || typeof reason !== 'string' || !isStatus(status) || status === 'attention'
      || (kind !== 'file' && kind !== 'folder') || (source !== 'codex' && source !== 'local')
      || (item.manualConfirmation === true) !== (status === 'manual-confirmed')) {
      throw new Error('The app returned an invalid interview question.');
    }
    if (adaptive.mode === 'external' && (source !== 'codex' || item.adaptive !== true || status !== 'last-moment')) {
      throw new Error('The app returned an invalid external retention proposal.');
    }
    question = { id, path, kind, prompt, status, reason, source, files: count(item.files), bytes: count(item.bytes),
      agentStatus: isStatus(item.agentStatus) ? item.agentStatus : null,
      manualConfirmation: item.manualConfirmation === true, adaptive: item.adaptive === true,
      recommendation: RECOMMENDATIONS.find(value => value === item.recommendation) ?? null };
  }
  const scan = data.scan ? record(data.scan) : {};
  return {
    phase, question, revision: data.revision, scope: typeof data.scope === 'string' ? data.scope : null,
    files: count(data.files), gaps: count(data.gaps), deferred: count(data.deferred), manualPending: count(data.manualPending),
    progress: parseInventoryProgress(scan), scanProblem: typeof scan.error === 'string' ? scan.error : null, adaptive,
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
  const [overrideDraft, setOverrideDraft] = useState<{ questionId: string; status: Status } | null>(null);
  const [questionLimit, setQuestionLimit] = useState('12');
  const [nowMs, setNowMs] = useState(() => Date.now());
  const inFlight = useRef(false);
  const heading = useRef<HTMLHeadingElement>(null);
  const override = useRef<HTMLFieldSetElement>(null);
  const overrideConfirm = useRef<HTMLButtonElement>(null);
  const question = interview?.question;
  const questionId = question?.id;
  const choosing = questionId !== undefined && overrideQuestion === questionId;
  const changedChoice = choosing && overrideDraft?.questionId === questionId ? overrideDraft.status : null;
  const scanStartedAt = interview?.progress.startedAt;
  useEffect(() => {
    if (!scanStartedAt || !['scanning', 'paused'].includes(interview?.phase ?? '')) return;
    setNowMs(Date.now());
    const timer = setInterval(() => setNowMs(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [scanStartedAt, interview?.phase]);
  const elapsed = inventoryElapsed(scanStartedAt ?? null, nowMs);
  function chooseOverride(open: boolean): void {
    if (open && (!questionId || readOnly || reading || working || problem)) throw new Error('The current question is not available for override.');
    setOverrideQuestion(open ? questionId ?? null : null);
    setOverrideDraft(null);
    if (!open) heading.current?.focus();
  }
  controlRef.current = {
    read: () => ({ interview, reading, working, problem, actionProblem, overrideOpen: choosing, overrideDraft: changedChoice }),
    execute: async command => {
      if (command.action !== 'setAssistanceOverride' || command.questionId !== questionId) throw new Error('The interview question changed. Read it again before opening its override.');
      chooseOverride(command.open === true);
    },
  };
  useEffect(() => () => { controlRef.current = null; }, [controlRef]);

  useEffect(() => {
    let active = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let external = false;
    setReading(true);
    async function read(): Promise<void> {
      try {
        const result = parseInterview(await invoke<unknown>('get_assisted_review'));
        if (!active) return;
        setInterview(result);
        setProblem(null);
        external = result.adaptive.mode === 'external';
        if (external || result.phase === 'scanning' || result.phase === 'other-scan') timer = setTimeout(read, external ? 1600 : 1200);
      } catch (error) {
        if (active) setProblem(String(error));
        if (active && external) timer = setTimeout(read, 2200);
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
  useEffect(() => {
    if (changedChoice) overrideConfirm.current?.focus();
  }, [changedChoice]);

  async function perform(work: () => Promise<unknown>): Promise<void> {
    if (inFlight.current) return;
    inFlight.current = true;
    setWorking(true);
    setActionProblem(null);
    try {
      await work();
      setOverrideQuestion(null);
      setOverrideDraft(null);
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

  function answer(status: Status, overrideConfirmed = false): void {
    if (!question) return;
    const changedExternal = question.adaptive && status !== question.status;
    if (changedExternal && !overrideConfirmed) {
      setOverrideDraft({ questionId: question.id, status });
      return;
    }
    edit({ type: 'assistAnswer', path: question.path, questionId: question.id, status,
      ...(changedExternal ? { overrideConfirmed: true } : {}) });
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
  const adaptive = interview?.adaptive;
  const external = adaptive?.mode === 'external';
  const limit = Number(questionLimit);
  const validLimit = Number.isSafeInteger(limit) && limit >= 1 && limit <= MAX_EXTERNAL_QUESTIONS;
  const externalWaiting = external && adaptive.phase !== 'question';
  const folderExclusionBlocked = question?.adaptive && question.kind === 'folder';

  return <div className="assistance-interview" aria-busy={reading || working}>
    <div className="assistance-scope">
      <span>{scope ? 'Review scope' : 'Current folder'}</span>
      <strong title={scope ?? currentPath}>{scope ?? currentPath}</strong>
    </div>
    {adaptive && <section className="assistance-mode" aria-label="Question mode">
      <div className="assistance-mode-heading"><strong>{external ? 'External Codex questions' : 'Local questions'}</strong>
        {external && <span>{adaptive.remainingQuestions.toLocaleString()} of {adaptive.questionLimit.toLocaleString()} questions remaining</span>}
      </div>
      {external ? <>
        <p role="status">{adaptiveLabel(adaptive)}</p>
        <p className="assistance-mode-usage">{adaptive.usage}</p>
        <details><summary>Connect the external Codex client</summary>
          <p>Open the Codex chat with this app’s Migration MCP connected. Ask Codex to send only the packet from <code>get_adaptive_question_packet</code> to the questioning model and submit one proposal with <code>propose_adaptive_question</code>. Keep file paths and notes in the app. Review the question in this window.</p>
        </details>
        <div className="assistance-actions">
          <button disabled={locked} onClick={() => edit({ type: 'adaptiveControl', action: 'local' })}>Use local questions</button>
          {adaptive.phase === 'revalidate' && <button disabled={locked || (phase !== 'question' && phase !== 'complete')}
            onClick={() => edit({ type: 'adaptiveControl', action: 'resume' })}>
            Revalidate saved question
          </button>}
        </div>
        {adaptive.retainedQuestion && adaptive.phase !== 'question' && <details className="assistance-retained">
          <summary>Earlier proposal · awaiting current evidence</summary>
          <p>{adaptive.retainedQuestion.prompt}</p><p className="assistance-path" title={adaptive.retainedQuestion.path}>{adaptive.retainedQuestion.path}</p>
          <p>{adaptive.retainedQuestion.reason}</p>
        </details>}
      </> : null}
      {!external && <div className="assistance-mode-start">
        <label>External question limit <input type="number" min={1} max={MAX_EXTERNAL_QUESTIONS} step={1}
          value={questionLimit} disabled={locked} onChange={event => setQuestionLimit(event.target.value)}/></label>
        <button disabled={locked || !validLimit || !scope} onClick={() => edit({ type: 'adaptiveControl', action: 'external', questionLimit: limit })}>
          Use external Codex
        </button>
        {!scope && <small>Choose a review folder first.</small>}
      </div>}
    </section>}
    {problem && <p role="alert" className="assistance-error">{problem}</p>}
    {actionProblem && <p role="alert" className="assistance-error">{actionProblem}</p>}
    {working && <p role="status">Updating interview.</p>}
    {!interview && !problem && <div className="assistance-progress-panel">
      <p role="status">Loading saved inventory and preparing the interview.</p>
      <progress className="assistance-progress-bar" aria-label="Preparing interview" />
    </div>}
    {(phase === 'idle' || problem) && <p>Choose a folder to review. The interview uses inventoried metadata to ask one question at a time.</p>}
    {phase === 'needs-scan' && <p>Run the metadata inventory for this folder first. The interview starts when it finishes.</p>}
    {phase === 'needs-scan' && interview?.scanProblem && <p role="alert" className="assistance-error">{interview.scanProblem}</p>}
    {(phase === 'scanning' || phase === 'paused') && interview && <section className="assistance-progress-panel" aria-label="Inventory progress">
      <div className="assistance-progress-heading"><strong>{phase === 'paused' ? 'Inventory paused' : 'Discovering folders'}</strong>
        {interview.progress.workers > 0 && <span>{interview.progress.workers} workers</span>}</div>
      {phase === 'scanning' && <progress className="assistance-progress-bar" aria-label="Metadata inventory"
        aria-valuetext={inventoryProgressLabel(interview.progress)} />}
      <p role="status">{inventoryProgressLabel(interview.progress)}</p>
      <p>{interview.progress.files.toLocaleString()} files found · {readableBytes(interview.progress.bytes)} logical size</p>
      {interview.progress.currentPath && <p className="assistance-progress-path" title={interview.progress.currentPath}>
        {interview.progress.currentPath}</p>}
      <small>{elapsed && <>{elapsed} since scan started · </>}
        {interview.progress.gaps > 0 && <>{interview.progress.gaps.toLocaleString()} recorded coverage gaps · </>}
        The total grows as folders are discovered. A reliable percentage and ETA are not available yet.</small>
      <small>Metadata inventory only. It does not hash file contents or verify a backup.</small>
    </section>}
    {phase === 'other-scan' && <p role="status">Another folder is being inventoried. You can start this scope when that scan stops.</p>}
    {phase === 'paused' && <p>The inventory is paused. Resume it to continue the interview.</p>}
    {question && <section className="assistance-question" aria-labelledby="assistance-question-title">
      <div className="assistance-recommendation">
        <span><Sparkles size={15} aria-hidden="true"/>{question.manualConfirmation
          ? 'Manual backup follow-up' : question.adaptive ? 'External Codex proposal' : question.source === 'codex' ? 'With Codex input' : 'Local suggestion'}</span>
        <StatusBadge effective={{ ...EMPTY_EFFECTIVE, status: question.manualConfirmation ? 'manual-pending' : question.status }}/>
      </div>
      <h3 id="assistance-question-title" ref={heading} tabIndex={-1}>{question.prompt}</h3>
      <p className="assistance-path" title={question.path}>{question.path}</p>
      <p>{question.files.toLocaleString()} inventoried {question.files === 1 ? 'file' : 'files'} in scope · {readableBytes(question.bytes)} logical size</p>
      <p className="assistance-reason">{question.reason}</p>
      {question.recommendation && question.recommendation !== 'keep' &&
        <p>{question.recommendation === 'review-children' ? 'Codex suggests reviewing children.' :
          question.recommendation === 'defer' ? 'Codex suggests leaving this question for later.' : 'Codex asks whether this data can be recreated.'}</p>}
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
        {folderExclusionBlocked && <p>Review children before excluding this folder’s contents.</p>}
        <p>Marked for Manual Backup keeps your copy pending. Choose Confirmed Manual Backup after copying the selected path; review child entries separately.</p>
        {STATUSES.filter(status => status !== 'attention' && status !== question.status).map(status =>
          <button key={status} aria-pressed={question.adaptive ? changedChoice === status : undefined}
            disabled={locked || Boolean(problem) || Boolean(folderExclusionBlocked && (status === 'redownload' || status === 'erase'))}
            onClick={() => answer(status)}>{LABELS[status]}</button>)}
        {changedChoice && <div className="assistance-confirm-override">
          <p>Apply {LABELS[changedChoice]} to this exact reviewed path.</p>
          <button ref={overrideConfirm} className="primary" disabled={locked || Boolean(problem)} onClick={() => answer(changedChoice, true)}>
            Confirm override
          </button>
        </div>}
        <button onClick={() => chooseOverride(false)}>Back</button>
      </fieldset> : <div className="assistance-actions">
        <button className="primary" disabled={locked || Boolean(problem)} onClick={() => answer(question.status)}>
          <Check size={16} aria-hidden="true"/>{question.manualConfirmation ? 'Confirm manual backup' : question.adaptive
            ? `Approve keep · ${LABELS[question.status]}` : `Approve ${LABELS[question.status]}`}
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
    {phase === 'complete' && interview && !externalWaiting && <p role="status">{interview.gaps || interview.deferred || interview.manualPending
      ? 'The interview has no more eligible questions. Pending manual backups, coverage gaps and deferred items still need attention.'
      : 'The interview is complete for this inventory.'}</p>}
    {interview && (phase === 'question' || phase === 'complete' || external) && <p className="assistance-coverage">
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

function adaptiveLabel(state: AdaptiveInterview): string {
  switch (state.phase) {
    case 'waiting-inventory': return 'Waiting for inventory coverage.';
    case 'waiting-model': return state.connectedAt ? 'Waiting for the next external proposal.' : 'Waiting for a Codex proposal through MCP.';
    case 'question': return 'Question ready for your review.';
    case 'revalidate': return 'The saved proposal needs current evidence before it can be answered.';
    case 'limit': return 'External question limit reached. Use local questions to continue or choose a new external session.';
    case 'unavailable': return 'No eligible external question is ready. Continue locally for manual or unresolved work.';
    case 'local': return 'Local questions are available.';
  }
}
