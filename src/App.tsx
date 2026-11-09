import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import type { CSSProperties, KeyboardEvent as ReactKeyEvent } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { listen } from '@tauri-apps/api/event';
import {
  ArrowDownToLine, ArrowLeft, ArrowRight, Check, CheckCircle2,
  ChevronLeft, ChevronRight, Clock3, Database, Download, ExternalLink,
  File, Folder, FolderOpen, HardDrive, History, Home, Info, Link2, ListFilter,
  LoaderCircle, Map as MapIcon, Pause, Play, RefreshCw, Search, ShieldCheck, Sparkles,
  TriangleAlert, Undo2, X, CircleHelp, Trash2, ChevronsDown, ChevronsUp,
} from 'lucide-react';
import {
  MutationQueue, STATUSES, baseName, parentPath, parseDetails, parseDocument,
  parseEntry, parseView, readableBytes, record, normalizeReviewPath, isStatus, resolveDecisionPath, parseGateway, previewBranch, closePreviewBranch,
} from './model';
import type {
  AppStatus, Boot, Decision, Details, Document, Effective, Entry, Listing,
  GroupTarget, Operation, QueueState, ScanStatus, Status, ViewState, Volume, GatewayStatus, PreviewColumn, ReviewLayout,
} from './model';
import { DECISION_CHOICES, DESCRIPTIONS, EMPTY_EFFECTIVE, ICONS, LABELS, decisionChoiceLabel, isDecisionChoiceSelected, reviewStatusLabel, RollupCoverageDetails, StatusBadge } from './classifications';
import { DirectoryColumn, PreviewFolderColumn, RowClassificationMenu, localRowSuggestion } from './FolderColumns';
import type { RowMenuTarget, RowSuggestion } from './FolderColumns';
import { OverflowIndicators } from './OverflowIndicators';
import { AuditControls, AuditCoverageDetails, AuditDetails, useAuditStatus } from './Audit';
import { NotificationBell } from './NotificationBell';
import { CopyQueue } from './CopyQueue';
import { AssistanceInterview } from './Assistance';
import { ResizeGrip, ReviewSidebar } from './ReviewLayout';
import { FolderReadCache } from './folder-read-cache';
import { centerOpenFolder, keepFolderOnBack } from './folder-column-navigation';
import { useFolderColumnMotion } from './folder-column-motion';
import { isStaleRead, reviewContentKey } from './read-cache';
import { auditRefreshKey } from './audit-model';
import './copy-queue.css';
import './review-layout.css';
import { isCaretBoundary, ReviewFilter, useReviewKeyboardFocus } from './ReviewKeyboard';
import type { ReviewFilterHandle } from './ReviewKeyboard';
import {
  isRestoredScroll, isReviewRowVisible, PULSE_MS, restoreReviewScroll, reviewPathKey, useReviewActivity, useReviewSurface,
} from './ReviewActivity';
import type { ReviewActivity } from './ReviewActivity';
import { useUiBridge } from './UIBridge';
import type { UiParticipant, UiCommand, UiRequest } from './UIBridge';
import { inspectPending, pendingFingerprint, pendingMetadata, textHash } from './pending-inspection';
import type { PendingSnapshot } from './pending-inspection';
import { assertNewCloseAllowed, CloseSession } from './close-session';
import type { CloseState } from './close-session';
import { SelectionReview } from './SelectionReview';
import { MirrorReview } from './MirrorReview';
import type { GroupOperation, GroupReview } from './SelectionReview';
import {
  changeSelection, EMPTY_SELECTION, latestUndoEvent, loadSelection, reportSelectionTarget, replaceSelection,
  resolvedSelection, rootForSelection, savedGroup, selectedFor, selectionKey, selectionSignature, selectionTarget,
} from './review-selection';
import type { SavedGroup, SelectionSets } from './review-selection';

const EMPTY_STATUS: AppStatus = { scan: { pending: 0 }, gaps: [], decisionCount: 0 };
const FOLLOW_PREFERENCE = 'migration-master.follow-ai';
const SELECTION_PREFERENCE = 'migration-master.group-selection-v1';

function readSavedSelection(): SelectionSets {
  try { return loadSelection(localStorage.getItem(SELECTION_PREFERENCE)); }
  catch { return new Map(); }
}

function readFollowPreference(): boolean {
  try { return localStorage.getItem(FOLLOW_PREFERENCE) === 'true'; }
  catch { return false; }
}

function isEditingField(): boolean {
  const focus = window.document.activeElement;
  if (!(focus instanceof HTMLElement)) return false;
  if (focus instanceof HTMLInputElement) return !['button', 'checkbox', 'radio', 'submit'].includes(focus.type);
  return focus.isContentEditable || ['TEXTAREA', 'SELECT'].includes(focus.tagName);
}
function savedViewFields(view: ViewState): Partial<ViewState> {
  const { path, selectedPath, search, filter, page, scrollTop, recentPaths, previewColumns, previewScrollLeft, browserColumnWidth, revealPath, layout } = view;
  return { path, selectedPath, search, filter, page, scrollTop, recentPaths, previewColumns, previewScrollLeft, browserColumnWidth, revealPath, layout };
}

function parseBoot(value: unknown): Boot {
  const b = record(value);
  if (!Array.isArray(b.volumes) || typeof b.dataPath !== 'string') throw new Error('The native app could not identify its data file.');
  const volumes: Volume[] = b.volumes.map(value => {
    const v = record(value);
    if (typeof v.root !== 'string' || typeof v.id !== 'string' || typeof v.label !== 'string' || typeof v.available !== 'boolean') throw new Error('Invalid native volume response.');
    return { root: v.root, id: v.id, label: v.label, available: v.available, problem: typeof v.problem === 'string' ? v.problem : null };
  });
  return { document: parseDocument(b.document), volumes, dataPath: b.dataPath, fixture: b.fixture === true, problem: typeof b.problem === 'string' ? b.problem : null, recoveryAvailable: b.recoveryAvailable === true, gateway: parseGateway(b.gateway) };
}
function parseAppStatus(value: unknown): AppStatus {
  const v = record(value);
  const s = record(v.scan);
  const number = (v: unknown): number => typeof v === 'number' ? v : 0;
  const scan: ScanStatus = { status: typeof s.status === 'string' ? s.status : undefined, pending: number(s.pending), processed: number(s.processed), files: number(s.files), bytes: number(s.bytes), skippedGenerated: number(s.skippedGenerated), currentPath: typeof s.currentPath === 'string' ? s.currentPath : undefined, error: typeof s.error === 'string' ? s.error : undefined };
  const gaps = Array.isArray(v.gaps) ? v.gaps.map(value => {
    const g = record(value);
    return { path: String(g.path ?? ''), problem: String(g.problem ?? ''), observedAt: String(g.observedAt ?? '') };
  }) : [];
  return { scan, gaps, decisionCount: number(v.decisionCount),
    mirrorEvidenceStamp: typeof v.mirrorEvidenceStamp === 'string' ? v.mirrorEvidenceStamp : undefined };
}
export default function App() {
  const [boot, setBoot] = useState<Boot | null>(null);
  const [document, setDocument] = useState<Document | null>(null);
  const [view, setView] = useState<ViewState | null>(null);
  const [listing, setListing] = useState<Listing | null>(null);
  const [details, setDetails] = useState<Details | null>(null);
  const [appStatus, setAppStatus] = useState<AppStatus>(EMPTY_STATUS);
  const [saveState, setSaveState] = useState<QueueState>({ pending: 0, saving: false, error: null });
  const [buffered, setBuffered] = useState(false);
  const [fatalError, setFatalError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [detailsLoading, setDetailsLoading] = useState(false);
  const [dataTick, setDataTick] = useState(0);
  const [auditDataTick, setAuditDataTick] = useState(0);
  const [refreshTick, setRefreshTick] = useState(0);
  const [pathDraft, setPathDraft] = useState('');
  const [notes, setNotes] = useState<Record<string, string>>({});
  const [showGaps, setShowGaps] = useState(false);
  const [includeGeneratedForIndex, setIncludeGeneratedForIndex] = useState(false);
  const [showHistory, setShowHistory] = useState(false);
  const [showDecisionPicker, setShowDecisionPicker] = useState(false);
  const [showContext, setShowContext] = useState(false);
  const [showMcp, setShowMcp] = useState(false);
  const [closeState, setCloseState] = useState<CloseState>({ phase: 'idle', ticket: null });
  const [lifecycleSnapshot, setLifecycleSnapshot] = useState<Record<string, unknown> | null>(null);
  const [gatewayDetailsOpen, setGatewayDetailsOpen] = useState(false);
  const [showAssistance, setShowAssistance] = useState(false);
  const [assistanceFooter, setAssistanceFooter] = useState<HTMLDivElement | null>(null);
  const [showMirror, setShowMirror] = useState(false);
  const [mirrorReviewPath, setMirrorReviewPath] = useState<string | null>(null);
  const [observedRevision, setObservedRevision] = useState(0);
  const observedSuggestions = useRef<Pick<Document, 'revision' | 'suggestions'> | null>(null);
  const [showConflict, setShowConflict] = useState(false);
  const [conflictLoading, setConflictLoading] = useState(false);
  const [conflictProblem, setConflictProblem] = useState<string | null>(null);
  const [gateway, setGateway] = useState<GatewayStatus>({ state: 'unavailable' });
  const [externalDocument, setExternalDocument] = useState<Document | null>(null);
  const [showSaved, setShowSaved] = useState(false);
  const [savedSearch, setSavedSearch] = useState('');
  const [savedFilter, setSavedFilter] = useState('all');
  const [savedPage, setSavedPage] = useState(0);
  const [knownSearch, setKnownSearch] = useState(false);
  const [knownResults, setKnownResults] = useState<string[] | null>(null);
  const [message, setMessage] = useState<string | null>(null);
  const [rowMenu, setRowMenu] = useState<RowMenuTarget | null>(null);
  const [selectedEntryInfo, setSelectedEntryInfo] = useState<Entry | null>(null);
  const [followAi, setFollowAi] = useState(readFollowPreference);
  const [revealVersion, setRevealVersion] = useState(0);
  const [selections, setSelections] = useState(readSavedSelection);
  const [displayedRows, setDisplayedRows] = useState<ReadonlyMap<number, readonly string[]>>(() => new Map());
  const [groupReview, setGroupReview] = useState<GroupReview | null>(null);
  const [groupAttempt, setGroupAttempt] = useState<GroupOperation | null>(null);
  const [groupBusy, setGroupBusy] = useState(false);
  const [groupProblem, setGroupProblem] = useState<string | null>(null);
  const selectionSets = useRef(selections);
  const activeGroup = useRef<GroupReview | null>(null);
  const attemptedGroup = useRef<GroupOperation | null>(null);
  const submittingGroup = useRef(false);
  const selectionHash = useRef<{ signature: string; promise: Promise<string> } | null>(null);
  const rowMenuRef = useRef<RowMenuTarget | null>(null);
  const followEnabled = useRef(followAi);
  const localFollowView = useRef(false);
  const followPending = useRef<ReviewActivity | null>(null);
  const followPlanning = useRef(false);
  const pendingExplicitReveal = useRef<ReviewActivity | null>(null);
  const pollExternalNow = useRef<() => void>(() => {});
  const holdMarkRowsUntil = useRef(0);
  const externalRefreshTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const queue = useRef<MutationQueue | null>(null);
  const nativeWriteLane = useRef<Promise<void>>(Promise.resolve());
  const draftGeneration = useRef(0);
  const fingerprintCache = useRef<{ writer: MutationQueue | null; version: number; draftVersion: number; path: string; promise: Promise<string> } | null>(null);
  const copyControl = useRef<UiParticipant | null>(null);
  const notificationControl = useRef<UiParticipant | null>(null);
  const assistanceControl = useRef<UiParticipant | null>(null);
  const mirrorControl = useRef<UiParticipant | null>(null);
  const documentRef = useRef<Document | null>(null);
  const conflictReviewOpen = useRef(false);
  const viewRef = useRef<ViewState | null>(null);
  const viewBuffer = useRef<Partial<ViewState>>({});
  const noteBuffer = useRef(new Map<string, string>());
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const appRef = useRef<HTMLDivElement>(null);
  const pathInputRef = useRef<HTMLInputElement>(null);
  const toolbarRef = useRef<HTMLDivElement>(null);
  const toolbarFocus = useRef<'path' | 'search' | 'compact' | 'expanded' | null>(null);
  const compactTrigger = useRef<HTMLButtonElement>(null);
  const expandedTrigger = useRef<HTMLButtonElement>(null);
  const restoreColumnsRef = useRef<HTMLButtonElement>(null);
  const columnFocus = useRef<number | 'restore' | null>(null);
  const columnStripRef = useRef<HTMLDivElement>(null);
  const searchRef = useRef<HTMLInputElement>(null);
  const knownButtonRef = useRef<HTMLButtonElement>(null);
  const filterRef = useRef<ReviewFilterHandle>(null);
  const driveRegionRef = useRef<HTMLDivElement>(null);
  const keyboard = useReviewKeyboardFocus(columnStripRef);
  const breadcrumbRef = useRef<HTMLElement>(null);
  const revealPreviewDepth = useRef<number | null>(null);
  const [folderReads] = useState(() => new FolderReadCache());
  const mounted = useRef(true);
  const closeSession = useRef<CloseSession | null>(null);
  const loadedDetailsPath = useRef<string | null>(null);
  const loadedListingContext = useRef<string | null>(null);
  const activeAuditReads = useRef(0);
  const selectedPath = view?.selectedPath ?? view?.path ?? null;
  const previewBranchKey = view?.previewColumns.map(column => column.path).join('\u0000');
  const toolbar = view?.layout?.toolbar ?? 'expanded';
  const requestedFill = view?.layout?.fillDepth;
  const fillDepth = requestedFill != null && requestedFill <= (view?.previewColumns.length ?? 0)
    ? requestedFill : null;
  const readOnly = Boolean(boot?.problem) || closeState.phase !== 'idle';
  const selectionRoot = rootForSelection(view?.path, boot?.volumes ?? []);
  const groupTargets = useMemo(() => selectionRoot
    ? resolvedSelection(selectedFor(selections, selectionRoot.id), selectionRoot) : EMPTY_SELECTION, [selections, selectionRoot]);
  const groupKeys = useMemo(() => new Set(groupTargets.map(selectionKey)), [groupTargets]);
  const visibleGroupCount = useMemo(() => {
    const columns = fillDepth === null ? Array.from(displayedRows.values()) : [displayedRows.get(fillDepth) ?? []];
    const paths = new Set(columns.flat().map(reviewPathKey));
    return groupTargets.filter(target => paths.has(reviewPathKey(target.path))).length;
  }, [displayedRows, fillDepth, groupTargets]);
  const onRowsDisplayed = useCallback((depth: number, _folder: string, paths: readonly string[] | null): void => {
    setDisplayedRows(previous => {
      if (!paths && !previous.has(depth)) return previous;
      const next = new Map(previous);
      if (paths) next.set(depth, paths);
      else next.delete(depth);
      return next;
    });
  }, []);
  const audit = useAuditStatus(Boolean(boot), () => {
    if (activeAuditReads.current > 0 || Date.now() < holdMarkRowsUntil.current || isEditingField() || rowMenuRef.current || window.document.querySelector('dialog[open], [data-notifications-preview="true"]')) return false;
    setAuditDataTick(value => value + 1);
    return true;
  });
  const reviewContent = useMemo(() => reviewContentKey(document), [document]);
  // Observe watcher/audit changes even while visible-row refresh is deferred during
  // editing. A later navigation must never reuse the superseded snapshot.
  const folderReadScope = JSON.stringify([boot?.dataPath, boot?.volumes, reviewContent,
    dataTick, refreshTick, appStatus.scan.error, audit.problem, appStatus.mirrorEvidenceStamp]);
  // A source event or missed-watcher interval requires fresh immediate metadata,
  // while hashes and decision edits only need the derived listing recomputed.
  const folderSourceScope = JSON.stringify([boot?.dataPath, boot?.volumes,
    audit.status?.watcherChangeSequence, audit.status?.reconciliationRequired, audit.problem]);
  useLayoutEffect(() => {
    folderReads.configure({ derived: folderReadScope, source: folderSourceScope });
  }, [folderReads, folderReadScope, folderSourceScope]);
  const folderProgressScope = JSON.stringify([appStatus.scan.status, appStatus.scan.processed, appStatus.scan.files,
    appStatus.scan.bytes, appStatus.scan.pending, audit.status ? auditRefreshKey(audit.status) : null]);
  useLayoutEffect(() => { folderReads.expireValues(); }, [folderReads, folderProgressScope]);
  const folderRefreshToken = JSON.stringify([folderReadScope, folderSourceScope, auditDataTick, appStatus.scan.status]);
  const reviewActivity = useReviewActivity(Boolean(boot), receiveReviewActivity);
  useReviewSurface(columnStripRef, view && document ? { view, appliedRevision: document.revision, followAi, following: localFollowView.current, knownSearch, loading } : null);

  function canAdoptReview(): boolean {
    const hasBuffers = noteBuffer.current.size > 0 || Object.keys(viewBuffer.current).length > 0 || timer.current !== null;
    return queue.current?.idle === true && !hasBuffers && !isEditingField() && !rowMenuRef.current &&
      !appRef.current?.hasAttribute('data-resize-active') &&
      !window.document.querySelector('dialog[open], [data-notifications-preview="true"]');
  }

  function applyDisplayedView(next: ViewState, transient: boolean, revealAgain = false): void {
    const previous = viewRef.current;
    if (previous) prepareFolderMotion(previous, next);
    localFollowView.current = transient;
    revealPreviewDepth.current = null;
    viewRef.current = next;
    setView(next);
    if (previous?.path !== next.path) setPathDraft(current => current === previous?.path ? next.path : current);
    if (next.revealPath && (revealAgain || previous?.revealPath !== next.revealPath)) setRevealVersion(value => value + 1);
    setSelectedEntryInfo(null);
    setKnownResults(null);
    setKnownSearch(false);
  }

  function changeFollowAi(checked: boolean): void {
    followEnabled.current = checked;
    followPending.current = null;
    setFollowAi(checked);
    try { localStorage.setItem(FOLLOW_PREFERENCE, String(checked)); }
    catch { setError('Follow AI changed for this session, but the preference could not be saved.'); }
  }

  function refreshAfterExternalChange(): void {
    if (externalRefreshTimer.current !== null) clearTimeout(externalRefreshTimer.current);
    function refreshWhenReady(): void {
      const delay = holdMarkRowsUntil.current - Date.now();
      if (delay > 0) {
        externalRefreshTimer.current = setTimeout(refreshWhenReady, delay);
        return;
      }
      externalRefreshTimer.current = null;
      if (mounted.current) setDataTick(value => value + 1);
    }
    refreshWhenReady();
  }

  function receiveReviewActivity(items: ReviewActivity[]): void {
    const current = viewRef.current;
    if (!current) return;
    let newRevision = false;
    for (const item of items) {
      const committedReveal = item.kind === 'reveal' && item.revision !== undefined;
      if (item.kind === 'mark' && current.filter !== 'all' && isReviewRowVisible(columnStripRef.current, item.path)) {
        holdMarkRowsUntil.current = Math.max(holdMarkRowsUntil.current, Date.now() + PULSE_MS);
      }
      if (committedReveal) {
        pendingExplicitReveal.current = item;
        followPending.current = null;
      } else if (followEnabled.current) followPending.current = item;
      if (item.revision !== undefined && item.revision > (queue.current?.revision ?? -1)) newRevision = true;
    }
    if (newRevision) pollExternalNow.current();
    const explicit = pendingExplicitReveal.current;
    if (explicit && explicit.revision !== undefined && explicit.revision <= (queue.current?.revision ?? -1) && canAdoptReview()) {
      pendingExplicitReveal.current = null;
      if (current.revealPath && reviewPathKey(current.revealPath) === reviewPathKey(explicit.path)) setRevealVersion(value => value + 1);
    }
    void attemptFollow();
  }

  async function attemptFollow(): Promise<void> {
    const item = followPending.current;
    const current = viewRef.current;
    const writer = queue.current;
    if (!followEnabled.current || !item || !current || !writer || readOnly || followPlanning.current || !canAdoptReview()) return;
    if (Date.now() - item.atMs > 60_000) { followPending.current = null; return; }
    if (item.revision !== undefined && item.revision > writer.revision) return;
    if (item.kind === 'mark' && Date.now() < holdMarkRowsUntil.current) return;
    const revision = writer.revision;
    followPlanning.current = true;
    try {
      const result = record(await invoke<unknown>('plan_review_reveal', { path: item.path }));
      if (!mounted.current || !followEnabled.current || followPending.current !== item ||
        viewRef.current !== current || writer.revision !== revision || !canAdoptReview()) return;
      if (typeof result.revision === 'number' && result.revision !== revision) {
        pollExternalNow.current();
        return;
      }
      if (typeof result.revealPath !== 'string' || reviewPathKey(result.revealPath) !== reviewPathKey(item.path)) {
        throw new Error('The native reveal plan returned a different path.');
      }
      const patch = record(result.patch);
      const next = parseView({ ...current, ...patch, search: current.search, filter: current.filter, revealPath: result.revealPath });
      followPending.current = null;
      applyDisplayedView(next, true, true);
    } catch {
      if (followPending.current === item) {
        followPending.current = null;
        if (mounted.current && followEnabled.current) setMessage(`AI activity could not reveal ${baseName(item.path)}.`);
      }
    } finally { followPlanning.current = false; }
  }

  function report(cause: unknown): void { if (mounted.current) setError(cause instanceof Error ? cause.message : String(cause)); }
  function receiveDocument(doc: Document, state: QueueState): void {
    if (!mounted.current) return;
    documentRef.current = doc;
    setDocument(doc);
    setSaveState(state);
  }
  async function loadConflictComparison(throwFailure = false): Promise<void> {
    setConflictLoading(true);
    setConflictProblem(null);
    try {
      const result = parseBoot(await invoke<unknown>('get_boot'));
      if (result.problem) throw new Error(result.problem);
      setExternalDocument(result.document);
    } catch (cause) { setConflictProblem(cause instanceof Error ? cause.message : String(cause)); if (throwFailure) throw cause; }
    finally { setConflictLoading(false); }
  }
  function openConflictReview(): void {
    void beginConflictReview(false);
  }
  async function beginConflictReview(throwFailure: boolean): Promise<void> {
    flushBuffers();
    conflictReviewOpen.current = true;
    setShowConflict(true);
    await loadConflictComparison(throwFailure);
  }
  function closeConflictReview(): void { conflictReviewOpen.current = false; setShowConflict(false); }
  async function applyReviewedPending(): Promise<void> {
    const writer = queue.current;
    if (!writer || !externalDocument || !writer.rebaseAfterReview(externalDocument)) {
      setConflictProblem('This comparison cannot be applied while a save is active or after the pending request changed. Refresh the comparison and review again.');
      throw new Error('The reviewed comparison is no longer applicable.');
    }
    writer.retry();
    try {
      await writer.flush();
      setExternalDocument(null);
      setConflictProblem(null);
      setError(null);
      setDataTick(value => value + 1);
      closeConflictReview();
      setMessage('Your reviewed pending edits are saved. Other saved fields were preserved.');
    } catch (cause) { setConflictProblem(`${String(cause)}. Your pending edits are still retained. Refresh the comparison before trying again.`); throw cause; }
  }
  function runNativeWrite<T>(operation: () => Promise<T>, savedDocument?: (result: T) => Document): Promise<T> {
    const task = nativeWriteLane.current.then(async () => {
      const writer = queue.current;
      if (!writer || readOnly) throw new Error('Recover the review data before starting a native write.');
      flushBuffers();
      await writer.flush();
      writer.holdForExternal();
      let adopted: Document | undefined;
      try {
        const result = await operation();
        adopted = savedDocument?.(result);
        return result;
      }
      finally {
        try {
          if (!adopted) {
            const latest = parseBoot(await invoke('get_boot'));
            if (latest.problem) throw new Error(latest.problem);
            adopted = latest.document;
          }
          writer.finishExternal(adopted);
          setObservedRevision(adopted.revision);
          setDataTick(value => value + 1);
        } catch (cause) { writer.failExternal(cause); throw cause; }
      }
    });
    nativeWriteLane.current = task.then(() => {}, () => {});
    return task;
  }
  function edit(operation: Operation): Promise<Document> {
    if (!queue.current || readOnly) return Promise.reject(new Error('The saved data must be recovered before editing.'));
    return queue.current.request(operation);
  }
  function saveMirror(operation: Extract<Operation, { type: 'mirrorBind' | 'mirrorConfirm' }>,
    expectedRevision: number, operationId: string): Promise<unknown> {
    return runNativeWrite(() => invoke('mutate', { expectedRevision, operationId, operation }), parseDocument);
  }
  async function editAssistance(operation: Operation, reviewedRevision: number): Promise<void> {
    const writer = queue.current;
    if (!writer || readOnly) throw new Error('The saved journal must be recovered before editing.');
    flushBuffers();
    await writer.flush();
    // Interview actions are fenced by a question ID. A superseded question is re-read
    // for review instead of being retained for automatic retry in the notes queue.
    const response = await invoke<unknown>('mutate', {
      expectedRevision: Math.max(writer.revision, reviewedRevision),
      operationId: crypto.randomUUID(),
      operation,
    });
    writer.acceptExternal(response);
    setDataTick(value => value + 1);
  }
  function flushBuffers(): void {
    draftGeneration.current += 1;
    if (timer.current) clearTimeout(timer.current);
    timer.current = null;
    const patch = viewBuffer.current;
    viewBuffer.current = {};
    if (Object.keys(patch).length) void edit({ type: 'view', patch }).catch(report);
    for (const [path, text] of noteBuffer.current) void edit({ type: 'note', path, notes: text }).catch(report);
    noteBuffer.current.clear();
    setBuffered(false);
  }
  function scheduleFlush(): void {
    draftGeneration.current += 1;
    setBuffered(true);
    // A fixed timer, rather than a trailing debounce, bounds the unsaved typing interval.
    if (!timer.current) timer.current = setTimeout(flushBuffers, 350);
  }
  function updateView(patch: Partial<ViewState>, immediate = false): void {
    const current = viewRef.current;
    if (!current) return;
    const previewChanged = patch.previewColumns && (patch.previewColumns.length !== current.previewColumns.length ||
      patch.previewColumns.some((column, index) => column.path !== current.previewColumns[index]?.path || column.page !== current.previewColumns[index]?.page));
    const semantic = previewChanged || patch.path !== undefined || patch.selectedPath !== undefined ||
      patch.search !== undefined || patch.filter !== undefined || patch.page !== undefined || patch.revealPath !== undefined;
    if (semantic) {
      followPending.current = null;
      pendingExplicitReveal.current = null;
      if (current.revealPath && patch.revealPath === undefined) patch = { ...patch, revealPath: null };
    }
    const next = { ...current, ...patch };
    if (previewChanged || patch.path !== undefined) prepareFolderMotion(current, next);
    viewRef.current = next;
    setView(next);
    if (localFollowView.current) {
      const resizedPreview = patch.previewColumns?.some((column, index) => column.width !== current.previewColumns[index]?.width);
      if (!semantic && !resizedPreview && patch.browserColumnWidth === undefined && patch.layout === undefined) return;
      localFollowView.current = false;
      patch = savedViewFields(next);
    }
    if (!readOnly) {
      viewBuffer.current = { ...viewBuffer.current, ...patch };
      if (immediate) flushBuffers(); else scheduleFlush();
    }
  }
  function setLayout(patch: Partial<ReviewLayout>): void {
    updateView({ layout: { ...viewRef.current?.layout, ...patch } }, true);
  }
  function setToolbar(mode: NonNullable<ReviewLayout['toolbar']>, focus = false): void {
    if (mode === toolbar) return;
    if (focus) toolbarFocus.current = mode;
    setLayout({ toolbar: mode });
  }
  function changeAssistance(open: boolean): void {
    if (open && window.innerHeight <= 740 && toolbar === 'expanded') setToolbar('compact');
    setShowAssistance(open);
  }
  function focusSearch(): void {
    if (toolbar === 'expanded' || toolbar === 'search') searchRef.current?.focus();
    else setToolbar('search', true);
  }
  function resizeColumn(depth: number, width: number, fill: boolean): void {
    if (fill) {
      columnFocus.current = 'restore';
      setLayout({ fillDepth: depth });
    } else if (depth === 0) updateView({ browserColumnWidth: width }, true);
    else changePreview(depth, { width });
  }
  function navigate(path: string, selection: string | null = null, keepFilter = false, movement?: 'center' | 'back'): string | null {
    const current = viewRef.current;
    if (!current) return null;
    flushBuffers();
    let normalized: string;
    try { normalized = normalizeReviewPath(path, boot?.volumes.map(v => v.root) ?? []); }
    catch (cause) { report(cause); return null; }
    const recent = [normalized, ...current.recentPaths.filter(item => reviewPathKey(item) !== reviewPathKey(normalized))].slice(0, 8);
    const branch = movement === 'center' ? centerOpenFolder(current, normalized)
      : movement === 'back' ? keepFolderOnBack(current, knownSearch) : {};
    const filter = keepFilter ? current.filter : 'all';
    const page = branch.page ?? 0;
    if (reviewPathKey(current.path) !== reviewPathKey(normalized)) {
      const cached = folderReads.peekRecentListing({ path: normalized, search: '', filter, page });
      setListing(cached ?? null);
      setLoading(cached === undefined);
    }
    revealPreviewDepth.current = null;
    setSelectedEntryInfo(null);
    dismissRowMenu(false);
    updateView({
      path: normalized, selectedPath: selection, search: '', filter, page, scrollTop: 0,
      recentPaths: recent, previewColumns: [], previewScrollLeft: 0,
      layout: { ...current.layout, fillDepth: movement ? null : current.layout?.fillDepth === 0 ? 0 : null },
      ...branch,
    }, true);
    setPathDraft(normalized);
    setKnownResults(null);
    setKnownSearch(false);
    setError(null);
    return normalized;
  }
  function navigateByKey(path: string): void {
    const folder = navigate(path, null, true);
    if (folder) keyboard.requestFocus({ depth: 0, folder, target: 'heading' });
  }
  function centerFolder(path: string): void {
    const current = viewRef.current;
    const folder = current && reviewPathKey(current.path) === reviewPathKey(path)
      ? current.path
      : navigate(path, null, true, 'center');
    if (folder) keyboard.requestFocus({ depth: 0, folder, target: 'heading', preventScroll: true });
  }
  function backFolder(depth = 0): void {
    const current = viewRef.current;
    if (!current) return;
    const parent = parentPath(current.path);
    const volumes = boot?.volumes ?? [];
    const drive = rootForSelection(current.path, volumes);
    if (parent && drive && rootForSelection(parent, volumes)?.id === drive.id) {
      const folder = navigate(parent, null, true, 'back');
      if (folder) keyboard.requestFocus({ depth: 0, folder, target: 'heading', preventScroll: true });
    } else if (depth > 0) {
      closePreviewByKey(depth);
    }
  }
  function selectEntry(entry: Entry): void {
    setSelectedEntryInfo(entry);
    flushBuffers();
    updateView({ selectedPath: entry.path }, true);
  }
  function currentSelection() {
    const root = rootForSelection(viewRef.current?.path, boot?.volumes ?? []);
    const targets = root ? resolvedSelection(selectedFor(selectionSets.current, root.id), root) : EMPTY_SELECTION;
    return { root, targets, signature: selectionSignature(root?.id ?? null, targets) };
  }
  function setRootSelection(rootId: string, targets: readonly GroupTarget[]): void {
    const next = replaceSelection(selectionSets.current, rootId, targets);
    selectionSets.current = next;
    setSelections(next);
    try { localStorage.setItem(SELECTION_PREFERENCE, JSON.stringify(Object.fromEntries(next))); }
    catch { setMessage('Selection updated for this session. Restoring it after restart is unavailable.'); }
  }
  function assertSelectionEditable(): void {
    if (readOnly || closeSession.current?.snapshot().phase !== 'idle') throw new Error('Group selection is unavailable while the review is read-only.');
    if (activeGroup.current) throw new Error('Finish or close the grouped review before changing its selection.');
  }
  function setEntrySelected(entry: Entry, selected?: boolean): void {
    assertSelectionEditable();
    const { root, targets } = currentSelection();
    const target = selectionTarget(entry, root);
    if (!root || !target) throw new Error('This entry is unavailable for grouped marking in the current review drive.');
    const next = changeSelection(targets, target, selected);
    if (next !== targets) setRootSelection(root.id, next);
  }
  function toggleEntry(entry: Entry): void {
    try { setEntrySelected(entry); } catch (cause) { report(cause); }
  }
  function setCurrentGroup(review: GroupReview | null): void {
    activeGroup.current = review;
    setGroupReview(review);
    attemptedGroup.current = null;
    setGroupAttempt(null);
    setGroupProblem(null);
  }
  function closeGroupReview(): void {
    if (submittingGroup.current || attemptedGroup.current) return;
    setCurrentGroup(null);
  }
  function groupRevision(): number {
    const writer = queue.current;
    if (!writer || readOnly || closeSession.current?.snapshot().phase !== 'idle') throw new Error('The saved review is unavailable for group changes.');
    return writer.revision;
  }
  function markReview(revision: number, selection = currentSelection()): GroupReview {
    const { root, targets, signature } = selection;
    if (!root || !targets.length) throw new Error('Select at least one review entry first.');
    return { mode: 'mark', operationId: crypto.randomUUID(), revision, rootId: root.id, signature, targets };
  }
  async function prepareGroupReview(expectedFingerprint?: unknown, expectedRevision?: unknown): Promise<void> {
    assertSelectionEditable();
    if (window.document.querySelector('dialog[open]')) throw new Error('Close the open dialog before reviewing the selection.');
    flushBuffers();
    await queue.current?.flush();
    const expected = expectedFingerprint === undefined ? null : await checkedSelection(expectedFingerprint);
    assertSelectionEditable();
    if (window.document.querySelector('dialog[open]')) throw new Error('Close the open dialog before reviewing the selection.');
    const revision = groupRevision();
    if (expected && (expected.signature !== currentSelection().signature || expectedRevision !== revision)) {
      throw new Error('The selection or review revision changed. Inspect the current selection again.');
    }
    setCurrentGroup(markReview(revision));
  }
  async function prepareGroupUndo(groupId: string, fromHistory = false): Promise<void> {
    if (activeGroup.current || (!fromHistory && window.document.querySelector('dialog[open]'))) {
      throw new Error('Close the open dialog before reviewing group Undo.');
    }
    flushBuffers();
    await queue.current?.flush();
    const revision = groupRevision();
    const saved = findSavedGroup(groupId);
    if (!saved) throw new Error('That saved group is no longer available to undo.');
    if (fromHistory) setShowHistory(false);
    setCurrentGroup({ mode: 'undo', operationId: crypto.randomUUID(), revision, group: saved });
  }
  function findSavedGroup(groupId: string): SavedGroup | null {
    const history = documentRef.current?.history ?? [];
    for (let index = history.length - 1; index >= 0; index--) {
      const group = savedGroup(history[index]);
      if (group?.id === groupId) return group;
    }
    return null;
  }
  function removeGroupTarget(target: GroupTarget): void {
    const review = activeGroup.current;
    if (review?.mode !== 'mark' || submittingGroup.current || attemptedGroup.current) return;
    const current = currentSelection();
    if (!current.root || current.signature !== review.signature) { setGroupProblem('The selection changed. Review the current entries again.'); return; }
    const next = changeSelection(current.targets, target, false);
    setRootSelection(current.root.id, next);
    setCurrentGroup(next.length ? markReview(review.revision) : null);
  }
  async function refreshGroupReview(): Promise<void> {
    const review = activeGroup.current;
    if (!review || submittingGroup.current || attemptedGroup.current) return;
    try {
      flushBuffers();
      await queue.current?.flush();
      if (activeGroup.current !== review) return;
      const revision = groupRevision();
      if (review.mode === 'mark') setCurrentGroup(markReview(revision));
      else {
        const group = findSavedGroup(review.group.id);
        if (!group) throw new Error('That saved group is no longer available to undo.');
        setCurrentGroup({ ...review, revision, group, operationId: crypto.randomUUID() });
      }
    } catch (cause) { setGroupProblem(String(cause)); }
  }
  async function applyGroup(operation: GroupOperation): Promise<void> {
    const review = activeGroup.current;
    if (!review || submittingGroup.current) return;
    const previous = attemptedGroup.current;
    if (previous && previous !== operation) return;
    submittingGroup.current = true;
    setGroupBusy(true);
    setGroupProblem(null);
    attemptedGroup.current = operation;
    setGroupAttempt(operation);
    let dispatched = false;
    let nativeRefusal = false;
    function committed(): boolean {
      return documentRef.current?.history.some(value => {
        if (!value || typeof value !== 'object' || Array.isArray(value)) return false;
        const event = record(value);
        return event.id === review?.operationId && event.type === operation.type;
      }) ?? false;
    }
    function finish(): void {
      if (!review) return;
      if (review.mode === 'mark') setRootSelection(review.rootId, []);
      setNotes({});
      setCurrentGroup(null);
      setMessage(review.mode === 'mark' ? `Saved one group mark for ${review.targets.length} entries.`
        : `Restored the previous marks for ${review.group.memberCount} group members.`);
      void refreshStatus().catch(report);
    }
    try {
      if (previous && queue.current?.needsExternalReconciliation) {
        const latest = parseBoot(await invoke('get_boot'));
        if (latest.problem) throw new Error(latest.problem);
        if (!queue.current.reconcileExternal(latest.document)) throw new Error('Review the retained writer state before retrying the group.');
        setObservedRevision(latest.document.revision);
        setDataTick(value => value + 1);
        if (committed()) { finish(); return; }
      }
      await runNativeWrite(async () => {
        const revision = groupRevision();
        if (!previous && revision !== review.revision) throw new Error('Saved review changed. Review the group again before applying it.');
        if (!previous && review.mode === 'mark' && currentSelection().signature !== review.signature) throw new Error('The selection changed. Review its exact entries again.');
        dispatched = true;
        try {
          return await invoke('mutate', { expectedRevision: review.revision, operationId: review.operationId, operation });
        } catch (cause) {
          nativeRefusal = typeof cause === 'string' && /no decisions were changed|no member was undone|no decisions were undone|group does not exist|invalid membership|revision conflict|review changed|safe grouped history size/i.test(cause);
          throw cause;
        }
      }, parseDocument);
      if (!committed()) throw new Error('The saved group receipt could not be confirmed.');
      finish();
    } catch (cause) {
      if (committed()) finish();
      else {
        if ((!dispatched && !previous) || (nativeRefusal && !queue.current?.state.error)) {
          attemptedGroup.current = null;
          setGroupAttempt(null);
        }
        setGroupProblem(attemptedGroup.current
          ? `The saved outcome is uncertain. Retry this same request to reconcile it. ${String(cause)}`
          : String(cause));
      }
    } finally {
      submittingGroup.current = false;
      setGroupBusy(false);
    }
  }
  function selectionFingerprint(signature: string): Promise<string> {
    if (selectionHash.current?.signature !== signature) {
      selectionHash.current = { signature, promise: textHash(signature) };
    }
    return selectionHash.current.promise;
  }
  async function checkedSelection(expected: unknown) {
    const current = currentSelection();
    if (typeof expected !== 'string' || expected !== await selectionFingerprint(current.signature) ||
        current.signature !== currentSelection().signature) {
      throw new Error('The group selection or review drive changed. Inspect its current fingerprint.');
    }
    return current;
  }
  function previewFolder(entry: Entry, fromDepth: number): void {
    if (entry.kind !== 'folder' || entry.problem || !viewRef.current) return;
    flushBuffers();
    const columns = previewBranch(viewRef.current.previewColumns, fromDepth, entry.path);
    setSelectedEntryInfo(entry);
    revealPreviewDepth.current = fromDepth + 1;
    updateView({ previewColumns: columns, selectedPath: entry.path, layout: { ...viewRef.current.layout, fillDepth: null } }, true);
  }
  function previewByKey(entry: Entry, fromDepth: number): void {
    previewFolder(entry, fromDepth);
    keyboard.requestFocus({ depth: fromDepth + 1, folder: entry.path, target: 'first' });
  }
  function changePreview(depth: number, patch: Partial<PreviewColumn>): void {
    const current = viewRef.current;
    if (!current || !current.previewColumns[depth - 1]) return;
    const columns = current.previewColumns.map((column, index) => index === depth - 1 ? { ...column, ...patch } : column);
    updateView({ previewColumns: columns }, patch.page !== undefined || patch.width !== undefined);
  }
  function closePreview(depth: number): void {
    const current = viewRef.current;
    if (!current) return;
    const removedPath = current.previewColumns[depth - 1]?.path;
    const columns = closePreviewBranch(current.previewColumns, depth);
    const descendantSelected = removedPath && current.selectedPath?.toLowerCase().startsWith(`${removedPath.replace(/\\$/, '').toLowerCase()}\\`);
    const layout = current.layout;
    updateView({ previewColumns: columns, selectedPath: descendantSelected ? removedPath : current.selectedPath,
      layout: layout?.fillDepth != null && layout.fillDepth >= depth ? { ...layout, fillDepth: null } : layout }, true);
  }
  function closePreviewByKey(depth: number): void {
    const current = viewRef.current;
    if (!current) return;
    const closing = current.previewColumns[depth - 1];
    const parent = depth === 1 ? current.path : current.previewColumns[depth - 2]?.path;
    if (!closing || !parent) return;
    closePreview(depth);
    keyboard.requestFocus({ depth: depth - 1, folder: parent, target: 'row', path: closing.path });
  }
  function focusDrive(): void {
    if (viewRef.current?.layout?.drivesCollapsed) {
      appRef.current?.querySelector<HTMLButtonElement>('[data-reopen="drives"]')?.focus();
      return;
    }
    const drives = Array.from(driveRegionRef.current?.querySelectorAll<HTMLButtonElement>('.drive-button:not(:disabled)') ?? []);
    (drives.find(button => button.getAttribute('aria-current') === 'true') ?? drives[0])?.focus();
  }
  function driveKey(event: ReactKeyEvent<HTMLDivElement>): void {
    if (event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
    if (!['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown'].includes(event.key)) return;
    const drives = Array.from(event.currentTarget.querySelectorAll<HTMLButtonElement>('.drive-button:not(:disabled)'));
    const index = drives.findIndex(button => button === event.target);
    if (index < 0) return;
    event.preventDefault();
    const delta = event.key === 'ArrowLeft' || event.key === 'ArrowUp' ? -1 : 1;
    drives[index + delta]?.focus();
  }
  function toolbarKey(event: ReactKeyEvent<HTMLDivElement>): void {
    if (event.defaultPrevented || event.nativeEvent.isComposing || event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
    const target = event.target;
    if (target !== searchRef.current && target !== knownButtonRef.current) return;
    if (event.key === 'Escape') {
      event.preventDefault();
      focusDrive();
    } else if (target === searchRef.current && event.key === 'ArrowRight' && isCaretBoundary(searchRef.current!, 'right')) {
      event.preventDefault();
      knownButtonRef.current?.focus();
    } else if (target === knownButtonRef.current && event.key === 'ArrowLeft') {
      event.preventDefault();
      searchRef.current?.focus();
    } else if (target === knownButtonRef.current && event.key === 'ArrowRight') {
      event.preventDefault();
      filterRef.current?.openFirst();
    }
  }
  async function firstAttention(folder: string, depth: number): Promise<void> {
    const current = viewRef.current;
    if (!current) return;
    const stillHere = keyboard.stillHere();
    const context = (view: ViewState | null): string => JSON.stringify([view?.path, view?.search, view?.filter, view?.previewColumns.map(column => column.path)]);
    const previous = context(current);
    const valid = (): boolean => stillHere() && context(viewRef.current) === previous;
    async function read(filter: string, refresh = false): Promise<Listing> {
      return folderReads.listing({ path: folder, search: '', filter, page: 0 }, refresh);
    }
    try {
      let attention = await read('attention');
      if (!valid()) return;
      if (attention.observedAt === null && attention.entries.length === 0) attention = await read('attention', true);
      if (!valid()) return;
      const first = attention.entries.find(entry => entry.effective.rollup?.kind !== 'mixed');
      const fallback = first ? null : (await read('all')).entries[0];
      if (!valid()) return;
      const target = first ?? fallback;
      if (!target) {
        setMessage('No entries need attention. This folder has no available rows.');
        return;
      }
      await queue.current?.flush();
      if (!valid()) return;
      const response = record(await invoke<unknown>('plan_review_reveal', { path: target.path }));
      if (!valid()) return;
      if (typeof response.revealPath !== 'string' || reviewPathKey(response.revealPath) !== reviewPathKey(target.path)) {
        throw new Error('The review plan returned a different path.');
      }
      if (typeof response.revision === 'number' && response.revision !== queue.current?.revision) {
        setMessage('The review changed while locating this row. Press Down again.');
        return;
      }
      const displayed = viewRef.current;
      if (!displayed) return;
      const plan = record(response.patch);
      let next = parseView({ ...displayed, ...plan, search: displayed.search, filter: displayed.filter, selectedPath: target.path, revealPath: target.path });
      if (depth > 0 && reviewPathKey(next.path) === reviewPathKey(folder) && reviewPathKey(displayed.path) !== reviewPathKey(folder)) {
        next = { ...displayed, selectedPath: target.path, revealPath: target.path };
      }
      setKnownResults(null);
      setKnownSearch(false);
      setSelectedEntryInfo(target);
      updateView(savedViewFields(next), true);
      setRevealVersion(value => value + 1);
      const targetDepth = reviewPathKey(next.path) === reviewPathKey(folder) ? 0
        : next.previewColumns.findIndex(column => reviewPathKey(column.path) === reviewPathKey(folder)) + 1;
      keyboard.requestFocus({ depth: targetDepth, folder, target: 'row', path: target.path });
      if (!first) setMessage('No entries need attention. Focused the first available row.');
    } catch (cause) {
      if (valid() && !isStaleRead(cause)) report(cause);
    }
  }
  function openRowMenu(target: RowMenuTarget): void { rowMenuRef.current = target; setRowMenu(target); }
  function dismissRowMenu(restoreFocus: boolean): void {
    const target = rowMenuRef.current;
    rowMenuRef.current = null;
    setRowMenu(null);
    if (restoreFocus && target?.invoker?.isConnected) requestAnimationFrame(() => target.invoker?.focus());
  }
  async function refreshStatus(): Promise<void> {
    const result = parseAppStatus(await invoke<unknown>('get_status'));
    if (mounted.current) setAppStatus(result);
  }

  useEffect(() => {
    mounted.current = true;
    let active = true;
    if (!('__TAURI_INTERNALS__' in window)) {
      setFatalError('Open PC Migration Helper as its native desktop application. A browser preview cannot read your folders or save the migration data file.');
      return;
    }
    void invoke<unknown>('get_boot').then(parseBoot).then(result => {
      if (!active) return;
      setBoot(result);
      setDocument(result.document);
      documentRef.current = result.document;
      setGateway(result.gateway);
      setView(result.document.view);
      viewRef.current = result.document.view;
      setPathDraft(result.document.view.path);
      queue.current = new MutationQueue(result.document, (revision, operationId, operation) => invoke<unknown>('mutate', { expectedRevision: revision, operationId, operation }), receiveDocument);
      void refreshStatus().catch(report);
    }).catch(cause => { if (active) setFatalError(String(cause)); });
    return () => {
      active = false;
      mounted.current = false;
      if (externalRefreshTimer.current !== null) clearTimeout(externalRefreshTimer.current);
    };
  }, []);

  useEffect(() => {
    if (!boot) return;
    let active = true;
    let inFlight = false;
    let failedReads = 0;
    async function pollExternal(): Promise<void> {
      if (inFlight) return;
      inFlight = true;
      try {
        const result = parseBoot(await invoke<unknown>('get_boot'));
        if (!active) return;
        failedReads = 0;
        if (!result.problem) {
          observedSuggestions.current = { revision: result.document.revision, suggestions: result.document.suggestions };
          setObservedRevision(result.document.revision);
        }
        setGateway(previous => JSON.stringify(previous) === JSON.stringify(result.gateway) ? previous : result.gateway);
        const writer = queue.current;
        if (!writer || result.problem || result.document.revision <= writer.revision) return;
        if (writer.state.error && !conflictReviewOpen.current) setExternalDocument(result.document);
        if (!canAdoptReview()) return;
        const previous = documentRef.current;
        if (!writer.acceptExternal(result.document)) return;
        setNotes({});
        setExternalDocument(null);
        if (previous && (JSON.stringify(previous.view) !== JSON.stringify(result.document.view) ||
          JSON.stringify(previous.viewProvenance) !== JSON.stringify(result.document.viewProvenance))) {
          const explicit = pendingExplicitReveal.current;
          const explicitApplied = explicit?.revision !== undefined && explicit.revision <= result.document.revision;
          if (explicitApplied) pendingExplicitReveal.current = null;
          applyDisplayedView(result.document.view, false, explicitApplied);
        }
        refreshAfterExternalChange();
      } catch (cause) {
        failedReads += 1;
        if (active && failedReads >= 3) setGateway({ state: 'unavailable', problem: `The native gateway status cannot currently be read: ${String(cause)}` });
      } finally { inFlight = false; }
    }
    pollExternalNow.current = () => { void pollExternal(); };
    const interval = setInterval(pollExternalNow.current, 1500);
    return () => { active = false; pollExternalNow.current = () => {}; clearInterval(interval); };
  }, [boot?.dataPath]);

  useLayoutEffect(() => {
    const strip = columnStripRef.current;
    if (!strip || !view) return;
    if (fillDepth !== null) {
      const column = strip.querySelector<HTMLElement>(`[data-column-depth="${fillDepth}"]`);
      if (column) restoreReviewScroll(strip, 'scrollLeft', column.offsetLeft);
      return;
    }
    const depth = revealPreviewDepth.current;
    if (depth !== null) {
      const column = strip.querySelector<HTMLElement>(`[data-column-depth="${depth}"]`);
      if (column) {
        const left = Math.max(0, column.offsetLeft + column.offsetWidth - strip.clientWidth);
        restoreReviewScroll(strip, 'scrollLeft', left);
        revealPreviewDepth.current = null;
        if (strip.scrollLeft !== view.previewScrollLeft) updateView({ previewScrollLeft: strip.scrollLeft });
      }
    } else restoreReviewScroll(strip, 'scrollLeft', view.previewScrollLeft);
  }, [view?.path, previewBranchKey, view?.previewScrollLeft, fillDepth]);
  const prepareFolderMotion = useFolderColumnMotion(columnStripRef, view);

  useLayoutEffect(() => {
    const target = toolbarFocus.current;
    toolbarFocus.current = null;
    if (target === 'path') pathInputRef.current?.focus();
    if (target === 'search') searchRef.current?.focus();
    if (target === 'compact') compactTrigger.current?.focus();
    if (target === 'expanded') expandedTrigger.current?.focus();
  }, [toolbar]);

  useLayoutEffect(() => {
    const target = columnFocus.current;
    columnFocus.current = null;
    if (target === 'restore') restoreColumnsRef.current?.focus({ preventScroll: true });
    else if (target !== null) columnStripRef.current?.querySelector<HTMLElement>(
      `[data-column-depth="${target}"] .review-resize-grip`,
    )?.focus({ preventScroll: true });
  }, [fillDepth]);

  useEffect(() => {
    if (!boot) return;
    let active = true;
    const refresh = (): void => { void refreshStatus().catch(cause => { if (active) report(cause); }); };
    const interval = setInterval(refresh, appStatus.scan.status === 'running' ? 1200 : 5000);
    return () => { active = false; clearInterval(interval); };
  }, [boot, appStatus.scan.status]);

  useEffect(() => {
    if (!view || !boot) return;
    let active = true;
    const path = view.path;
    const contextKey = JSON.stringify([path, view.search, view.filter, view.page]);
    const foreground = loadedListingContext.current !== contextKey || refreshRequested.current;
    const force = refreshRequested.current;
    refreshRequested.current = false;
    const query = { path, search: view.search, filter: view.filter, page: view.page };
    const cached = !force ? folderReads.peekListing(query) : undefined;
    if (cached) { loadedListingContext.current = contextKey; setListing(cached); setLoading(false); return; }
    const recent = !force ? folderReads.peekRecentListing(query) : undefined;
    if (recent) { setListing(recent); setLoading(false); }
    else if (foreground) setLoading(true);
    const timeout = setTimeout(() => {
      activeAuditReads.current += 1;
      void folderReads.listing(query, force).then(result => {
        if (!active) return;
        loadedListingContext.current = contextKey;
        setListing(result);
        setLoading(false);
      }).catch(cause => {
        if (!active) return;
        if (isStaleRead(cause)) { setLoading(false); return; }
        if (foreground) setListing(null);
        setLoading(false);
        report(cause);
        void refreshStatus().catch(report);
      }).finally(() => { activeAuditReads.current = Math.max(0, activeAuditReads.current - 1); });
    }, view.search ? 160 : 0);
    return () => { active = false; clearTimeout(timeout); };
  }, [boot, view?.path, view?.search, view?.filter, view?.page, folderRefreshToken]);
  const refreshRequested = useRef(false);

  useEffect(() => {
    if (!selectedPath || !boot) return;
    let active = true;
    if (loadedDetailsPath.current !== selectedPath) setDetailsLoading(true);
    activeAuditReads.current += 1;
    void invoke<unknown>('get_details', { path: selectedPath }).then(parseDetails).then(value => {
      if (active) { loadedDetailsPath.current = selectedPath; setDetails(value); setDetailsLoading(false); }
    }).catch(cause => { if (active) { setDetails(null); setDetailsLoading(false); report(cause); } }).finally(() => { activeAuditReads.current = Math.max(0, activeAuditReads.current - 1); });
    return () => { active = false; };
  }, [selectedPath, boot, dataTick, auditDataTick, refreshTick, appStatus.scan.status, appStatus.mirrorEvidenceStamp]);

  useEffect(() => {
    if (!boot) return;
    let disposed = false;
    const stops: (() => void)[] = [];
    const session = new CloseSession({
      status: async () => {
        const state = record(await invoke('background_status'));
        if (!disposed) setLifecycleSnapshot({ ...state, observedAtMs: Date.now() });
        return { enabled: state.enabled === true, ready: state.ready === true };
      },
      begin: async () => {
        const result = record(await invoke('background_begin_exit'));
        if (typeof result.ticket !== 'number' || !Number.isSafeInteger(result.ticket) || result.ticket < 1) throw new Error('The app returned an invalid exit ticket.');
        return result.ticket;
      },
      flush: async () => { flushBuffers(); await queue.current?.flush(); },
      hide: () => invoke('background_hide'),
      confirm: ticket => invoke('background_confirm_exit', { ticket }),
      cancel: ticket => invoke('background_cancel_exit', { ticket }),
    }, state => { if (!disposed) setCloseState(state); });
    closeSession.current = session;
    const request = (ticket?: number): void => {
      void session.request(ticket).catch(cause => {
        if (disposed) return;
        report(cause);
        setMessage(session.snapshot().phase === 'waiting'
          ? 'The window stays open while Exit awaits a safe checkpoint. Retry Exit or explicitly cancel it.'
          : 'The window stays open because closing could not finish. Pending edits remain here.');
      });
    };
    void getCurrentWindow().onCloseRequested(event => { event.preventDefault(); request(); })
      .then(stop => { if (disposed) stop(); else stops.push(stop); }).catch(report);
    void listen('background-exit-requested', event => {
      const ticket = record(event.payload).ticket;
      if (typeof ticket === 'number' && Number.isSafeInteger(ticket) && ticket > 0) request(ticket);
    }).then(stop => { if (disposed) stop(); else stops.push(stop); }).catch(report);
    return () => { disposed = true; for (const stop of stops) stop(); if (closeSession.current === session) closeSession.current = null; };
  }, [boot]);

  async function classify(status: Status): Promise<void> {
    if (!selectedPath) return;
    await classifyPath(selectedPath, status);
  }
  async function classifyPath(path: string, status: Status): Promise<void> {
    const contextPath = viewRef.current?.path;
    flushBuffers();
    try {
      await edit({ type: 'classify', path, status });
      if (viewRef.current?.path === contextPath) updateView({ selectedPath: path }, true);
      setDataTick(value => value + 1);
      setMessage(`${LABELS[status]} saved for ${baseName(path)}.`);
      void refreshStatus().catch(report);
    } catch (cause) { report(cause); }
  }
  async function undo(): Promise<void> {
    flushBuffers();
    try {
      await queue.current?.flush();
      const saved = documentRef.current;
      const latest = saved ? latestUndoEvent(saved) : undefined;
      if (latest?.type === 'bulkClassify') {
        if (typeof latest.id !== 'string') throw new Error('The saved group has no usable Undo identity.');
        await prepareGroupUndo(latest.id);
        return;
      }
      await edit({ type: 'undo' });
      setNotes({});
      setDataTick(value => value + 1);
      setMessage('Previous decision or note restored.');
      void refreshStatus().catch(report);
    } catch (cause) { report(cause); }
  }
  async function nextAttention(): Promise<void> {
    if (!view) return;
    try {
      const response = await invoke<unknown>('next_attention', { path: view.path, after: view.selectedPath });
      if (response === null) { setMessage('No other known item needs attention. Inventory more folders to extend coverage.'); return; }
      const next = record(response);
      if (typeof next.path !== 'string') throw new Error('Invalid next-folder response.');
      const selection = typeof next.selectedPath === 'string' ? next.selectedPath : null;
      const page = typeof next.page === 'number' ? next.page : 0;
      if (next.path.toLowerCase() === view.path.toLowerCase()) updateView({ selectedPath: selection, search: '', filter: 'all', page, scrollTop: 0, previewScrollLeft: 0 }, true);
      else { navigate(next.path, selection); updateView({ page }, true); }
      setMessage('Moved to the next known item needing attention.');
    } catch (cause) { report(cause); }
  }
  async function scan(mode: 'all' | 'current' | 'resume' | 'pause'): Promise<void> {
    if (!view) return;
    const freshScan = mode === 'all' || mode === 'current';
    try {
      await invoke('scan', { mode, path: view.path, includeGenerated: freshScan && includeGeneratedForIndex });
      if (freshScan) setIncludeGeneratedForIndex(false);
      await refreshStatus();
    } catch (cause) {
      report(cause);
    }
  }
  async function searchKnown(throwFailure = false): Promise<void> {
    if (!view?.search.trim()) { const problem = 'Type a folder name in Search, then search known folders.'; setMessage(problem); if (throwFailure) throw new Error(problem); return; }
    const searchView = viewRef.current;
    try {
      const response = await invoke<unknown>('search_dirs', { search: view.search });
      if (!Array.isArray(response) || response.some(v => typeof v !== 'string')) throw new Error('Invalid directory search response.');
      if (viewRef.current !== searchView) return;
      if (searchView?.revealPath || localFollowView.current) updateView({ revealPath: null }, true);
      setKnownResults(response);
      setKnownSearch(true);
    } catch (cause) { report(cause); if (throwFailure) throw cause; }
  }
  async function refreshVisible(): Promise<void> {
    const current = viewRef.current;
    if (!current || loading) throw new Error('Wait for the current folder read to finish.');
    setLoading(true);
    try {
      const next = await folderReads.listing({ path: current.path, search: current.search, filter: current.filter, page: current.page }, true);
      if (viewRef.current !== current) throw new Error('The review location changed during refresh.');
      setListing(next);
      setRefreshTick(value => value + 1);
    } finally { setLoading(false); }
  }
  function closePanels(): void {
    if (submittingGroup.current || attemptedGroup.current) throw new Error('Reconcile the submitted group before closing its review.');
    closeGroupReview();
    setShowGaps(false); setShowHistory(false); setShowDecisionPicker(false); setShowContext(false);
    setShowMcp(false); setShowAssistance(false); setShowMirror(false); setMirrorReviewPath(null); setShowSaved(false); closeConflictReview(); dismissRowMenu(true);
    void notificationControl.current?.execute({ action: 'setNotifications', mode: 'closed' }, { command: { action: 'setNotifications' } });
    void copyControl.current?.execute({ action: 'setCopyDetails', open: false }, { command: { action: 'setCopyDetails' } });
  }
  function readUiState() {
    const retained = queue.current?.retainedRequests ?? [];
    const retainedMetadata = pendingMetadata(retained);
    const draftNotes = Array.from(noteBuffer.current, ([path, text]) => ({ path, text }));
    const visibleText = selectedPath ? notes[selectedPath] ?? details?.decision?.notes ?? '' : '';
    return {
      revision: documentRef.current?.revision, readOnly, followAi, close: { ...closeState, nativeAtRequest: lifecycleSnapshot },
      panels: { gaps: showGaps, history: showHistory, decision: showDecisionPicker, context: showContext,
        gateway: showMcp, assistance: showAssistance, saved: showSaved, conflict: showConflict, mirror: showMirror, selection: groupReview?.mode ?? null },
      selection: { rootId: selectionRoot?.id ?? null, total: groupTargets.length, visible: visibleGroupCount,
        hidden: groupTargets.length - visibleGroupCount, targets: groupTargets.map(reportSelectionTarget),
        review: groupReview ? { mode: groupReview.mode, revision: groupReview.revision, operationId: groupReview.operationId,
          groupId: groupReview.mode === 'undo' ? groupReview.group.id : null, pending: groupBusy, outcomeUncertain: !groupBusy && groupAttempt !== null,
          problem: groupProblem } : null },
      rowMenu: rowMenu ? { path: rowMenu.entry.path, disabled: readOnly || Boolean(rowMenu.entry.problem) } : null,
      gatewayDetailsOpen, knownFolders: { active: knownSearch, search: view?.search, results: knownResults },
      savedRecords: { search: savedSearch, filter: savedFilter, page: savedPage },
      writer: { ...queue.current?.state, revision: queue.current?.revision, retainedRequestCount: retainedMetadata.total,
        retainedRequests: retainedMetadata.entries, metadataTruncated: retainedMetadata.truncated, inspection: 'listPending / readPendingText' },
      drafts: { path: pathDraft.slice(0, 100_000), pathTruncated: pathDraft.length > 100_000,
        viewFields: Object.keys(viewBuffer.current), noteCount: draftNotes.length,
        notes: draftNotes.slice(0, 16).map((item, index) => ({ operationId: `buffered-note-${index}`, pathPreview: item.path.slice(0, 256), textCharacters: item.text.length })),
        metadataTruncated: draftNotes.length > 16,
        visibleNote: { path: selectedPath, text: visibleText.slice(0, 100_000), textCharacters: visibleText.length, textTruncated: visibleText.length > 100_000 } },
      conflict: { loading: conflictLoading, problem: conflictProblem, comparisonRevision: externalDocument?.revision ?? null },
      notifications: notificationControl.current?.read() ?? null,
      copy: copyControl.current?.read() ?? null,
      assistance: assistanceControl.current?.read() ?? null,
      mirror: mirrorControl.current?.read() ?? null,
      feedback: { status: message, error, saveError: saveState.error },
    };
  }
  function readPendingSource(): PendingSnapshot {
    const writer = queue.current;
    const retained = writer?.retainedRequests ?? [];
    const noteDrafts = Array.from(noteBuffer.current, ([path, notes], index) => ({ id: `buffered-note-${index}`, origin: 'draft-note', operation: { type: 'note', path, notes } }));
    const viewDrafts = Object.keys(viewBuffer.current).length ? [{ id: 'buffered-view', origin: 'draft-view', operation: { type: 'view', patch: viewBuffer.current } }] : [];
    const pathDrafts = pathDraft !== viewRef.current?.path ? [{ id: 'buffered-path', origin: 'draft-path', operation: { type: 'path-draft', text: pathDraft } }] : [];
    return {
      writer: { ...writer?.state, revision: writer?.revision, retainedRequests: retained },
      drafts: { path: pathDraft, view: viewBuffer.current, notes: noteDrafts },
      requests: [...retained.map(item => ({ ...item, origin: 'retained' })), ...noteDrafts, ...viewDrafts, ...pathDrafts],
    };
  }
  async function readUiReport(): Promise<Record<string, unknown>> {
    const writer = queue.current;
    const version = writer?.inspectionVersion ?? 0;
    const cached = fingerprintCache.current;
    if (!cached || cached.writer !== writer || cached.version !== version || cached.draftVersion !== draftGeneration.current || cached.path !== pathDraft) {
      fingerprintCache.current = { writer, version, draftVersion: draftGeneration.current, path: pathDraft, promise: pendingFingerprint(readPendingSource()) };
    }
    const state = readUiState();
    const [pending, groupFingerprint] = await Promise.all([
      fingerprintCache.current!.promise,
      selectionFingerprint(selectionSignature(selectionRoot?.id ?? null, groupTargets)),
    ]);
    return { ...state, pendingFingerprint: pending, selection: { ...state.selection, fingerprint: groupFingerprint } };
  }
  async function executeUi(command: UiCommand, request: UiRequest): Promise<unknown> {
    const action = command.action;
    if (action === 'listPending' || action === 'readPendingText') return inspectPending(readPendingSource, command);
    if (['closeWindow', 'exitApp', 'retryExit', 'cancelExit'].includes(action)) {
      const session = closeSession.current;
      if (!session) throw new Error('The close workflow is not ready. Wait for the saved session to load.');
      if (action === 'closeWindow' || action === 'exitApp') {
        if (session.snapshot().phase !== 'idle') throw new Error('Exit is already pending. Inspect its ticket and use Retry Exit or Cancel Exit.');
        const native = record(await invoke('background_status'));
        assertNewCloseAllowed(session.snapshot(), native);
      }
      if (action === 'closeWindow') { await session.request(); return; }
      if (action === 'exitApp') {
        const pending = record(await invoke('background_begin_exit'));
        if (typeof pending.ticket !== 'number' || !Number.isSafeInteger(pending.ticket) || pending.ticket < 1) throw new Error('The app returned an invalid exit ticket.');
        await session.request(pending.ticket); return;
      }
      const native = record(await invoke('background_status'));
      const current = session.snapshot();
      if (command.expectedExitTicket !== current.ticket || command.expectedExitTicket !== record(native.pending).ticket || current.phase !== 'waiting') throw new Error('This exit ticket or phase changed. Inspect the current close state before continuing.');
      if (action === 'cancelExit') { await session.cancel(); setError(null); setMessage('Exit canceled. You can continue reviewing.'); }
      else await session.request();
      return;
    }
    if (closeState.phase !== 'idle' && action !== 'dismissFeedback') throw new Error('The app is finishing Exit. Pending work remains inspectable; wait or cancel Exit before changing controls.');
    const retainedWorkflow = ['compareConflict', 'retryReviewed', 'retryPending', 'flushPending', 'closePanels', 'dismissFeedback'].includes(action) || action === 'openPanel' && command.panel === 'conflict';
    if (!retainedWorkflow && request.expectedRevision !== documentRef.current?.revision) throw new Error('The displayed revision changed before this UI command could apply. Read the current UI state again.');
    if (action === 'setEntrySelection' || action === 'clearEntrySelection' || action === 'openSelectionReview') {
      assertSelectionEditable();
      const expected = command.expectedSelectionFingerprint;
      if (action === 'openSelectionReview') { await prepareGroupReview(expected, request.expectedRevision); return; }
      const selection = await checkedSelection(expected);
      assertSelectionEditable();
      if (selection.signature !== currentSelection().signature || request.expectedRevision !== documentRef.current?.revision) {
        throw new Error('The selection or review revision changed. Inspect the current selection again.');
      }
      if (!selection.root) throw new Error('There is no current review drive for group selection.');
      if (action === 'clearEntrySelection') { setRootSelection(selection.root.id, []); return; }
      if (typeof command.path !== 'string' || typeof command.selected !== 'boolean') throw new Error('Supply the exact entry path and desired selection state.');
      const normalized = normalizeReviewPath(command.path, [selection.root.root]);
      const key = reviewPathKey(normalized);
      if (!command.selected) {
        const target = selection.targets.find(target => reviewPathKey(target.path) === key);
        if (target) setRootSelection(selection.root.id, changeSelection(selection.targets, target, false));
        return;
      }
      const displayed = (): boolean => Array.from(columnStripRef.current?.querySelectorAll<HTMLElement>('.entry-row[data-entry-path]') ?? [])
        .some(row => reviewPathKey(row.dataset.entryPath ?? '') === key);
      if (!displayed()) throw new Error('Open the exact entry in a current folder column before selecting it.');
      const entry = parseEntry(await invoke('get_review_entry', { path: normalized }));
      const rechecked = await checkedSelection(expected);
      if (rechecked.signature !== currentSelection().signature || request.expectedRevision !== documentRef.current?.revision ||
          !displayed() || reviewPathKey(entry.path) !== key) throw new Error('The displayed entry changed. Review its current row again.');
      setEntrySelected(entry, true);
      return;
    }
    if (action === 'setFollowAI') { changeFollowAi(command.enabled === true); return; }
    if (action === 'setAutoCopy' || action === 'setCopyDetails') {
      if (!copyControl.current) throw new Error('The copy panel is not available.');
      await copyControl.current.execute(command, request); return;
    }
    if (action === 'setNotifications') {
      if (!notificationControl.current) throw new Error('Notifications are not available.');
      await notificationControl.current.execute(command, request); return;
    }
    if (action === 'setAssistanceOverride' || action === 'setAssistanceScanDetails' || action === 'setAssistanceDetails' || action === 'navigateAssistance') {
      if (!showAssistance || !assistanceControl.current) throw new Error('Open AI Assistance first.');
      await assistanceControl.current.execute(command, request); return;
    }
    if (action === 'closePanels') { closePanels(); return; }
    if (action === 'openPanel') {
      if (command.panel === 'conflict') {
        if (!queue.current?.state.error) throw new Error('No failed writer is available to compare.');
        await beginConflictReview(true); return;
      }
      if (command.panel === 'decision' && (readOnly || !selectedPath || detailsLoading || !details || !details.exists)) throw new Error('The selected decision is not available for editing.');
      const setters: Record<string, (open: boolean) => void> = { assistance: changeAssistance, decision: setShowDecisionPicker,
        context: setShowContext, history: setShowHistory, saved: setShowSaved, gaps: setShowGaps, gateway: setShowMcp, mirror: setShowMirror };
      const setter = setters[String(command.panel)];
      if (!setter) throw new Error('Unsupported panel');
      setter(true); return;
    }
    if (action === 'setSavedRecords') {
      if (!showSaved) throw new Error('Open Saved records before controlling its query.');
      const search = typeof command.search === 'string' ? command.search : savedSearch;
      const filter = typeof command.filter === 'string' ? command.filter : savedFilter;
      const page = typeof command.page === 'number' ? command.page : command.search !== undefined || command.filter !== undefined ? 0 : savedPage;
      const total = Object.values(documentRef.current?.decisions ?? {}).filter(decision => (filter === 'all' || decision.status === filter) && `${decision.path}\n${decision.notes ?? ''}`.toLowerCase().includes(search.toLowerCase())).length;
      if (page >= Math.max(1, Math.ceil(total / 100))) throw new Error('The saved-record page is outside the current results.');
      setSavedSearch(search); setSavedFilter(filter); setSavedPage(page); return;
    }
    if (action === 'setGatewayDetails') { if (!showMcp) throw new Error('Open Local MCP gateway first.'); setGatewayDetailsOpen(command.open === true); return; }
    if (action === 'setRowMenu') {
      if (command.path === null) { dismissRowMenu(true); return; }
      const path = String(command.path);
      const parent = parentPath(path);
      if (!viewRef.current || !parent || ![viewRef.current.path, ...viewRef.current.previewColumns.map(column => column.path)].some(folder => reviewPathKey(folder) === reviewPathKey(parent))) throw new Error('Open the parent review column before opening this row menu.');
      const entry = parseEntry(await invoke('get_review_entry', { path }));
      openRowMenu({ id: crypto.randomUUID(), entry, x: Math.max(8, window.innerWidth / 2 - 180), y: 100, invoker: null }); return;
    }
    if (action === 'refreshVisible') { await refreshVisible(); return; }
    if (action === 'searchKnownFolders') { await searchKnown(true); return; }
    if (action === 'flushPending') { flushBuffers(); await queue.current?.flush(); return; }
    if (action === 'retryPending') {
      const writer = queue.current;
      if (!writer?.state.error) throw new Error('There is no failed save to retry.');
      if (/newer saved state|revision conflict|outside this app|stale save response/i.test(writer.state.error)) throw new Error('Review the current conflict before applying retained edits.');
      writer.retry(); await writer.flush(); return;
    }
    if (action === 'compareConflict') { if (!queue.current?.state.error) throw new Error('There is no failed writer to compare.'); await beginConflictReview(true); return; }
    if (action === 'retryReviewed') {
      if (!showConflict || command.expectedComparisonRevision !== externalDocument?.revision || command.expectedPendingFingerprint !== await pendingFingerprint(readPendingSource())) throw new Error('The pending requests or displayed comparison changed. Refresh and review them again.');
      await applyReviewedPending(); return;
    }
    if (action === 'dismissFeedback') { if (command.kind === 'status') setMessage(null); else setError(null); return; }
    throw new Error('Unsupported UI command');
  }
  useUiBridge(gateway.instanceId, { read: readUiState, readForReport: readUiReport, execute: executeUi });
  useEffect(() => {
    function key(event: KeyboardEvent): void {
      if (event.defaultPrevented || rowMenuRef.current || window.document.querySelector('dialog[open], .review-filter-options')) return;
      const target = event.target;
      if (target instanceof HTMLElement && (target.isContentEditable || ['INPUT', 'TEXTAREA', 'SELECT'].includes(target.tagName))) return;
      if (event.ctrlKey && event.key.toLowerCase() === 'z') { event.preventDefault(); if (!readOnly) void undo(); }
      else if (event.altKey && event.key === 'ArrowUp' && view) {
        event.preventDefault();
        backFolder();
      }
    }
    window.addEventListener('keydown', key);
    return () => window.removeEventListener('keydown', key);
  }, [view, selectedPath, listing, readOnly, boot]);

  if (fatalError) return <main className="startup"><div className="startup-card"><TriangleAlert size={38}/><h1>PC Migration Helper could not open</h1><p>{fatalError}</p><p>The migration data file has not been reset. Close this window and reopen after resolving the issue.</p></div></main>;
  if (!boot || !view || !document) return <main className="startup"><LoaderCircle className="spin" size={32}/><h1>Opening your review</h1><p>Reading persistent decisions and checking the connected volumes…</p></main>;

  const effective = details?.path === selectedPath ? details.effective : EMPTY_EFFECTIVE;
  const activeDecision = details?.path === selectedPath ? details.decision : null;
  const activeProvenance = typeof activeDecision?.provenance === 'object' && activeDecision.provenance !== null && !Array.isArray(activeDecision.provenance) ? record(activeDecision.provenance) : null;
  const agentDecision = activeProvenance?.kind === 'agent-mcp' || activeProvenance?.kind === 'mcp-agent';
  const noteText = selectedPath ? notes[selectedPath] ?? activeDecision?.notes ?? '' : '';
  const up = parentPath(view.path);
  const driveHome = rootForSelection(view.path, boot.volumes)?.root;
  const canUp = up !== null && driveHome !== undefined && reviewPathKey(view.path) !== reviewPathKey(driveHome);
  const scanRunning = appStatus.scan.status === 'running';
  const saveError = saveState.error;
  const revisionConflict = Boolean(saveError && /newer saved state|revision conflict|outside this app|stale save response/i.test(saveError));
  const saving = buffered || saveState.saving;
  const visibleError = saveError || error;
  const pageCount = Math.max(1, Math.ceil((listing?.total ?? 0) / 100));
  const selectedEntry = listing?.entries.find(entry => entry.path === selectedPath) ?? (selectedEntryInfo?.path === selectedPath ? selectedEntryInfo : null);
  const knownInView = Object.values(listing?.counts ?? {}).reduce((sum, count) => sum + count, 0);
  const reviewed = Math.max(0, knownInView - (listing?.attention ?? 0));
  const percent = knownInView ? Math.round(reviewed / knownInView * 100) : 0;
  const ActiveIcon = ICONS[effective.status];
  const classificationUnavailable = readOnly || !selectedPath || detailsLoading || !details || details.exists === false;
  const manualContext = effective.status === 'manual-pending' ? 'Manual backup pending · confirm after copying'
    : effective.status === 'manual-confirmed' ? 'Your manual-backup confirmation · checksum verification separate' : null;
  const incompleteRollup = effective.rollup?.kind === 'unknown' && !effective.rollup.coverage?.complete;
  const contextSummary = details?.exists === false ? 'Missing or unavailable · notes retained'
    : effective.stale ? 'Source changed · review again'
    : effective.rollup ? reviewStatusLabel(effective)
    : effective.inheritedFrom && effective.originalStatus === 'on-external' ? 'Parent’s F: assertion does not cover this child'
    : effective.inheritedFrom && effective.originalStatus === 'manual-confirmed' ? 'Parent’s manual confirmation · review this child'
    : manualContext ?? (effective.inheritedFrom ? 'Inherited decision · child override available'
      : effective.mixed > 0 ? `${effective.mixed} child exceptions · mixed decisions`
      : effective.status === 'on-external' ? 'Manual F: assertion · completeness unverified'
      : 'Decisions only · source files stay untouched');
  const observed = observedSuggestions.current;
  const proposalSource = showAssistance && observed && observed.revision > document.revision ? observed : document;
  const suggestions = Array.isArray(proposalSource.suggestions) ? proposalSource.suggestions.flatMap(value => {
    if (typeof value !== 'object' || value === null || Array.isArray(value)) return [];
    const suggestion = record(value);
    if (typeof suggestion.path !== 'string' || !isStatus(suggestion.status) || typeof suggestion.reason !== 'string') return [];
    const location = resolveDecisionPath({ ...suggestion, path: suggestion.path, status: suggestion.status }, boot.volumes);
    if (!location.path || (!showAssistance && location.path.toLowerCase() !== selectedPath?.toLowerCase())) return [];
    const provenance = typeof suggestion.provenance === 'object' && suggestion.provenance !== null && !Array.isArray(suggestion.provenance) ? record(suggestion.provenance) : null;
    return [{ path: location.path, id: typeof suggestion.id === 'string' ? suggestion.id : suggestion.path, status: suggestion.status, reason: suggestion.reason, createdAt: typeof suggestion.createdAt === 'string' ? suggestion.createdAt : null, actor: typeof provenance?.actor === 'string' ? provenance.actor : 'Unspecified agent' }];
  }) : [];
  const pathSuggestions = suggestions.filter(suggestion => reviewPathKey(suggestion.path) === reviewPathKey(selectedPath ?? ''));
  const assistanceSuggestions = new Map<string, RowSuggestion>();
  if (showAssistance) for (const suggestion of suggestions) {
    assistanceSuggestions.set(reviewPathKey(suggestion.path),
      { source: 'ai', label: decisionChoiceLabel(suggestion.status), reason: suggestion.reason });
  }
  function inspectAssistanceQuestion(path: string, kind: 'file' | 'folder'): void {
    const folder = kind === 'file' ? parentPath(path) : path;
    if (!folder || reviewPathKey(viewRef.current?.path ?? '') === reviewPathKey(folder)) return;
    navigate(folder, kind === 'file' ? path : null);
  }
  const assistanceBrowser: UiParticipant = {
    read: () => ({ root: view.path, selectedPath, columns: [
      { path: view.path, page: view.page, width: view.browserColumnWidth ?? 360, scrollTop: view.scrollTop },
      ...view.previewColumns,
    ], menuPath: rowMenu?.entry.path ?? null }),
    execute: async command => {
      const current = viewRef.current;
      if (!current) throw new Error('The folder browser is unavailable.');
      const mode = command.mode;
      if (mode === 'menu' && command.path === null) { dismissRowMenu(true); return; }
      if (mode === 'center' || mode === 'preview' || mode === 'menu') {
        const path = command.path;
        if (typeof path !== 'string') throw new Error('Choose a displayed entry.');
        const row = Array.from(columnStripRef.current?.querySelectorAll<HTMLElement>('.entry-row') ?? [])
          .find(item => reviewPathKey(item.dataset.entryPath ?? '') === reviewPathKey(path));
        const actual = row?.dataset.entryPath;
        if (!row || !actual) throw new Error('Choose a displayed entry.');
        const entry = parseEntry(await invoke('get_review_entry', { path: actual }));
        if (!row.isConnected || entry.path !== actual) throw new Error('The entry changed. Refresh its folder.');
        if (mode === 'menu') {
          const invoker = row.querySelector<HTMLButtonElement>('.entry-more');
          const bounds = invoker?.getBoundingClientRect();
          if (!invoker || !bounds) throw new Error('The row menu is unavailable.');
          openRowMenu({ id: crypto.randomUUID(), entry, x: bounds.right, y: bounds.bottom, invoker });
          return;
        }
        if (entry.kind !== 'folder' || entry.problem) throw new Error('Choose a readable folder.');
        if (mode === 'center') { centerFolder(entry.path); return; }
        const depth = Number(row.closest<HTMLElement>('[data-column-depth]')?.dataset.columnDepth);
        if (!Number.isInteger(depth)) throw new Error('The folder column is no longer open.');
        previewFolder(entry, depth);
        return;
      }
      const depth = command.depth;
      if (typeof depth !== 'number' || !Number.isInteger(depth) || depth < 0 || depth > current.previewColumns.length) {
        throw new Error('Choose an open column.');
      }
      if (mode === 'close' && depth > 0) { closePreview(depth); return; }
      if (mode === 'resize' && typeof command.value === 'number') { resizeColumn(depth, command.value, false); return; }
      if (mode === 'page' && typeof command.value === 'number' && Number.isInteger(command.value) && command.value >= 0) {
        if (depth) changePreview(depth, { page: command.value, scrollTop: 0 });
        else updateView({ page: command.value, scrollTop: 0 }, true);
        return;
      }
      if (mode === 'refresh') {
        if (!depth) { await refreshVisible(); return; }
        const column = current.previewColumns[depth - 1];
        if (!column) throw new Error('The column is no longer open.');
        await folderReads.listing({ path: column.path, search: '', filter: 'all', page: column.page }, true);
        setRefreshTick(value => value + 1);
        return;
      }
      throw new Error('Invalid folder browser action.');
    },
  };
  const savedDecisions = Object.values(document.decisions).filter(decision => (savedFilter === 'all' || decision.status === savedFilter) && `${decision.path}\n${decision.notes ?? ''}`.toLowerCase().includes(savedSearch.toLowerCase())).sort((a, b) => a.path.localeCompare(b.path));
  const columnActions = {
    selectedPath,
    menuPath: rowMenu?.entry.path ?? null,
    reviewActivity,
    revealPath: view.revealPath,
    revealVersion,
    suggestionFor: showAssistance ? (entry: Entry) => assistanceSuggestions.get(reviewPathKey(entry.path)) ?? localRowSuggestion(entry) : undefined,
    onSelect: selectEntry,
    onToggleSelect: toggleEntry,
    canGroupSelect: (entry: Entry) => !readOnly && Boolean(selectionTarget(entry, selectionRoot)),
    isGroupSelected: (entry: Entry) => {
      const target = selectionTarget(entry, selectionRoot);
      return target !== undefined && groupKeys.has(selectionKey(target));
    },
    onRowsDisplayed,
    onOpen: centerFolder,
    onOpenByKey: centerFolder,
    onPreview: previewFolder,
    onPreviewByKey: previewByKey,
    onCloseByKey: closePreviewByKey,
    onEscapeRows: focusSearch,
    onResizeColumn: resizeColumn,
    fillDepth,
    onFirstAttention: (path: string, depth: number): void => { void firstAttention(path, depth); },
    onMenu: openRowMenu,
    onReviewFolder: (path: string): void => { setSelectedEntryInfo(null); updateView({ selectedPath: path }, true); },
    onReadPendingChange: (delta: number): void => { activeAuditReads.current = Math.max(0, activeAuditReads.current + delta); },
  };
  const rowMenuDisabled = readOnly ? 'Recover the saved data before changing decisions.' : rowMenu?.entry.problem ? 'This path is unavailable. Its saved record is retained; review its notes from Saved records.' : null;

  const layout = view.layout ?? {};
  const layoutStyle = {
    '--saved-drives-width': layout.drivesWidth === undefined ? undefined : layout.drivesWidth + 'px',
    '--saved-decision-width': layout.decisionWidth === undefined ? undefined : layout.decisionWidth + 'px',
  } as CSSProperties;
  return <div className="app-shell review-layout" ref={appRef} style={layoutStyle}
    data-drives-collapsed={layout.drivesCollapsed || undefined} data-decision-collapsed={layout.decisionCollapsed || undefined}>
    <header className="topbar">
      <div className="brand-icon"><MapIcon size={23}/></div>
      <div className="brand"><h1>PC Migration Helper</h1><span>A careful path to a fresh start</span></div>
      {boot.fixture && <span className="fixture-badge">SAFE TEST FIXTURE</span>}
      <span className="review-only"><ShieldCheck size={15}/>Decision journal · read-only source browsing</span>
      <button className={`mcp-status mcp-${gateway.state}`} aria-haspopup="dialog" onClick={() => setShowMcp(true)} title="Inspect the local MCP gateway; readiness is separate from a Codex connection"><Link2 size={15}/><span>{gateway.state === 'running' ? 'MCP ready' : gateway.state === 'error' ? 'MCP needs attention' : 'MCP unavailable'}</span></button>
      <label className="follow-ai-control" title="Reveal folders and files as the agent touches them. Only you can turn this on or off."><input type="checkbox" aria-label="Follow AI’s movement" checked={followAi} onChange={event => changeFollowAi(event.target.checked)}/><span>Follow AI’s movement</span></label>
      <div className={`save-indicator ${saveError || boot.problem ? 'save-error' : ''}`} role="status" aria-live="polite">
        {saveError || boot.problem ? <TriangleAlert size={16}/> : saving || closeState.phase !== 'idle' ? <LoaderCircle size={16} className="spin"/> : <CheckCircle2 size={16}/>}
        <span>{closeState.phase === 'waiting' ? 'Exit waiting' : closeState.phase === 'stopping' ? 'Finishing safe checkpoint…' : closeState.phase !== 'idle' ? 'Saving before close…' : saveError ? 'Not saved' : boot.problem ? 'Recovery needed' : saving ? 'Saving…' : boot.fixture ? 'Saved in test fixture' : 'Saved locally'}</span>
      </div>
      <NotificationBell enabled={Boolean(boot)} unread={audit.status?.notificationsUnread ?? 0} volumes={boot.volumes} onNavigate={navigate} onReadStateChanged={audit.refresh} onError={report} controlRef={notificationControl}/>
    </header>
    <ReviewSidebar side="drives" name="review drives" collapsed={Boolean(layout.drivesCollapsed)} app={() => appRef.current}
      onResize={(width, collapse) => setLayout(collapse ? { drivesCollapsed: true } : { drivesWidth: width })}
      onToggle={drivesCollapsed => setLayout({ drivesCollapsed })}>
    <aside className="sidebar">
      <div className="side-section" ref={driveRegionRef} role="group" aria-label="Review drives" onKeyDown={driveKey}><h2>REVIEW DRIVES</h2>
        {boot.volumes.map(volume => <button key={volume.id} className={`drive-button ${view.path.toLowerCase().startsWith(volume.root.toLowerCase()) ? 'active' : ''}`} onClick={event => { if (event.detail === 0) navigateByKey(volume.root); else navigate(volume.root); }} aria-current={view.path.toLowerCase().startsWith(volume.root.toLowerCase()) ? "true" : undefined} disabled={!volume.available} title={volume.problem || volume.root}>
          <HardDrive size={22}/><span><strong>{volume.root}</strong><small>{volume.label || 'Local volume'}</small></span><ChevronRight size={15}/>
        </button>)}
      </div>
      <div className="side-section progress-section"><h2>THIS VIEW</h2><div className="progress-numbers"><strong>{reviewed.toLocaleString()}<span> / {knownInView.toLocaleString()}</span></strong><span>{percent}%</span></div>
        <div className="progress-track" role="progressbar" aria-label="Decisions in this folder" aria-valuenow={percent} aria-valuemin={0} aria-valuemax={100}><div style={{ width: `${percent}%` }}/></div>
        <p className="progress-caption" title="Known items with a decision. Coverage and backup verification are separate.">Decisions in this view</p>
        <button className="saved-count" onClick={() => setShowSaved(true)}><Database size={13}/>{appStatus.decisionCount.toLocaleString()} saved records<ChevronRight size={12}/></button>
      </div>
      <label className="recent-places"><span>RECENT LOCATIONS</span><select aria-label="Open recent location" value="" disabled={!view.recentPaths.length} onChange={event => navigate(event.target.value)}><option value="" disabled>{view.recentPaths.length ? 'Choose a recent folder…' : 'No recent folders yet'}</option>{view.recentPaths.map(path => <option value={path} key={path}>{path}</option>)}</select></label>
      <div className="inventory-card audit-inventory"><AuditControls status={audit.status} problem={audit.problem} busy={audit.busy} path={view.path} onControl={audit.control} onDetails={() => setShowGaps(true)}/></div>
    </aside>
    </ReviewSidebar>
    <main className="workspace">
      <CopyQueue revision={document.revision} enabled={!readOnly} controlRef={copyControl} onNativeWrite={runNativeWrite}/>
      {closeState.phase !== 'idle' && <div className="recovery-banner" role="status"><LoaderCircle size={20} className="spin"/><div>
        <strong>{closeState.phase === 'waiting' ? 'Exit is waiting for a safe checkpoint' : 'Finishing before closing'}</strong>
        <p>Pending edits stay in this window until saved. Exit stops background work at a safe checkpoint.</p>
        {closeState.phase === 'waiting' && <><button onClick={() => { void closeSession.current?.request().catch(report); }}>Retry Exit</button>
          <button onClick={() => { void closeSession.current?.cancel().then(() => { setError(null); setMessage('Exit canceled. You can continue reviewing.'); }).catch(report); }}>Cancel Exit</button></>}
      </div></div>}
      <div className="backup-reminder"><Info size={16}/><span><strong>Review journal.</strong> Include migration_master_data.json in your external backup.</span></div>
      {boot.problem && <div className="recovery-banner" role="alert"><TriangleAlert size={20}/><div><strong>Saved data needs recovery</strong><p>{boot.problem}</p>{boot.recoveryAvailable && <button onClick={() => { void invoke<unknown>('recover').then(parseDocument).then(doc => { setDocument(doc); documentRef.current = doc; setView(doc.view); viewRef.current = doc.view; setBoot({ ...boot, document: doc, problem: null }); queue.current = new MutationQueue(doc, (revision, operationId, operation) => invoke('mutate', { expectedRevision: revision, operationId, operation }), receiveDocument); setDataTick(v => v + 1); setMessage('Recovered the last-known-good copy. The malformed original was preserved separately.'); }).catch(report); }}>Recover last-known-good copy</button>}</div></div>}
      {visibleError && <div className="error-banner" role="alert"><TriangleAlert size={17}/><span>{visibleError}{revisionConflict && <small>Your pending edits remain in this window. A newer saved version is protected; nothing will be reapplied automatically.</small>}</span>{revisionConflict ? <button onClick={openConflictReview}>Review conflict</button> : saveError ? <button onClick={() => queue.current?.retry()}>Retry saving</button> : <button aria-label="Dismiss error" onClick={() => setError(null)}><X size={16}/></button>}</div>}
      <section className="browser-panel">
        <div className="browser-heading">
          <div><span className="eyebrow">FOLDER REVIEW</span><h2>{baseName(view.path)}</h2></div>
          <section className="review-heading-actions" aria-label="Folder review actions">
            <button className="assistance-trigger" aria-haspopup="dialog" onClick={() => setShowMirror(true)}>
              <ShieldCheck size={16} aria-hidden="true"/> Manual mirror
            </button>
            <button className="assistance-trigger" aria-expanded={showAssistance} aria-controls="inline-assistance" onClick={() => changeAssistance(!showAssistance)}>
              <Sparkles size={16} aria-hidden="true"/> AI Assistance
            </button>
            <button className="primary next-button" aria-disabled={loading} onClick={() => { if (!loading) void nextAttention(); }}>
              <span>Next needing attention</span><ArrowRight size={16}/>
            </button>
          </section>
        </div>
        <div ref={toolbarRef} id="review-toolbar" className="review-toolbar" data-mode={toolbar}>
          {toolbar !== 'expanded' && <div className="review-compact-tools">
            <button ref={compactTrigger} aria-expanded={toolbar === 'path'} aria-controls="review-path-tools"
              onClick={() => toolbar === 'path' ? setToolbar('compact') : setToolbar('path', true)}><FolderOpen size={15}/>Path</button>
            <button aria-expanded={toolbar === 'search'} aria-controls="review-search-tools"
              onClick={() => toolbar === 'search' ? setToolbar('compact') : setToolbar('search', true)}><Search size={15}/>Search</button>
            <span title={view.path}>{view.path}</span>
            <button className="icon-button" aria-label="Expand path and search tools" title="Expand path and search tools"
              onClick={() => setToolbar('expanded', true)}><ChevronsDown size={16}/></button>
          </div>}
        <div id="review-path-tools" hidden={toolbar !== 'expanded' && toolbar !== 'path'}>
        <div className="path-row"><button className="icon-button" disabled={!canUp} aria-label="Back to parent folder" title="Back to parent folder" onClick={() => backFolder()}><ArrowLeft size={17}/></button>
          <button className="icon-button" disabled={!driveHome || !canUp} aria-label="Go to drive home" title={driveHome ? `Drive home · ${driveHome}` : 'Drive home'} onClick={() => { if (driveHome) navigateByKey(driveHome); }}><Home size={16}/></button>
          <form className="path-form" onSubmit={event => { event.preventDefault(); navigate(pathDraft); }}><FolderOpen size={17}/><input ref={pathInputRef} aria-label="Current folder path" value={pathDraft} spellCheck={false} onChange={event => setPathDraft(event.target.value)}/><button type="submit" aria-label="Open folder path" title="Open path"><ArrowRight size={16}/></button></form><button className="icon-button" aria-label="Rescan current folder" title="Refresh immediate contents; keep decisions" disabled={loading} onClick={() => { void refreshVisible().catch(report); }}><RefreshCw size={17} className={loading ? 'spin' : ''}/></button>
          {toolbar === 'expanded' && <button ref={expandedTrigger} className="icon-button" aria-label="Compact path and search tools"
            title="Compact path and search tools" onClick={() => setToolbar('compact', true)}><ChevronsUp size={16}/></button>}
        </div>
        <div className="breadcrumb-frame"><nav className="breadcrumbs" ref={breadcrumbRef} aria-label="Folder breadcrumbs">{view.path.replace(/\\$/, '').split('\\').map((part, index, parts) => <span key={`${index}-${part}`}><button title={parts.slice(0, index + 1).join('\\')} onClick={() => navigate(index === 0 ? `${part}\\` : parts.slice(0, index + 1).join('\\'))}>{part || '\\'}</button>{index < parts.length - 1 && <ChevronRight size={11}/>}</span>)}</nav><OverflowIndicators targetRef={breadcrumbRef} axis="horizontal" refreshKey={view.path}/></div>
        </div>
        <div id="review-search-tools" className="browser-tools" hidden={toolbar !== 'expanded' && toolbar !== 'search'} onKeyDown={toolbarKey}><label className="search-field"><Search size={16}/><input ref={searchRef} aria-label="Search in current folder" placeholder="Search this folder…" value={view.search} onChange={event => { setKnownResults(null); setKnownSearch(false); updateView({ search: event.target.value, page: 0, scrollTop: 0 }); }}/>{view.search && <button aria-label="Clear search" onClick={() => { setKnownResults(null); setKnownSearch(false); updateView({ search: '', page: 0, scrollTop: 0 }, true); }}><X size={14}/></button>}</label>
          <button className="known-search-button" ref={knownButtonRef} title={view.search.trim() ? "Search directory names discovered so far, up to 500 results" : "Type a folder name in Search to find known folders"} onClick={() => void searchKnown()} aria-disabled={!view.search.trim()}><Search size={14}/>Known folders</button>
          <ReviewFilter ref={filterRef} value={view.filter} onChange={filter => {
            setKnownResults(null);
            setKnownSearch(false);
            updateView({ filter, page: 0, scrollTop: 0 }, true);
          }} onMoveLeft={() => knownButtonRef.current?.focus()} onEscape={focusDrive}/>
        </div>
          <ResizeGrip axis="y" controls="review-toolbar" label="Resize path and search tools" min={36} max={230} step={230}
            size={() => toolbarRef.current?.getBoundingClientRect().height ?? 36}
            edge={(_, raw) => raw <= 56} edgeLabel="Release for compact path and search tools"
            onCommit={height => setToolbar(height <= 56 ? 'compact' : 'expanded', true)}/>
        </div>
        {showAssistance && <AssistanceInterview currentPath={view.path} revision={Math.max(document.revision, observedRevision)}
          scanSignal={`${appStatus.scan.status}:${appStatus.mirrorEvidenceStamp}`}
          readOnly={readOnly} onEdit={editAssistance} controlRef={assistanceControl}
          footerHost={assistanceFooter} inspectQuestion={inspectAssistanceQuestion} browser={assistanceBrowser}
          deferMarked={document.inventoryDeferMarked !== false}
          onOpenMirror={path => { setMirrorReviewPath(path); setShowMirror(true); }}/>}
        <div className="browse-interaction-hint"><span>Click to preview · focus button or row menu to center</span><span>Ctrl+Space selects · Space marks</span></div>
        {groupTargets.length > 0 && <div className="review-selection-bar" role="group" aria-label="Grouped review selection">
          <span aria-live="polite"><strong>{groupTargets.length} selected</strong> · {visibleGroupCount} shown · {groupTargets.length - visibleGroupCount} hidden</span>
          <div className="selection-actions">
            <button type="button" disabled={readOnly} aria-haspopup="dialog" onClick={() => { void prepareGroupReview().catch(report); }}>Review selection</button>
            <button type="button" disabled={readOnly} onClick={() => {
              try {
                assertSelectionEditable();
                if (selectionRoot) setRootSelection(selectionRoot.id, []);
                columnStripRef.current?.querySelector<HTMLButtonElement>('.root-column .column-review')?.focus();
              } catch (cause) { report(cause); }
            }}>Clear selection</button>
          </div>
        </div>}
        <div className="folder-strip-frame" data-column-filled={fillDepth !== null || undefined}><div className="folder-columns-strip" ref={columnStripRef} aria-label="Folder preview columns" onScroll={event => { if (fillDepth === null && !appRef.current?.hasAttribute('data-resize-active') && !isRestoredScroll(event.currentTarget, 'scrollLeft')) updateView({ previewScrollLeft: event.currentTarget.scrollLeft }); }}>
          <DirectoryColumn {...columnActions} previewPath={view.previewColumns[0]?.path} path={view.path} depth={0} page={view.page} scrollTop={view.scrollTop} width={view.browserColumnWidth ?? 360} listing={listing} loading={loading} error={!loading && !listing ? 'This location could not be read. Its coverage gap is recorded; you can continue elsewhere.' : null} knownResults={knownResults} knownSearch={knownSearch} listRef={listRef} onBack={canUp ? () => backFolder() : undefined} onScroll={scrollTop => updateView({ scrollTop })} onPage={page => updateView({ page, scrollTop: 0 }, true)} onRefresh={() => { void refreshVisible().catch(report); }}/>
          {fillDepth === null && !view.previewColumns.length && <div className="preview-placeholder" aria-hidden="true"><ChevronRight size={27}/><h3>Look one layer deeper</h3><p>Click a folder to preview its contents in the next column.</p><p>Right-click for the folder actions and decision menu.</p></div>}
          {view.previewColumns.slice(0, fillDepth ?? undefined).map((column, index) => <PreviewFolderColumn key={reviewPathKey(column.path)} {...columnActions} previewPath={view.previewColumns[index + 1]?.path} column={column} depth={index + 1} cache={folderReads} cacheContext={{ derived: folderReadScope, source: folderSourceScope }} refreshToken={folderRefreshToken} onChange={patch => changePreview(index + 1, patch)} onBack={() => backFolder(index + 1)} backPreservesBranch={canUp} onClose={() => closePreview(index + 1)}/>)}
        </div>
        <OverflowIndicators targetRef={columnStripRef} axis="horizontal" refreshKey={view.previewColumns}/>
        {fillDepth !== null && <button ref={restoreColumnsRef} className="review-restore-columns"
          aria-label="Restore folder columns" title="Restore folder columns" onClick={() => {
            columnFocus.current = fillDepth;
            setLayout({ fillDepth: null });
          }}><ChevronLeft size={20} aria-hidden="true"/></button>}
        </div>
        {showAssistance && <div ref={setAssistanceFooter} className="inline-assistance-footer"/>}
      </section>
      <footer className="workspace-footer"><span><ShieldCheck size={13}/>Copy runs use saved marks and the selected destination.</span><span>Enter: open · →: preview · ←: close preview</span></footer>
    </main>
    <ReviewSidebar side="decision" name="decision panel" collapsed={Boolean(layout.decisionCollapsed)} app={() => appRef.current}
      onResize={(width, collapse) => setLayout(collapse ? { decisionCollapsed: true } : { decisionWidth: width })}
      onToggle={decisionCollapsed => setLayout({ decisionCollapsed })}>
    <aside className="details-panel">
      <div className="details-header"><span className="eyebrow">YOUR DECISION</span><button className="icon-button" aria-label="Undo last decision or note" title="Undo last decision or note (Ctrl+Z)" disabled={readOnly || saveState.saving || !document.history.length} onClick={() => void undo()}><Undo2 size={17}/></button></div>
      <div className="details-controls">
        <div className="selected-title"><div className="selected-icon">{selectedEntry?.kind === 'file' ? <File size={26}/> : selectedEntry?.kind === 'link' ? <Link2 size={26}/> : <Folder size={26}/>}</div><div><h2 title={selectedPath ?? ''}>{selectedPath ? baseName(selectedPath) : 'Select a folder'}</h2><p title={selectedPath ?? ''}>{selectedPath}</p></div></div>
        <div className="review-location-row">{selectedPath === view.path ? <span>Reviewing this folder itself</span> : <button className="text-button" onClick={() => updateView({ selectedPath: null }, true)}><ArrowLeft size={13}/>Review this folder instead</button>}{detailsLoading && <LoaderCircle size={13} className="spin" aria-label="Reading decision"/>}</div>
        <button className={`decision-picker-trigger status-${effective.status}`} aria-haspopup="dialog" aria-expanded={showDecisionPicker} disabled={classificationUnavailable} onClick={() => setShowDecisionPicker(true)}><ActiveIcon size={20}/><span><strong>{effective.manualEvidence === 'mirror-verified' ? reviewStatusLabel(effective) : LABELS[effective.status]}{effective.status === 'migrate-now' ? ' · pending' : ''}</strong><small>Change decision</small></span><ChevronRight size={17}/></button>
        <button className={`context-summary ${details?.exists === false || effective.stale || incompleteRollup ? 'warning' : ''}`} onClick={() => setShowContext(true)} title={contextSummary}><Info size={14}/><span>{contextSummary}</span><ChevronRight size={13}/></button>
        <div className="notes-section"><label className="notes-label" htmlFor="decision-notes">Notes <span>autosaved</span></label><textarea id="decision-notes" disabled={readOnly || !selectedPath || detailsLoading} placeholder="Exclusions, destination, exports needed…" value={noteText} onChange={event => { if (!selectedPath) return; const text = event.target.value; setNotes(current => ({ ...current, [selectedPath]: text })); noteBuffer.current.set(selectedPath, text); scheduleFlush(); }} rows={2}/></div>
        <div className="detail-actions"><button onClick={() => setShowContext(true)} title={pathSuggestions.length ? `${pathSuggestions.length} assistant proposals; separate from your decisions` : 'Review context and child exceptions'}><ListFilter size={14}/><span>Context{effective.mixed ? ` · ${effective.mixed}` : pathSuggestions.length ? ` · ${pathSuggestions.length} proposals` : ''}</span></button><button onClick={() => setShowHistory(true)}><History size={14}/><span>History</span></button></div>
        <p className="compact-safety">{effective.status === 'manual-pending'
          ? 'Manual copy is still pending. Confirm it after you finish copying.'
          : effective.status === 'manual-confirmed'
            ? 'Your confirmation for this path. Review children separately. Checksums and restore testing supply separate evidence.'
            : 'Purple = pending copy. Green = your manual F: assertion, unverified.'}</p>
      </div>
      <div className="data-location"><Database size={14}/><span title={boot.dataPath}>migration_master_data.json<small>Native JSON · version {document.schemaVersion} · revision {document.revision}</small></span></div>
    </aside>
    </ReviewSidebar>
    {rowMenu && <RowClassificationMenu key={rowMenu.id} target={rowMenu} disabledReason={rowMenuDisabled} onDismiss={dismissRowMenu}
      onCenterFolder={rowMenu.entry.kind === 'folder' && !rowMenu.entry.problem ? () => centerFolder(rowMenu.entry.path) : undefined}
      onChoose={status => { const target = rowMenu; setSelectedEntryInfo(target.entry); dismissRowMenu(true); void classifyPath(target.entry.path, status).then(() => { if (target.invoker && !target.invoker.isConnected) window.document.querySelector<HTMLButtonElement>('.decision-picker-trigger')?.focus(); }); }}/>}
    {message && <div className="toast" role="status"><CheckCircle2 size={16}/><span>{message}</span><button onClick={() => setMessage(null)} aria-label="Dismiss status"><X size={14}/></button></div>}
    {showMcp && <Modal title="Local MCP gateway" compact onClose={() => setShowMcp(false)}><div className={`mcp-state-line mcp-${gateway.state}`}><Link2 size={19}/><strong>{gateway.state === 'running' ? 'MCP ready' : gateway.state === 'error' ? 'Gateway needs attention' : 'Gateway unavailable'}</strong></div><p className="modal-intro">Keep the native app open while using an MCP client. This indicator shows the local gateway’s readiness.</p>{gateway.problem && <div className="context-box warning"><TriangleAlert size={16}/><p>{gateway.problem}</p></div>}{gateway.endpoint && <label className="gateway-endpoint">Local endpoint<input aria-label="Local MCP gateway endpoint" readOnly value={gateway.endpoint}/></label>}<p className="modal-intro gateway-instructions">Register the included local stdio server with your MCP client. The bridge shares the app’s saved decisions and revision checks. Agent proposals remain separate from decisions; accepted edits retain their provenance.</p><details className="gateway-diagnostics" open={gatewayDetailsOpen} onToggle={event => setGatewayDetailsOpen(event.currentTarget.open)}><summary>Session details</summary>{gateway.startedAt && <p>Gateway started: {new Date(gateway.startedAt).toLocaleString()}</p>}{gateway.instanceId && <p>Instance: {gateway.instanceId}</p>}<p>Authentication credentials are never displayed here.</p></details></Modal>}
    {showConflict && <Modal title="Saved-state conflict" onClose={closeConflictReview}><p className="modal-intro">Another reviewer saved a newer version while your edit was pending. The newer file was not overwritten. Your pending requests below remain in this window and will not be replayed automatically. Compare both versions, then explicitly choose whether to apply your pending edits to the displayed revision.</p><div className="context-box warning"><TriangleAlert size={17}/><p>Keep this window open while resolving the conflict. Pending edits are still held in memory. You can select and copy this text to preserve a separate record before any manual recovery.</p></div><p className="conflict-revisions">This window last acknowledged revision {queue.current?.revision ?? document.revision}.{externalDocument ? ` The newer saved version is revision ${externalDocument.revision}.` : ' The latest saved revision will be read on the next status check.'}</p><label className="conflict-draft-label">Retained pending requests<textarea aria-label="Retained pending requests" readOnly value={JSON.stringify(queue.current?.retainedOperations ?? [], null, 2)}/></label>{externalDocument && <div className="conflict-saved-summary"><h3>Newer saved decisions for pending paths</h3>{Array.from(new Set((queue.current?.retainedOperations ?? []).flatMap(operation => 'path' in operation ? [operation.path] : []))).map(path => { const decision = Object.values(externalDocument.decisions).find(decision => resolveDecisionPath(decision, boot.volumes).path?.toLowerCase() === path.toLowerCase()); return <article key={path}><strong>{path}</strong><p>{decision ? LABELS[decision.status] : 'No explicit saved decision'}</p>{decision?.notes && <p>{decision.notes}</p>}</article>; })}</div>}{externalDocument && (queue.current?.retainedOperations ?? []).some(operation => operation.type === 'view') && <div className="conflict-saved-summary"><h3>Saved review position in this revision</h3><p>{externalDocument.view.path}</p><p>Selection: {externalDocument.view.selectedPath ?? 'folder itself'} · filter: {externalDocument.view.filter} · search: {externalDocument.view.search || '(empty)'}</p></div>}{externalDocument && (queue.current?.retainedOperations ?? []).some(operation => operation.type === 'undo') && <ReviewedUndoTarget document={externalDocument}/>}<div className="conflict-recovery-actions"><button disabled={conflictLoading || !queue.current?.state.error} onClick={() => void loadConflictComparison()}><RefreshCw size={14}/>{conflictLoading ? 'Reading saved version…' : 'Refresh comparison'}</button><button className="primary" disabled={conflictLoading || !externalDocument || !queue.current?.state.error || (externalDocument?.revision ?? 0) < (queue.current?.revision ?? 0)} onClick={() => { void applyReviewedPending().catch(() => {}); }}>Apply my pending edits{(queue.current?.retainedOperations ?? []).some(operation => operation.type === 'undo') ? ' + reviewed Undo' : ''} to revision {externalDocument?.revision ?? '…'}</button></div>{conflictProblem && <div className="context-box warning" role="alert"><TriangleAlert size={17}/><p>{conflictProblem}</p></div>}<p className="modal-intro conflict-footnote">Applying uses the displayed revision and the original request identifiers. Unrelated saved fields stay intact. For fields changed in both versions, this action deliberately chooses your pending edit. If the saved file changes again, the revision check stops the edit for another review.</p><p className="modal-intro">No pending request is discarded and no revision fence is bypassed by closing this panel.</p></Modal>}
    {showDecisionPicker && <Modal title="Choose a migration decision" compact onClose={() => setShowDecisionPicker(false)}><p className="picker-intro">Your choice is autosaved. It never copies or deletes source files.</p><div className="decision-choices" role="group" aria-label="Migration decision">{DECISION_CHOICES.map(status => { const Icon = ICONS[status]; const isSelected = isDecisionChoiceSelected(effective, status); return <button key={status} className={`classification-button status-${status} ${isSelected ? 'selected' : ''}`} aria-pressed={isSelected} disabled={classificationUnavailable} onClick={() => { setShowDecisionPicker(false); if (!isSelected) void classify(status); }}><Icon size={18}/><span><strong>{decisionChoiceLabel(status)}</strong><small>{DESCRIPTIONS[status]}</small></span>{isSelected && <Check size={16}/>}</button>; })}</div></Modal>}
    {showContext && <Modal title="Review context & exceptions" onClose={() => setShowContext(false)}><p className="context-path">{selectedPath}</p><StatusBadge effective={effective}/><RollupCoverageDetails effective={effective}/><AuditDetails stats={details?.audit ?? selectedEntry?.audit} logicalFallback={selectedEntry?.kind === 'file' ? selectedEntry.size : undefined}/>{agentDecision && <div className="context-box"><Link2 size={16}/><div><p>Saved through MCP by {typeof activeProvenance?.actor === 'string' ? activeProvenance.actor : 'an agent'}{typeof activeProvenance?.at === 'string' ? ` · ${new Date(activeProvenance.at).toLocaleString()}` : ''}.</p>{typeof activeProvenance?.reason === 'string' && <p>{activeProvenance.reason}</p>}</div></div>}{pathSuggestions.length > 0 && <section className="assistant-proposals"><h3>Assistant proposals · not applied</h3><p>Suggestions are separate from saved decisions. No proposal below changes a classification.</p>{pathSuggestions.map(suggestion => <article key={suggestion.id}><StatusBadge effective={{ ...EMPTY_EFFECTIVE, status: suggestion.status }}/><p>{suggestion.reason}</p><small>{suggestion.actor} · MCP suggestion{suggestion.createdAt ? ` · ${new Date(suggestion.createdAt).toLocaleString()}` : ''}</small></article>)}</section>}{effective.inheritedFrom && <div className="context-box"><Info size={16}/>{effective.originalStatus === 'on-external' ? <p>The parent <strong>{effective.inheritedFrom}</strong> has a manual F: assertion. This child needs its own review. Backup assertions are not inherited.</p> : effective.originalStatus === 'manual-confirmed' ? <p>The parent <strong>{effective.inheritedFrom}</strong> has your manual-backup confirmation for that path. Review this child separately.</p> : <p>Inherited from <strong>{effective.inheritedFrom}</strong>. Choosing a decision creates a specific child exception.</p>}</div>}{effective.stale && <div className="context-box warning"><TriangleAlert size={16}/><p>Source metadata changed or new content was discovered after the decision. Review it again. The earlier claim remains in history.</p></div>}{details && !details.exists && <div className="context-box warning"><TriangleAlert size={16}/><p>This path is missing or unavailable. Its saved decision is retained. You can edit notes; classification is unavailable until the path can be read again.</p></div>}{selectedPath !== view.path && !selectedEntry && !loading && <p className="modal-intro">The selected path is outside the current page or filter. Its decision remains open.</p>}{activeDecision?.decidedAt && <p className="decision-date">Decision recorded {new Date(activeDecision.decidedAt).toLocaleString()}</p>}<div className="evidence-note"><ShieldCheck size={18}/><p>Manual marks record your pending copy or your confirmation. Source checksums, destination-copy receipts and restore checks provide separate evidence. The green Already on F: mark retains your manual F: assertion.</p></div>{details && details.overrides.length > 0 ? <div className="overrides"><h3>Specific child decisions · {details.overrides.length} recorded</h3>{details.overrides.map((decision, index) => { const location = resolveDecisionPath(decision, boot.volumes); return <div key={`${decision.path}-${index}`}><button disabled={!location.path} title={decision.path} onClick={() => { if (!location.path) return; setShowContext(false); const parent = parentPath(location.path); navigate(parent ?? location.path, parent ? location.path : null); }}><span className={`decision-dot status-${decision.status}`}/><span>{decision.path}</span><ChevronRight size={13}/></button>{location.explanation && <p className="unavailable-record">{location.explanation}</p>}</div>; })}</div> : <p className="modal-intro context-empty">No specific child exceptions are recorded for this path.</p>}</Modal>}
    {showGaps && <Modal title="Coverage gaps" onClose={() => setShowGaps(false)}><div className="audit-overview"><h3>Current or latest source audit</h3><p>Phase: {audit.status?.phase ?? 'unavailable'}{audit.status?.workers ? ` · ${audit.status.workers} workers` : ''}{audit.status?.coverageGaps ? ` · ${audit.status.coverageGaps.toLocaleString()} audit coverage gaps` : ''}</p>{audit.status?.currentPath && <p className="context-path">{audit.status.currentPath}</p>}{audit.problem && <p className="audit-detail-problem">{audit.problem}</p>}<AuditDetails stats={audit.status ?? undefined}/><AuditCoverageDetails status={audit.status}/></div><div className="legacy-index-tools"><h3>Directory index only</h3><p>The directory index skips known generated folders by default. Count &amp; hash runs a separate source audit.</p>
      {!scanRunning && <label className="follow-ai-control">
        <input type="checkbox" checked={includeGeneratedForIndex} onChange={event => setIncludeGeneratedForIndex(event.target.checked)}/>
        <span>Include generated folders in the next new index</span>
      </label>}
      <div>{scanRunning ? <button onClick={() => void scan('pause')}><Pause size={13}/>Pause directory index</button> : appStatus.scan.pending > 0 ? <button onClick={() => void scan('resume')}><Play size={13}/>Resume directory index</button> : <button onClick={() => void scan('all')}><Play size={13}/>Index all roots</button>}{!scanRunning && <button onClick={() => void scan('current')}>Index this subtree</button>}</div></div><p className="inventory-detail-counts">Inventory: {(appStatus.scan.processed ?? 0).toLocaleString()} folders processed · {(appStatus.scan.files ?? 0).toLocaleString()} files · {readableBytes(appStatus.scan.bytes ?? 0)} counted · {appStatus.scan.pending.toLocaleString()} folders queued
      {Boolean(appStatus.scan.skippedGenerated) && <> · {appStatus.scan.skippedGenerated?.toLocaleString()} generated folders skipped</>}
      </p><p className="modal-intro">This list shows up to 200 entries, including a sample of skipped generated folders and the latest filesystem gaps.</p>{appStatus.scan.error && <div className="context-box warning"><TriangleAlert size={16}/><p>Inventory error: {appStatus.scan.error}</p></div>}{appStatus.scan.currentPath && <p className="context-path">Current or most recent inventory path: {appStatus.scan.currentPath}</p>}{appStatus.gaps.length ? appStatus.gaps.map((gap, index) => <article className="gap-row" key={`${gap.path}-${index}`}><TriangleAlert size={17}/><div><strong>{gap.path}</strong><p>{gap.problem}</p><small>{gap.observedAt ? new Date(gap.observedAt).toLocaleString() : 'Observation time unavailable'}</small></div><button onClick={() => { setShowGaps(false); const parent = parentPath(gap.path); navigate(parent ?? gap.path, parent ? gap.path : null); }}>Review</button></article>) : <div className="empty-state"><ShieldCheck size={28}/><h3>No gaps recorded yet</h3><p>This does not establish complete coverage. Inventory the drives to discover what can and cannot be read.</p></div>}</Modal>}
    {showHistory && <Modal title="Decision history" onClose={() => setShowHistory(false)}><p className="modal-intro">Changes for {selectedPath}. Earlier decisions stay in the saved journal, including undo events.</p>{details?.history.length ? details.history.slice(0, 100).map((item, index) => <HistoryEntry key={index} value={item} onGroupUndo={readOnly ? undefined : groupId => { void prepareGroupUndo(groupId, true).catch(report); }}/>) : <div className="empty-state"><History size={28}/><h3>No changes recorded for this path</h3></div>}</Modal>}
    {showSaved && <Modal title="Saved decisions & notes" onClose={() => setShowSaved(false)}><p className="modal-intro">Every saved record remains here, including paths that were moved, removed or belong to a previous volume. Availability is checked when you open a path. A previous disk’s decision is never applied to a different disk using the same drive letter.</p><div className="saved-tools"><input aria-label="Search saved decisions and notes" placeholder="Search saved paths and notes…" value={savedSearch} onChange={event => { setSavedSearch(event.target.value); setSavedPage(0); }}/><select aria-label="Filter saved records" value={savedFilter} onChange={event => { setSavedFilter(event.target.value); setSavedPage(0); }}><option value="all">All saved records</option>{STATUSES.map(status => <option value={status} key={status}>{LABELS[status]}</option>)}</select></div><p className="saved-result-count">{savedDecisions.length.toLocaleString()} matching saved records · current availability not yet checked</p>{savedDecisions.slice(savedPage * 100, (savedPage + 1) * 100).map((decision, index) => { const location = resolveDecisionPath(decision, boot.volumes); return <article className="saved-record" key={`${decision.path}-${index}`}><div><strong>{decision.path}</strong><StatusBadge effective={{ ...EMPTY_EFFECTIVE, status: decision.status, own: true }}/>{decision.noteOnly === true && <small>Note only · classification not set</small>}{decision.notes && <p>{decision.notes}</p>}{decision.decidedAt && <small>Recorded {new Date(decision.decidedAt).toLocaleString()}</small>}</div><div className="saved-review-action"><button disabled={!location.path} onClick={() => { if (!location.path) return; setShowSaved(false); const parent = parentPath(location.path); navigate(parent ?? location.path, parent ? location.path : null); }}>Review path</button>{location.explanation && <p className="unavailable-record">{location.explanation}</p>}{location.path && location.path.toLowerCase() !== decision.path.toLowerCase() && <small>Current location: {location.path}</small>}</div></article>; })}{!savedDecisions.length && <div className="empty-state"><Database size={28}/><h3>No matching saved records</h3><p>New decisions and notes are captured as you review folders.</p></div>}<div className="saved-pagination"><button disabled={savedPage === 0} onClick={() => setSavedPage(page => page - 1)}>Previous 100</button><span>Page {savedPage + 1} / {Math.max(1, Math.ceil(savedDecisions.length / 100))}</span><button disabled={(savedPage + 1) * 100 >= savedDecisions.length} onClick={() => setSavedPage(page => page + 1)}>Next 100</button></div></Modal>}
    {showMirror && <Modal title="Manual mirror comparison" onClose={() => { setShowMirror(false); setMirrorReviewPath(null); }}>
      <MirrorReview currentPath={mirrorReviewPath ?? view.path} instanceId={gateway.instanceId} enabled={!readOnly}
        controlRef={mirrorControl} onNativeWrite={runNativeWrite} onSave={saveMirror}/>
    </Modal>}
    {groupReview && <Modal title={groupReview.mode === 'mark' ? 'Review selected entries' : 'Review group Undo'}
      closeDisabled={groupBusy || groupAttempt !== null} onClose={closeGroupReview}
      fallbackFocus={() => columnStripRef.current?.querySelector<HTMLButtonElement>('.root-column .column-review')?.focus()}>
      <SelectionReview key={groupReview.operationId} review={groupReview} attempted={groupAttempt} busy={groupBusy}
        error={groupProblem} onSubmit={operation => { void applyGroup(operation); }}
        onRefresh={() => { void refreshGroupReview(); }} onRemove={removeGroupTarget}/>
    </Modal>}
  </div>;
}

function HistoryEntry({ value, onGroupUndo }: { value: unknown; onGroupUndo?: (groupId: string) => void }) {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return <article className="human-history"><strong>Earlier journal entry</strong><p>Retained in your data file.</p></article>;
  const item = record(value);
  const before = typeof item.before === 'object' && item.before !== null && !Array.isArray(item.before) ? record(item.before) : null;
  const after = typeof item.after === 'object' && item.after !== null && !Array.isArray(item.after) ? record(item.after) : null;
  const beforeLabel = before && isStatus(before.status) ? LABELS[before.status] : 'No explicit decision';
  const afterLabel = after && isStatus(after.status) ? LABELS[after.status] : 'No explicit decision';
  const groupCount = typeof item.memberCount === 'number' && Number.isSafeInteger(item.memberCount) ? item.memberCount : null;
  const groupSuffix = groupCount === null ? '' : ` · ${groupCount} entries`;
  const label = item.type === 'bulkClassify' ? `Group decision saved${groupSuffix}`
    : item.type === 'undoGroup' ? `Group decision undone${groupSuffix}`
    : item.type === 'note' ? 'Notes updated' : item.type === 'undo' ? 'Previous change undone' : 'Decision changed';
  const provenance = typeof item.provenance === 'object' && item.provenance !== null && !Array.isArray(item.provenance) ? record(item.provenance) : null;
  const origin = provenance?.kind === 'agent-mcp' || provenance?.kind === 'mcp-agent' ? `${typeof provenance.actor === 'string' ? provenance.actor : 'Agent'} · MCP` : 'Manual app edit';
  const groupId = item.type === 'bulkClassify' && item.undoneAt == null && typeof item.id === 'string' ? item.id : null;
  return <article className="human-history"><div><strong>{label}</strong>{typeof item.at === 'string' && <small>{new Date(item.at).toLocaleString()}</small>}</div><small className="history-origin">{origin}</small>{item.type === 'note' ? <p>{after && typeof after.notes === 'string' && after.notes ? after.notes : 'Notes cleared'}</p> : <p>{beforeLabel}<ArrowRight size={12}/>{afterLabel}</p>}{typeof item.undoneAt === 'string' && <small>This change was later undone.</small>}
    {groupId && onGroupUndo && <button type="button" onClick={() => onGroupUndo(groupId)}>Review group Undo</button>}
  </article>;
}

function ReviewedUndoTarget({ document }: { document: Document }) {
  const event = latestUndoEvent(document);
  if (!event) return <div className="context-box warning"><TriangleAlert size={16}/><p>This revision has no eligible history change to undo. The pending Undo cannot complete unless an eligible change exists.</p></div>;
  if (event.type === 'bulkClassify') return <div className="context-box warning"><TriangleAlert size={16}/><p>The latest saved change is a group. It requires reviewing and confirming Undo for the exact group.</p></div>;
  const previous = typeof event.before === 'object' && event.before !== null && !Array.isArray(event.before) ? record(event.before) : null;
  const current = typeof event.after === 'object' && event.after !== null && !Array.isArray(event.after) ? record(event.after) : null;
  return <div className="conflict-saved-summary"><h3>Undo latest saved change</h3><p>Applying pending Undo targets this reviewed change, which may be from another reviewer. A later saved change stops the request for another review.</p><strong>{typeof event.path === 'string' ? event.path : 'Path not recorded'}</strong><p>{event.type === 'note' ? 'Notes update' : 'Classification change'}{typeof event.at === 'string' ? ` · ${new Date(event.at).toLocaleString()}` : ''}{typeof event.id === 'string' ? ` · event ${event.id}` : ''}</p>{event.type === 'note' ? <p>Restore notes to: {typeof previous?.notes === 'string' ? previous.notes || '(empty)' : '(no earlier notes)'}</p> : <p>{isStatus(current?.status) ? LABELS[current.status] : 'No explicit decision'} → {isStatus(previous?.status) ? LABELS[previous.status] : 'No explicit decision'}</p>}</div>;
}

function Modal({ title, onClose, children, compact = false, closeDisabled = false, fallbackFocus }: {
  title: string; onClose: () => void; children: React.ReactNode; compact?: boolean;
  closeDisabled?: boolean; fallbackFocus?: () => void;
}) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const dialog = ref.current;
    const previous = window.document.activeElement;
    dialog?.showModal();
    return () => {
      dialog?.close();
      if (previous instanceof HTMLElement && previous.isConnected) previous.focus();
      else fallbackFocus?.();
    };
  }, []);
  return <dialog ref={ref} aria-label={title} className={`modal${compact ? ' compact-modal' : ''}`} onCancel={event => { event.preventDefault(); if (!closeDisabled) onClose(); }}><div className="modal-header"><h2>{title}</h2><button className="icon-button" aria-label={`Close ${title}`} disabled={closeDisabled} onClick={onClose}><X size={19}/></button></div><div className="modal-body">{children}</div></dialog>;
}
