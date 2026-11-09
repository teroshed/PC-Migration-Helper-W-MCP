import { useEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { invoke } from '@tauri-apps/api/core';
import type { UiControlRef } from './UIBridge';
import { Check, ChevronDown, FolderTree, Pause, Play, ScanSearch, Sparkles } from 'lucide-react';
import { DECISION_CHOICES, LABELS, StatusBadge, EMPTY_EFFECTIVE, decisionChoiceLabel } from './classifications';
import { isStatus, parentPath, readableBytes, record } from './model';
import type { Operation, Status } from './model';
import { inventoryElapsed, inventoryProgressLabel, parseInventoryProgress } from './assistance-progress';
import type { InventoryProgress } from './assistance-progress';
import { OverflowIndicators } from './OverflowIndicators';
import { reviewPathKey } from './ReviewActivity';
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
  early: boolean;
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
const SCAN_PHASES = ['running', 'paused', 'complete', 'error'] as const;
type ScanPhase = typeof SCAN_PHASES[number];
const SCAN_DETAILS_PREFERENCE = 'pc-migration-helper.assistance-scan-details';

function readScanDetailsPreference(): boolean {
  try { return localStorage.getItem(SCAN_DETAILS_PREFERENCE) === 'true'; }
  catch { return false; }
}

interface Interview {
  skippedGenerated: number;
  phase: typeof PHASES[number];
  scope: string | null;
  earlyBranch: string | null;
  revision: number;
  question: Question | null;
  files: number;
  gaps: number;
  deferred: number;
  manualPending: number;
  progress: InventoryProgress;
  scanPhase: ScanPhase | null;
  scanSkippedGenerated: number;
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
      manualConfirmation: item.manualConfirmation === true, early: typeof item.earlyBranch === 'string', adaptive: item.adaptive === true,
      recommendation: RECOMMENDATIONS.find(value => value === item.recommendation) ?? null };
  }
  const scan = data.scan ? record(data.scan) : {};
  return {
    phase, question, revision: data.revision, scope: typeof data.scope === 'string' ? data.scope : null,
    earlyBranch: typeof data.earlyBranch === 'string' ? data.earlyBranch : null,
    files: count(data.files), gaps: count(data.gaps), deferred: count(data.deferred), manualPending: count(data.manualPending),
    skippedGenerated: count(data.skippedGenerated ?? scan.skippedGenerated),
    scanPhase: SCAN_PHASES.find(item => item === scan.status) ?? null,
    scanSkippedGenerated: count(scan.skippedGenerated),
    progress: parseInventoryProgress(scan), scanProblem: typeof scan.error === 'string' ? scan.error : null, adaptive,
  };
}

export function AssistanceInterview({ currentPath, revision, scanSignal, readOnly, onEdit, controlRef,
  footerHost, inspectQuestion, browser, deferMarked, onOpenMirror }: {
  currentPath: string;
  revision: number;
  scanSignal: string | undefined;
  readOnly: boolean;
  onEdit: (operation: Operation, reviewedRevision: number) => Promise<void>;
  controlRef: UiControlRef;
  footerHost: HTMLDivElement | null;
  inspectQuestion: (path: string, kind: Question['kind']) => void;
  browser: NonNullable<UiControlRef['current']>;
  deferMarked: boolean;
  onOpenMirror: (path: string) => void;
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
  const [scanDetailsOpen, setScanDetailsOpen] = useState(readScanDetailsPreference);
  const [detailsOpen, setDetailsOpen] = useState(false);
  const inFlight = useRef(false);
  const heading = useRef<HTMLHeadingElement>(null);
  const override = useRef<HTMLFieldSetElement>(null);
  const overrideConfirm = useRef<HTMLButtonElement>(null);
  const reviewScroll = useRef<HTMLDivElement>(null);
  const question = interview?.question;
  const questionId = question?.id;
  const choosing = questionId !== undefined && overrideQuestion === questionId;
  const changedChoice = choosing && overrideDraft?.questionId === questionId ? overrideDraft.status : null;
  function changeScanDetails(open: boolean): void {
    setScanDetailsOpen(open);
    try { localStorage.setItem(SCAN_DETAILS_PREFERENCE, String(open)); }
    catch { setActionProblem('The scan details preference could not be saved for the next session.'); }
  }
  function returnToQuestion(): void {
    if (question) inspectQuestion(question.path, question.kind);
  }
  function compareManualBackup(target: Question): void {
    onOpenMirror(target.kind === 'file' ? parentPath(target.path) ?? target.path : target.path);
  }
  function chooseOverride(open: boolean): void {
    if (open && (!questionId || readOnly || reading || working || problem)) throw new Error('The current question is not available for override.');
    setOverrideQuestion(open ? questionId ?? null : null);
    setOverrideDraft(null);
    if (!open) heading.current?.focus();
  }
  controlRef.current = {
    read: () => ({ interview, reading, working, problem, actionProblem, scanDetailsOpen, detailsOpen,
      browser: browser.read(), overrideOpen: choosing, overrideDraft: changedChoice }),
    execute: async (command, request) => {
      if (command.action === 'setAssistanceScanDetails') {
        changeScanDetails(command.open === true);
        return;
      }
      if (command.action === 'setAssistanceDetails') {
        setDetailsOpen(command.open === true);
        return;
      }
      if (command.action === 'navigateAssistance') {
        if (command.mode === 'return') { returnToQuestion(); return; }
        if (command.mode === 'compare' && question?.manualConfirmation) { compareManualBackup(question); return; }
        await browser.execute(command, request);
        return;
      }
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
        if (external || result.scanPhase === 'running' || result.phase === 'scanning' || result.phase === 'other-scan') {
          timer = setTimeout(read, external ? 1600 : 1200);
        }
      } catch (error) {
        if (active) setProblem(String(error));
        if (active && external) timer = setTimeout(read, 2200);
      } finally {
        if (active) setReading(false);
      }
    }
    void read();
    return () => { active = false; clearTimeout(timer); };
  }, [revision, reload, scanSignal]);

  useEffect(() => {
    if (questionId) {
      reviewScroll.current?.scrollTo({ top: 0 });
      heading.current?.focus({ preventScroll: true });
      if (question) inspectQuestion(question.path, question.kind);
    }
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

  function scan(mode: 'current' | 'pause' | 'resume', includeGenerated = false): void {
    if (mode === 'current' && !interview?.scope) return;
    const path = interview?.scope ?? currentPath;
    void perform(() => invoke('scan', { mode, path, includeGenerated }));
  }

  const locked = readOnly || reading || working;
  const phase = interview?.phase;
  const scope = interview?.scope;
  const scopeCandidate = currentPath;
  const canChangeScope = scope?.toLowerCase() !== scopeCandidate.toLowerCase();
  const adaptive = interview?.adaptive;
  const external = adaptive?.mode === 'external';
  const limit = Number(questionLimit);
  const validLimit = Number.isSafeInteger(limit) && limit >= 1 && limit <= MAX_EXTERNAL_QUESTIONS;
  const externalWaiting = external && adaptive.phase !== 'question';
  const folderExclusionBlocked = question?.adaptive && question.kind === 'folder';
  const early = interview?.earlyBranch;
  const scanActive = interview?.scanPhase === 'running';
  const questionFolder = question?.kind === 'file' ? parentPath(question.path) : question?.path;
  const browsingElsewhere = questionFolder && reviewPathKey(questionFolder) !== reviewPathKey(currentPath);
  const scanRunning = interview?.scanPhase === 'running';
  const canScan = Boolean(scope) && !scanRunning && (phase === 'needs-scan' || phase === 'complete' || Boolean(problem));
  const canIncludeGenerated = Boolean(scope) && !scanRunning &&
    (phase === 'needs-scan' || phase === 'complete' || (phase === 'question' && Boolean(interview?.skippedGenerated)));
  const stage = working ? 'Updating review' : problem || actionProblem ? 'Needs attention'
    : early && scanActive ? (question ? 'Question ready · scan running' : 'Branch ready · scan running')
    : reviewStage(interview);
  const showDetails = detailsOpen || choosing || Boolean(problem || actionProblem);
  const approveLabel = question?.manualConfirmation ? 'Compare manual backup' : question?.adaptive
    ? `Approve keep · ${LABELS[question.status]}`
    : question ? `Approve ${decisionChoiceLabel(question.status)}` : 'Approve';
  const scopeActions = <div className="assistance-actions assistance-scope-actions">
    {(phase === 'idle' || problem || canChangeScope) && <button disabled={locked}
      onClick={() => edit({ type: 'assist', action: 'scope', path: scopeCandidate })}>
      {scope ? 'Use current folder' : 'Review this folder'}
    </button>}
    {canScan && <button disabled={locked} onClick={() => scan('current')}>
      <ScanSearch size={16} aria-hidden="true"/>{phase === 'needs-scan' ? 'Scan this folder' : 'Scan again'}
    </button>}
    {canIncludeGenerated && <button disabled={locked} onClick={() => scan('current', true)}>
      Scan including generated folders
    </button>}
    {Boolean(interview?.deferred) && <button disabled={locked} onClick={() => edit({ type: 'assist', action: 'restore' })}>
      Review deferred items
    </button>}
    {problem && <button disabled={reading || working} onClick={() => setReload(value => value + 1)}>Refresh interview</button>}
  </div>;

  return <section id="inline-assistance" className="assistance-interview inline-assistance" aria-label="AI Assistance" aria-busy={reading || working}>
    <div className="assistance-topline">
      <div className="assistance-scope">
        {question ? <h3 id="assistance-question-title" ref={heading} tabIndex={-1}>{question.prompt}</h3>
          : <strong>AI Assistance</strong>}
        <span title={scope ?? currentPath}>{scope ? 'Review scope' : 'Current folder'} · {scope ?? currentPath}</span>
      </div>
      <div className="assistance-top-actions">
        <div className="assistance-stage" role="status" data-stage={question ? 'ready' : phase}>
          <span className="assistance-stage-dot" aria-hidden="true"/>
          <span>{stage}</span>
        </div>
        <button aria-expanded={showDetails} aria-controls="assistance-review-details"
          disabled={choosing || Boolean(problem || actionProblem)} onClick={() => setDetailsOpen(open => !open)}>Details</button>
      </div>
    </div>
    {question && !showDetails && <p className="assistance-inline-reason" title={question.reason}>
      <strong>{decisionChoiceLabel(question.status === 'manual-confirmed' ? 'manual-pending' : question.status)}</strong> · {question.reason}
    </p>}
    <div id="assistance-review-details" className="assistance-content-frame" hidden={!showDetails}>
    <div className="assistance-scroll" ref={reviewScroll} tabIndex={0} role="region" aria-label="Folder review">
    <div className="assistance-content">
    {problem && <p role="alert" className="assistance-error">{problem}</p>}
    {actionProblem && <p role="alert" className="assistance-error">{actionProblem}</p>}
    {(phase === 'idle' || problem) && <p>Choose a folder to review. The interview uses inventoried metadata to ask one question at a time.</p>}
    {phase === 'needs-scan' && <p>The interview starts after the folder inventory finishes. The standard scan skips known generated folders.</p>}
    {phase === 'needs-scan' && interview?.scanProblem && <p role="alert" className="assistance-error">{interview.scanProblem}</p>}
    {phase === 'scanning' && <p className="assistance-waiting">Questions appear as soon as a finished branch has enough coverage to review.</p>}
    {phase === 'other-scan' && <p>Another folder is being inventoried. You can start this scope when that scan stops.</p>}
    {phase === 'paused' && <p>The inventory is paused. Resume it to continue the interview.</p>}
    {question && <section className="assistance-question" aria-labelledby="assistance-question-title">
      <div className="assistance-recommendation">
        <span><Sparkles size={15} aria-hidden="true"/>{question.manualConfirmation
          ? 'Manual backup follow-up' : question.source === 'codex' ? 'AI summary' : 'Review summary'}</span>
        <StatusBadge effective={{ ...EMPTY_EFFECTIVE, status: question.manualConfirmation ? 'manual-pending' : question.status }}/>
      </div>
      <p className="assistance-path" title={question.path}>{question.path}</p>
      <p className="assistance-reason">{question.reason}</p>
      {question.recommendation && question.recommendation !== 'keep' &&
        <p>{question.recommendation === 'review-children' ? 'Codex suggests reviewing children.' :
          question.recommendation === 'defer' ? 'Codex suggests leaving this question for later.' : 'Codex asks whether this data can be recreated.'}</p>}
      {question.agentStatus && question.agentStatus !== question.status && <p>
        Codex suggested {LABELS[question.agentStatus]}. Keeping is the default until you explicitly override it.
      </p>}
      {question.kind === 'folder' && !question.manualConfirmation &&
        <p>Explicit child decisions still apply. New entries can inherit this folder’s decision.</p>}
      {question.manualConfirmation
        ? <p>{question.kind === 'file' ? 'Compare its parent folder with the manual backup to verify this file.' : 'Compare this folder with its manual backup to verify the match.'}</p>
        : <p>If Auto-copy is on, Migrate now can enter the copy queue.</p>}
      {question.early && <p>This question covers one inventoried branch. Choose a keep decision or Manual backup.</p>}
      {choosing ? <fieldset ref={override} className="assistance-overrides">
        <legend>Choose a different decision</legend>
        <p>Choose To Redownload only if you can recreate what you need. To be ERASED excludes the item from migration.</p>
        {folderExclusionBlocked && <p>Review children before excluding this folder’s contents.</p>}
        <p>Manual backup records your plan. A matching mirror comparison can verify it.</p>
        {DECISION_CHOICES.filter(status => status !== 'attention' && status !== question.status &&
          (!question.early || status === 'migrate-now' || status === 'last-moment' || status === 'manual-pending') &&
          !(question.manualConfirmation && status === 'manual-pending')).map(status =>
          <button key={status} aria-pressed={question.adaptive ? changedChoice === status : undefined}
            disabled={locked || Boolean(problem) || Boolean(folderExclusionBlocked && (status === 'redownload' || status === 'erase'))}
            onClick={() => answer(status)}>
            <span className={`decision-dot status-${status}`} aria-hidden="true"/>{decisionChoiceLabel(status)}
          </button>)}
      </fieldset> : null}
    </section>}
    {phase === 'complete' && interview && !externalWaiting && <p role="status">{interview.gaps || interview.deferred || interview.manualPending
      ? 'The interview has no more eligible questions. Pending manual backups, coverage gaps and deferred items still need attention.'
      : 'The interview is complete for this inventory.'}</p>}
    {interview && (phase === 'question' || phase === 'complete' || external) && <p className="assistance-coverage">
      {interview.files.toLocaleString()} files inventoried {early ? 'in the ready branch' : 'across this scope'}
      {interview.gaps > 0 && <> · {interview.gaps.toLocaleString()} coverage gaps</>}
      {interview.skippedGenerated > 0 && <> · {interview.skippedGenerated.toLocaleString()} generated folders skipped</>}
      {interview.deferred > 0 && <> · {interview.deferred.toLocaleString()} deferred</>}
      {interview.manualPending > 0 && <> · {interview.manualPending.toLocaleString()} manual backups pending</>}
    </p>}
    {external && adaptive?.phase === 'unavailable' && Boolean(interview?.skippedGenerated) &&
      <p>External questions require an inventory that includes generated folders in this scope. Use local questions or choose a smaller folder.</p>}
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
    {question && scopeActions}
    {readOnly && <p>Recover the saved journal before changing interview choices.</p>}
    </div>
    </div>
    <OverflowIndicators targetRef={reviewScroll} axis="vertical"/>
    </div>
    {footerHost && createPortal(<div className="assistance-footer" aria-label="Assistance and inventory controls">
      <InventoryDock interview={interview} detailsOpen={scanDetailsOpen} locked={locked}
        deferMarked={deferMarked} onPriorityChange={value => edit({ type: 'inventoryPriority', deferMarked: value })}
        onDetailsChange={changeScanDetails} onScanControl={scan}/>
      {!question ? scopeActions : choosing ? <div className="assistance-actions assistance-review-actions">
        <button onClick={() => chooseOverride(false)}>Back</button>
        {changedChoice && <>
          <span>Apply {LABELS[changedChoice]} to this exact path</span>
          <button ref={overrideConfirm} className="primary" disabled={locked || Boolean(problem)}
            onClick={() => answer(changedChoice, true)}>Confirm override</button>
        </>}
      </div> : <div className="assistance-actions assistance-review-actions">
        {browsingElsewhere && <button onClick={returnToQuestion}>Show question folder</button>}
        {question.kind === 'folder' && !question.manualConfirmation && <button disabled={locked || Boolean(problem)}
          onClick={() => edit({ type: 'assist', action: 'split', path: question.path, questionId: question.id })}>
          <FolderTree size={16} aria-hidden="true"/> Review children
        </button>}
        <button disabled={locked || Boolean(problem)} onClick={() => chooseOverride(true)}>Override</button>
        <button disabled={locked || Boolean(problem)}
          onClick={() => edit({ type: 'assist', action: 'defer', path: question.path, questionId: question.id })}>Skip for now</button>
        <button className="primary" aria-label={approveLabel} title={approveLabel} disabled={locked || Boolean(problem)}
          onClick={() => { if (question.manualConfirmation) compareManualBackup(question); else answer(question.status); }}>
          <Check size={16} aria-hidden="true"/>{question.manualConfirmation ? 'Compare backup' : 'Approve'}
        </button>
      </div>}
    </div>, footerHost)}
  </section>;
}

function reviewStage(interview: Interview | null): string {
  if (!interview) return 'Preparing review';
  if (interview.question) return 'Question ready';
  if (interview.phase === 'idle') {
    if (interview.scanPhase === 'running') return 'Background inventory running';
    if (interview.scanPhase === 'paused') return 'Background inventory paused';
    return 'Choose a folder';
  }
  if (interview.phase === 'scanning') return 'Scanning review scope';
  if (interview.phase === 'other-scan') return 'Another folder is scanning';
  if (interview.phase === 'paused') return 'Scan paused';
  if (interview.phase === 'needs-scan') return 'Waiting for inventory';
  if (interview.adaptive.mode === 'external') {
    switch (interview.adaptive.phase) {
      case 'waiting-model': return 'Waiting for Codex';
      case 'revalidate': return 'Revalidation needed';
      case 'limit': return 'Question limit reached';
      case 'unavailable': return 'External questions unavailable';
    }
  }
  return interview.gaps || interview.deferred || interview.manualPending
    ? 'Questions finished · items remain' : 'Review complete';
}

function InventoryDock({ interview, detailsOpen, locked, deferMarked, onPriorityChange, onDetailsChange, onScanControl }: {
  interview: Interview | null;
  detailsOpen: boolean;
  locked: boolean;
  deferMarked: boolean;
  onPriorityChange: (value: boolean) => void;
  onDetailsChange: (open: boolean) => void;
  onScanControl: (mode: 'pause' | 'resume') => void;
}) {
  const phase = interview?.scanPhase;
  const progress = interview?.progress;
  const detailScroll = useRef<HTMLDivElement>(null);
  const [nowMs, setNowMs] = useState(() => Date.now());
  const tracking = detailsOpen && (phase === 'running' || phase === 'paused');
  useEffect(() => {
    if (!tracking) return;
    setNowMs(Date.now());
    const timer = setInterval(() => setNowMs(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [tracking]);
  const elapsed = tracking ? inventoryElapsed(progress?.startedAt ?? null, nowMs) : null;
  const label = phase === 'running' ? 'Discovering folders' : phase === 'paused' ? 'Inventory paused'
    : phase === 'complete' ? 'Inventory finished' : phase === 'error' ? 'Inventory needs attention' : 'Metadata inventory idle';
  return <section className="assistance-scan-dock" aria-label="Background inventory">
    <div className="assistance-scan-main">
      <div className="assistance-scan-caption">
        <strong>{label}</strong>
        {progress && phase && <span>{progress.processed.toLocaleString()} folders
          {phase === 'running' && progress.workers > 0 && <> · {progress.workers} workers</>}
        </span>}
      </div>
      <div className="assistance-scan-controls">
        <button aria-expanded={detailsOpen} aria-controls="assistance-scan-details"
          onClick={() => onDetailsChange(!detailsOpen)}>
          Scan details <ChevronDown size={14} aria-hidden="true"/>
        </button>
        {phase === 'running' && <button disabled={locked} onClick={() => onScanControl('pause')}>
          <Pause size={14} aria-hidden="true"/>Pause scan
        </button>}
        {phase === 'paused' && Boolean(progress?.pending) && <button disabled={locked} onClick={() => onScanControl('resume')}>
          <Play size={14} aria-hidden="true"/>Resume scan
        </button>}
      </div>
    </div>
    {phase === 'running' && progress && <progress className="assistance-progress-bar"
      aria-label="Metadata inventory" aria-valuetext={inventoryProgressLabel(progress)}/>}
    {phase === 'complete' && <progress className="assistance-progress-bar" max={1} value={1}
      aria-label="Metadata inventory finished"/>}
    {phase === 'paused' && <div className="assistance-paused-track" aria-hidden="true"/>}
    <div id="assistance-scan-details" className="assistance-scan-details-frame" hidden={!detailsOpen}>
      {detailsOpen && <>
        <div className="assistance-scan-details" ref={detailScroll} tabIndex={0} role="region" aria-label="Inventory details">
          <label className="assistance-inventory-priority">
            <input type="checkbox" checked={deferMarked} disabled={locked} onChange={event => onPriorityChange(event.target.checked)}/>
            Scan marked folders last
          </label>
          <div>
            {progress && phase ? <>
              <dl>
                <div><dt>Folders processed</dt><dd>{progress.processed.toLocaleString()}</dd></div>
                <div><dt>Queued</dt><dd>{progress.pending?.toLocaleString() ?? 'Unknown'}</dd></div>
                <div><dt>Files found</dt><dd>{progress.files.toLocaleString()}</dd></div>
                <div><dt>Logical size</dt><dd>{readableBytes(progress.bytes)}</dd></div>
                <div><dt>Active folders</dt><dd>{progress.active.toLocaleString()}</dd></div>
                <div><dt>Workers</dt><dd>{progress.workers.toLocaleString()}</dd></div>
                <div><dt>Generated skipped</dt><dd>{(interview?.scanSkippedGenerated ?? 0).toLocaleString()}</dd></div>
                <div><dt>Coverage gaps</dt><dd>{progress.gaps.toLocaleString()}</dd></div>
              </dl>
              {progress.currentPath && <p className="assistance-progress-path">
                <span>Current path</span> {progress.currentPath}
              </p>}
              <p className="assistance-scan-note">
                {elapsed && <>{elapsed} since scan started · </>}
                {phase === 'running' ? 'Final total and finish time are unknown during discovery.' : 'Metadata inventory.'}
              </p>
              {phase === 'error' && interview?.scanProblem && <p className="assistance-error">{interview.scanProblem}</p>}
            </> : <p>Start a metadata scan to see folder counts and coverage here.</p>}
          </div>
        </div>
        <OverflowIndicators targetRef={detailScroll} axis="vertical"/>
      </>}
    </div>
  </section>;
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
