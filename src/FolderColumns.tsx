import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import type { RefObject, KeyboardEvent as ReactKeyboardEvent } from 'react';
import { createPortal } from 'react-dom';
import { invoke } from '@tauri-apps/api/core';
import { ArrowLeft, Check, ChevronLeft, ChevronRight, File, Focus, Folder, FolderOpen, Link2, LoaderCircle, MoreHorizontal, RefreshCw, Search, Sparkles, Square, SquareCheck, TriangleAlert, X } from 'lucide-react';
import { baseName, parentPath, parseEntry } from './model';
import type { Entry, Listing, PreviewColumn, Status } from './model';
import type { FolderReadCache, FolderReadContext } from './folder-read-cache';
import { isStaleRead } from './read-cache';
import { DECISION_CHOICES, DESCRIPTIONS, ICONS, decisionChoiceLabel, isDecisionChoiceSelected, reviewStatusLabel, StatusBadge } from './classifications';
import { focusAdjacentRow } from './ReviewKeyboard';
import { OverflowIndicators } from './OverflowIndicators';
import { MAX_COLUMN, MIN_COLUMN, ResizeGrip } from './ReviewLayout';
import { allocatedLabel, auditDescription, AuditProgress } from './Audit';
import { isRestoredScroll, restoreReviewScroll, reviewActivityLabel, reviewPathKey, showReviewRow } from './ReviewActivity';
import type { ActivityRow, ReviewActivitySnapshot } from './ReviewActivity';

export interface RowMenuTarget { id: string; entry: Entry; x: number; y: number; invoker: HTMLElement | null }
export interface RowSuggestion { label: string; reason: string; source: 'ai' | 'local' }
export function localRowSuggestion(entry: Entry): RowSuggestion {
  if (entry.problem || !['file', 'folder'].includes(entry.kind)) {
    return { source: 'local', label: 'Review access', reason: 'Check this entry separately before relying on its coverage.' };
  }
  if (entry.effective.stale) {
    return { source: 'local', label: 'Review changes', reason: 'The entry changed since its saved decision.' };
  }
  if (entry.kind === 'folder' && /^(?:node_modules|_cacache|\.npm-cache|\.pnpm-store|__pycache__|\.next|\.nuxt|\.svelte-kit)$/i.test(entry.name)) {
    return { source: 'local', label: 'Check if rebuildable', reason: 'Inspect generated data for files you cannot recreate.' };
  }
  return { source: 'local', label: 'Keep for review', reason: 'Preserve this entry unless you can verify that you can recreate it.' };
}
export interface ColumnActions {
  selectedPath: string | null;
  menuPath: string | null;
  onSelect: (entry: Entry) => void;
  onToggleSelect: (entry: Entry) => void;
  canGroupSelect: (entry: Entry) => boolean;
  isGroupSelected: (entry: Entry) => boolean;
  onRowsDisplayed: (depth: number, folder: string, paths: readonly string[] | null) => void;
  onOpen: (path: string) => void;
  onOpenByKey: (path: string) => void;
  onPreview: (entry: Entry, depth: number) => void;
  onPreviewByKey: (entry: Entry, depth: number) => void;
  onCloseByKey: (depth: number) => void;
  onEscapeRows: () => void;
  onFirstAttention: (path: string, depth: number) => void;
  onResizeColumn: (depth: number, width: number, fill: boolean) => void;
  fillDepth: number | null;
  onMenu: (target: RowMenuTarget) => void;
  onReviewFolder: (path: string) => void;
  onReadPendingChange: (delta: number) => void;
  reviewActivity: ReviewActivitySnapshot;
  revealPath: string | null;
  revealVersion: number;
  suggestionFor?: (entry: Entry) => RowSuggestion;
  previewPath?: string | null;
}
interface DirectoryColumnProps extends ColumnActions {
  path: string;
  depth: number;
  page: number;
  width: number;
  listing: Listing | null;
  loading: boolean;
  scrollTop: number;
  error?: string | null;
  knownResults?: string[] | null;
  knownSearch?: boolean;
  listRef?: RefObject<HTMLDivElement | null>;
  onScroll: (scrollTop: number) => void;
  onPage: (page: number) => void;
  onClose?: () => void;
  onBack?: () => void;
  backPreservesBranch?: boolean;
  onRefresh: () => void;
}

export function DirectoryColumn(props: DirectoryColumnProps) {
  const ownRef = useRef<HTMLDivElement>(null);
  const columnRef = useRef<HTMLElement>(null);
  const listRef = props.listRef ?? ownRef;
  const listing = props.listing?.path.toLowerCase() === props.path.toLowerCase() ? props.listing : null;
  const pages = Math.max(1, Math.ceil((listing?.total ?? 0) / 100));
  const columnKey = reviewPathKey(props.path);
  const revealPath = props.revealPath;
  const revealKey = revealPath ? reviewPathKey(revealPath) : null;
  const revealParent = revealPath ? parentPath(revealPath) : null;
  const revealChild = revealParent !== null && reviewPathKey(revealParent) === columnKey;
  const includesReveal = listing?.entries.some(entry => reviewPathKey(entry.path) === revealKey);
  const showPin = Boolean(revealPath && revealChild && !includesReveal && !props.loading && !props.knownSearch);
  const [pin, setPin] = useState<{ path: string; entry: Entry | null } | null>(null);
  const pinned = showPin && pin?.path === revealPath ? pin.entry : null;
  const entries = pinned ? [pinned, ...(listing?.entries ?? [])] : listing?.entries ?? [];
  const revealedVersion = useRef<number | null>(null);
  const headingRef = useRef<HTMLButtonElement>(null);
  const columnId = 'review-folder-column-' + props.depth;
  const pinErrorRef = useRef<HTMLDivElement>(null);
  const folderActivity = props.reviewActivity.paths.get(columnKey);
  const folderActivityLabel = reviewActivityLabel(folderActivity, props.reviewActivity.nowMs);
  const { depth, path, knownSearch, onRowsDisplayed } = props;

  useLayoutEffect(() => {
    const paths = knownSearch ? [] : (listing?.entries ?? []).map(entry => entry.path);
    if (pinned && !knownSearch) paths.unshift(pinned.path);
    onRowsDisplayed(depth, path, paths);
    return () => onRowsDisplayed(depth, path, null);
  }, [depth, path, knownSearch, onRowsDisplayed, listing?.entries, pinned]);

  useLayoutEffect(() => {
    const list = listRef.current;
    if (list && !props.loading && listing) restoreReviewScroll(list, 'scrollTop', props.scrollTop);
  }, [listing?.path, props.page, props.loading, props.scrollTop, listRef]);

  useEffect(() => {
    if (!showPin || !revealPath) return;
    let active = true;
    void invoke<unknown>('get_review_entry', { path: revealPath }).then(parseEntry).then(entry => {
      if (reviewPathKey(entry.path) !== revealKey) throw new Error('The revealed path changed.');
      if (active) setPin({ path: revealPath, entry });
    }).catch(() => { if (active) setPin({ path: revealPath, entry: null }); });
    return () => { active = false; };
  }, [showPin, revealPath, revealKey, listing]);

  useEffect(() => {
    if (props.loading || !revealKey || revealedVersion.current === props.revealVersion) return;
    const frame = requestAnimationFrame(() => {
      const list = listRef.current;
      if (!list) return;
      let target: HTMLElement | null | undefined;
      if (props.depth === 0 && revealKey === columnKey && !revealChild) target = headingRef.current;
      else if (revealChild) {
        target = Array.from(list.querySelectorAll<HTMLElement>('[data-entry-kind]'))
          .find(row => row.dataset.entryPath && reviewPathKey(row.dataset.entryPath) === revealKey) ?? pinErrorRef.current;
      }
      if (!target) return;
      showReviewRow(list, target);
      revealedVersion.current = props.revealVersion;
    });
    return () => cancelAnimationFrame(frame);
  }, [columnKey, listing, listRef, pinned, pin, props.depth, props.loading, props.revealVersion, revealChild, revealKey]);

  function key(event: ReactKeyboardEvent<HTMLDivElement>): void {
    if (event.defaultPrevented || event.nativeEvent.isComposing || event.altKey || event.metaKey || event.shiftKey) return;
    const target = event.target;
    if (!(target instanceof HTMLElement)) return;
    const row = target.closest<HTMLElement>('.entry-row');
    const known = target.closest<HTMLButtonElement>('.known-result');
    if (target.classList.contains('column-back')) {
      if (event.key === 'ArrowDown') {
        event.preventDefault();
        listRef.current?.querySelector<HTMLButtonElement>('.entry-select, .known-result')?.focus();
      }
      return;
    }
    if (!row && !known) return;
    const entry = entries.find(entry => entry.path === row?.dataset.entryPath);
    const accept = (): void => { event.preventDefault(); event.stopPropagation(); };
    if (event.ctrlKey) {
      if (event.key === ' ' && entry) { accept(); if (!event.repeat) props.onToggleSelect(entry); }
      return;
    }
    if (event.key === 'Escape') {
      accept();
      props.onEscapeRows();
    } else if (event.key === 'ArrowRight' && entry?.kind === 'folder') {
      accept();
      if (!event.repeat) props.onPreviewByKey(entry, props.depth);
    } else if (event.key === 'ArrowLeft' && props.depth > 0) {
      accept();
      if (!event.repeat) props.onCloseByKey(props.depth);
    } else if (event.key === 'Enter' && target.classList.contains('entry-select') && entry?.kind === 'folder') {
      accept();
      if (!event.repeat) props.onOpenByKey(entry.path);
    } else if (event.key === 'Enter' && known?.dataset.entryPath) {
      accept();
      if (!event.repeat) props.onOpenByKey(known.dataset.entryPath);
    } else if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      accept();
      const current = row?.querySelector<HTMLButtonElement>('.entry-select') ?? known;
      if (!current) return;
      const button = focusAdjacentRow(listRef.current, current, event.key === 'ArrowDown' ? 1 : -1);
      if (!button && event.key === 'ArrowUp') listRef.current?.querySelector<HTMLButtonElement>('.column-back')?.focus();
      const next = entries.find(item => item.path === button?.dataset.entryPath);
      if (next) props.onSelect(next);
    }
  }
  function headingKey(event: ReactKeyboardEvent<HTMLButtonElement>): void {
    if (event.nativeEvent.isComposing || event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
    if (event.key === 'Escape') {
      event.preventDefault();
      props.onEscapeRows();
    } else if (event.key === 'ArrowDown') {
      event.preventDefault();
      if (!event.repeat) props.onFirstAttention(props.path, props.depth);
    } else if (event.key === 'ArrowLeft' && props.depth > 0) {
      event.preventDefault();
      if (!event.repeat) props.onCloseByKey(props.depth);
    }
  }
  return <section ref={columnRef} id={columnId} className={`folder-column ${props.depth === 0 ? 'root-column' : 'preview-column'}`} data-column-depth={props.depth} data-fill={props.fillDepth === props.depth ? 'true' : undefined} data-column-path={props.path} data-column-page={props.page} data-column-total={props.knownSearch ? props.knownResults?.length ?? 0 : listing?.total ?? 0} aria-label={props.depth ? `Preview folder ${props.path}` : `Current folder ${props.path}`} style={{ '--column-width': `${props.width}px` } as React.CSSProperties}>
    <div className="column-heading"><button ref={headingRef} className="column-review" onKeyDown={headingKey} aria-label={`Review folder ${props.path}`} data-review-activity={folderActivity?.pulse?.kind} title={`Review this folder: ${props.path}${folderActivityLabel ? ` · ${folderActivityLabel}` : ''}`} onClick={() => props.onReviewFolder(props.path)}><FolderOpen size={15}/><span><small>{props.depth ? `PREVIEW ${props.depth}` : 'CURRENT LOCATION'}{folderActivityLabel && ` · ${folderActivityLabel}`}</small><strong>{baseName(props.path)}</strong></span>{folderActivity?.pulse && <i key={folderActivity.pulse.sequence} className="review-activity-pulse" aria-hidden="true"/>}</button>{props.loading && <LoaderCircle size={14} className="spin" aria-label="Reading folder"/>}<button className="column-icon" title="Refresh immediate contents" aria-label={`Refresh column ${props.path}`} disabled={props.loading} onClick={props.onRefresh}><RefreshCw size={14}/></button>{props.onClose && <button className="column-icon" aria-label={`Close preview ${props.depth} and deeper previews`} onClick={props.onClose}><X size={15}/></button>}</div>
    <div className="column-path" title={props.path}>{props.path}</div>
    <div className="column-labels"><span>{props.knownSearch ? 'KNOWN FOLDERS' : 'FOLDER / FILE'}</span><span>{props.suggestionFor ? 'DECISION / SUGGESTION' : 'DECISION · ⋯ TO MARK'}</span></div>
    <div className="column-scroll-frame"><div className={`entry-list column-entry-list ${props.loading ? 'is-loading' : ''}`} ref={listRef} aria-label={`Contents of ${props.path}`} aria-busy={props.loading} onKeyDown={key} onScroll={event => { if (!isRestoredScroll(event.currentTarget, 'scrollTop')) props.onScroll(event.currentTarget.scrollTop); }}>
      {props.onBack && <button className="column-back" onClick={props.onBack}
        aria-label={props.depth && !props.backPreservesBranch ? 'Back from this preview' : 'Back to parent folder'}>
        <ArrowLeft size={15} aria-hidden="true"/><strong>Back</strong><span>{props.depth && !props.backPreservesBranch ? 'Close this preview' : 'Keep open folders'}</span>
      </button>}
      {props.loading && !listing && <div className="empty-state"><LoaderCircle size={24} className="spin"/><h3>Reading this folder…</h3><p>Loading immediate contents only.</p></div>}
      {props.knownSearch && props.knownResults ? props.knownResults.length ? props.knownResults.map(path => <button key={path} className="known-result" data-entry-path={path} data-entry-kind="folder" onClick={() => props.onOpen(path)}><Folder size={16}/><span>{path}</span><ChevronRight size={14}/></button>) : <div className="empty-state"><Search size={25}/><h3>No known folders match</h3></div> : entries.map(entry => {
        const entryKey = reviewPathKey(entry.path);
        return <EntryRow key={entryKey} entry={entry} selected={props.selectedPath !== null && reviewPathKey(props.selectedPath) === entryKey}
          groupSelected={props.isGroupSelected(entry)} canGroupSelect={props.canGroupSelect(entry)}
          menuOpen={props.menuPath === entry.path} pinned={entry === pinned} activity={props.reviewActivity.paths.get(entryKey)}
          nowMs={props.reviewActivity.nowMs} onSelect={() => props.onSelect(entry)} onGroupToggle={() => props.onToggleSelect(entry)}
          onOpen={() => props.onOpen(entry.path)} onPreview={() => props.onPreview(entry, props.depth)} onMenu={props.onMenu}
          suggestion={props.suggestionFor?.(entry)}
          previewing={reviewPathKey(props.previewPath ?? '') === entryKey}/>;
      })}
      {showPin && !pinned && pin?.path === revealPath && <div className="review-pin-error" ref={pinErrorRef}><strong>Revealed path unavailable</strong><span title={revealPath ?? ''}>{revealPath}</span></div>}
      {!props.knownSearch && listing && !listing.entries.length && <div className="empty-state"><FolderOpen size={25}/><h3>No matching entries</h3><p>This folder may be empty, or the current search and filter hide its entries.</p></div>}
      {props.error && !props.loading && <div className="column-error"><TriangleAlert size={23}/><h3>Folder could not be read</h3><p>{props.error}</p><p>The gap is retained. Other columns remain usable.</p><button onClick={props.onRefresh}>Try reading again</button></div>}
    </div><OverflowIndicators targetRef={listRef} axis="vertical" refreshKey={pinned ?? listing}/></div>
    <div className="column-footer"><span>{props.loading ? 'Reading…' : `${(listing?.total ?? 0).toLocaleString()} items`}</span>{!props.knownSearch && <div className="pagination"><button aria-label={`Previous page in ${props.path}`} disabled={!props.page || props.loading} onClick={() => props.onPage(Math.max(0, props.page - 1))}><ChevronLeft size={15}/></button><span>{props.page + 1} / {pages}</span><button aria-label={`Next page in ${props.path}`} disabled={props.page + 1 >= pages || props.loading} onClick={() => props.onPage(props.page + 1)}><ChevronRight size={15}/></button></div>}</div>
    {props.fillDepth !== props.depth && <ResizeGrip label={`Resize folder column ${props.depth + 1}`}
      controls={columnId} min={MIN_COLUMN} max={MAX_COLUMN} size={() => columnRef.current?.getBoundingClientRect().width ?? props.width}
      target={() => columnRef.current} variable="--drag-column-width" edgeLabel="Release to fill the folder pane"
      edge={(event, raw) => {
        const frame = columnRef.current?.closest('.folder-strip-frame')?.getBoundingClientRect();
        return Boolean(frame && raw > props.width + 6 && event.clientX >= frame.right - 46);
      }}
      onCommit={(width, fill) => props.onResizeColumn(props.depth, width, fill)}/>}
  </section>;
}

interface PreviewFolderProps extends ColumnActions {
  column: PreviewColumn;
  depth: number;
  refreshToken: string;
  cache: FolderReadCache;
  cacheContext: FolderReadContext;
  onChange: (patch: Partial<PreviewColumn>) => void;
  onClose?: () => void;
  onBack: () => void;
  backPreservesBranch: boolean;
}
export function PreviewFolderColumn(props: PreviewFolderProps) {
  const [listing, setListing] = useState<Listing | null>(() => props.cache.peekRecentListing({
    path: props.column.path, search: '', filter: 'all', page: props.column.page,
  }, props.cacheContext) ?? null);
  const [loading, setLoading] = useState(listing === null);
  const [error, setError] = useState<string | null>(null);
  const [refresh, setRefresh] = useState(0);
  const loadedContext = useRef<string | null>(null);
  const forceRefresh = useRef(false);
  const listRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    let active = true;
    const contextKey = JSON.stringify([props.column.path, props.column.page]);
    const foreground = loadedContext.current !== contextKey || forceRefresh.current;
    const query = { path: props.column.path, search: '', filter: 'all', page: props.column.page };
    const force = forceRefresh.current;
    const cached = !force ? props.cache.peekListing(query) : undefined;
    if (cached) { loadedContext.current = contextKey; setListing(cached); setLoading(false); setError(null); return; }
    const recent = !force ? props.cache.peekRecentListing(query) : undefined;
    if (recent) { setListing(recent); setLoading(false); setError(null); }
    else if (foreground) { setLoading(true); setError(null); }
    forceRefresh.current = false;
    props.onReadPendingChange(1);
    void props.cache.listing(query, force).then(result => {
      if (!active) return;
      loadedContext.current = contextKey; setListing(result); setLoading(false); setError(null);
    }).catch(cause => { if (active) { if (!isStaleRead(cause)) { if (foreground) setListing(null); setError(String(cause)); } setLoading(false); } }).finally(() => props.onReadPendingChange(-1));
    return () => { active = false; };
  }, [props.column.path, props.column.page, props.refreshToken, refresh]);
  return <DirectoryColumn {...props} path={props.column.path} width={props.column.width ?? 360} page={props.column.page} scrollTop={props.column.scrollTop} listing={listing} loading={loading} error={error} listRef={listRef} onScroll={scrollTop => props.onChange({ scrollTop })} onPage={page => props.onChange({ page, scrollTop: 0 })} onRefresh={() => { forceRefresh.current = true; setRefresh(value => value + 1); }}/>
}

interface EntryRowProps {
  entry: Entry;
  selected: boolean;
  groupSelected: boolean;
  canGroupSelect: boolean;
  menuOpen: boolean;
  pinned: boolean;
  activity?: ActivityRow;
  nowMs: number;
  suggestion?: RowSuggestion;
  previewing: boolean;
  onSelect: () => void;
  onGroupToggle: () => void;
  onOpen: () => void;
  onPreview: () => void;
  onMenu: (target: RowMenuTarget) => void;
}
function EntryRow({ entry, selected, groupSelected, canGroupSelect, menuOpen, pinned, activity, nowMs, suggestion, previewing,
  onSelect, onGroupToggle, onOpen, onPreview, onMenu }: EntryRowProps) {
  const Icon = entry.kind === 'folder' ? Folder : entry.kind === 'link' ? Link2 : entry.kind === 'unavailable' ? TriangleAlert : File;
  const activityLabel = reviewActivityLabel(activity, nowMs);
  const reviewLabel = [pinned ? 'Revealed · outside current results' : null, activityLabel].filter(Boolean).join(' · ');
  const suggestionLabel = suggestion ? (suggestion.source === 'ai' ? 'AI suggestion' : 'Local suggestion') : null;
  function menu(invoker: HTMLElement, x?: number, y?: number): void {
    const rectangle = invoker.getBoundingClientRect();
    onMenu({ id: crypto.randomUUID(), entry, x: x ?? rectangle.right, y: y ?? rectangle.bottom, invoker });
  }
  return <div className={`entry-row status-${entry.effective.status} ${selected ? 'selected' : ''}`} data-entry-path={entry.path} data-entry-name={entry.name} data-entry-kind={entry.kind} data-entry-status={entry.effective.rollup?.kind === 'mixed' ? 'mixed' : entry.effective.status} data-review-pinned={pinned || undefined} data-review-activity={activity?.pulse?.kind} onContextMenu={event => { event.preventDefault(); event.stopPropagation(); const target = event.target instanceof HTMLElement ? event.target.closest<HTMLElement>('button') : null; const invoker = target ?? event.currentTarget.querySelector<HTMLElement>('.entry-select') ?? event.currentTarget; menu(invoker, event.clientX || undefined, event.clientY || undefined); }} onKeyDown={event => {
    const target = event.target instanceof HTMLElement ? event.target.closest<HTMLElement>('button') : null;
    const space = event.key === ' ' && !event.ctrlKey && !event.metaKey && !event.altKey && !event.shiftKey && target?.classList.contains('entry-select');
    if (event.key === 'ContextMenu' || event.shiftKey && event.key === 'F10' || space) {
      event.preventDefault();
      event.stopPropagation();
      if (!event.repeat) menu(target ?? event.currentTarget);
    }
  }}>
    <button type="button" className="entry-group-toggle" aria-pressed={groupSelected} disabled={!canGroupSelect}
      aria-label={`Select ${entry.name} for group marking`}
      title={canGroupSelect ? 'Toggle group selection (Ctrl+Space)' : 'This entry cannot be added to a group'}
      onClick={onGroupToggle}>{groupSelected ? <SquareCheck size={15} aria-hidden="true"/> : <Square size={15} aria-hidden="true"/>}</button>
    <button className="entry-select" data-entry-path={entry.path} aria-current={selected ? 'true' : undefined}
      aria-expanded={entry.kind === 'folder' ? previewing : undefined}
      aria-label={`${entry.kind === 'folder' ? 'Preview folder' : 'Review file'} ${entry.name}, ${reviewStatusLabel(entry.effective)}${suggestion ? `, ${suggestionLabel}: ${suggestion.label}` : ''}${reviewLabel ? `, ${reviewLabel}` : ''}`}
      title={suggestion ? `${entry.path}\n${suggestionLabel}: ${suggestion.reason}` : `${entry.path}\n${auditDescription(entry.audit, entry.kind === 'file' ? entry.size : undefined)}${reviewLabel ? `\n${reviewLabel}` : ''}`}
      onClick={entry.kind === 'folder' ? onPreview : onSelect}>
      <span className="entry-icon"><AuditProgress stats={entry.audit} size={27}><Icon size={14}/></AuditProgress></span><span className="entry-name"><strong>{entry.name}</strong><small className="entry-allocation">{entry.problem ?? `Disk ${allocatedLabel(entry.audit)}`}{entry.effective.inheritedFrom ? entry.effective.originalStatus === 'on-external' ? ' · parent F: claim' : entry.effective.originalStatus === 'manual-confirmed' ? ' · parent manual claim' : ' · inherited' : ''}{entry.effective.stale ? ' · changed' : ''}</small>{pinned && <small className="review-row-label">Revealed · outside current results</small>}</span>
      {suggestion && <span className="assistance-row-suggestion" data-source={suggestion.source} title={suggestion.reason}>
        {suggestion.source === 'ai' && <Sparkles size={11} aria-hidden="true"/>}
        <span>{suggestionLabel} · {suggestion.label}</span>
      </span>}
      <span className="review-row-status"><StatusBadge effective={entry.effective} compact/>{activityLabel && <small className="review-row-label">{activityLabel}</small>}</span>
    </button>
    {entry.kind === 'folder' && <button className="entry-preview" disabled={Boolean(entry.problem)} title={`Center on ${entry.name}`} aria-label={`Center on folder ${entry.name}`} onClick={onOpen}><Focus size={17}/></button>}
    <button className="entry-more" title={`Mark ${entry.name}`} aria-label={`Mark ${entry.name}: more actions`} aria-haspopup="menu" aria-expanded={menuOpen} aria-controls={menuOpen ? 'row-classification-menu' : undefined} onClick={event => menu(event.currentTarget)}><MoreHorizontal size={17}/></button>
    {activity?.pulse && <i key={activity.pulse.sequence} className="review-activity-pulse" aria-hidden="true"/>}
  </div>;
}

export function RowClassificationMenu({ target, disabledReason, onChoose, onDismiss, onCenterFolder, portalRoot }: {
  target: RowMenuTarget; disabledReason: string | null; onChoose: (status: Status) => void;
  onDismiss: (restoreFocus: boolean) => void; onCenterFolder?: () => void; portalRoot?: HTMLElement;
}) {
  const menuRef = useRef<HTMLDivElement>(null);
  const [position, setPosition] = useState<{ left: number; top: number } | null>(null);
  useLayoutEffect(() => {
    const menu = menuRef.current;
    if (!menu) return;
    const rectangle = menu.getBoundingClientRect();
    setPosition({ left: Math.max(8, Math.min(target.x, window.innerWidth - rectangle.width - 8)), top: Math.max(8, Math.min(target.y, window.innerHeight - rectangle.height - 8)) });
  }, [target]);
  useEffect(() => {
    if (!position) return;
    const menu = menuRef.current;
    const checked = menu?.querySelector<HTMLButtonElement>('button[aria-checked="true"]:not(:disabled)');
    const first = menu?.querySelector<HTMLButtonElement>('button:not(:disabled)');
    (checked ?? first ?? menu)?.focus({ preventScroll: true });
    function pointer(event: PointerEvent): void { if (event.target instanceof Node && !menu?.contains(event.target)) onDismiss(false); }
    function outsideScroll(event: Event): void { if (event.target instanceof Node && !menu?.contains(event.target)) onDismiss(false); }
    function resized(): void { onDismiss(true); }
    window.document.addEventListener('pointerdown', pointer, true);
    window.document.addEventListener('scroll', outsideScroll, true);
    window.addEventListener('resize', resized);
    return () => { window.document.removeEventListener('pointerdown', pointer, true); window.document.removeEventListener('scroll', outsideScroll, true); window.removeEventListener('resize', resized); };
  }, [target.id, position !== null]);
  function key(event: ReactKeyboardEvent<HTMLDivElement>): void {
    if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); onDismiss(true); return; }
    if (event.key === 'Tab') { event.preventDefault(); onDismiss(true); return; }
    const items = Array.from(menuRef.current?.querySelectorAll<HTMLButtonElement>('button:not(:disabled)') ?? []);
    if (!items.length || !['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) return;
    event.preventDefault(); event.stopPropagation();
    const index = items.findIndex(item => item === window.document.activeElement);
    const next = event.key === 'Home' ? 0 : event.key === 'End' ? items.length - 1 : (index + (event.key === 'ArrowDown' ? 1 : -1) + items.length) % items.length;
    items[next]?.focus();
  }
  return createPortal(<div id="row-classification-menu" ref={menuRef} role="menu" tabIndex={-1}
    aria-label={`Actions for ${target.entry.name}`} className="row-classification-menu"
    style={{ position: 'fixed', left: position?.left ?? 0, top: position?.top ?? 0, visibility: position ? 'visible' : 'hidden' }} onKeyDown={key}>
    <div className="row-menu-title"><strong>{target.entry.name}</strong><span title={target.entry.path}>{target.entry.path}</span></div>
    {onCenterFolder && <button role="menuitem" tabIndex={-1} className="row-menu-item" onClick={onCenterFolder}>
      <Focus size={17}/><span>Center on this folder</span>
    </button>}
    {DECISION_CHOICES.map(status => {
      const Icon = ICONS[status];
      const chosen = isDecisionChoiceSelected(target.entry.effective, status);
      return <button key={status} role="menuitemradio" aria-checked={chosen} tabIndex={-1}
        disabled={disabledReason !== null} className={`row-menu-item status-${status}`} title={DESCRIPTIONS[status]}
        onClick={() => { if (chosen) onDismiss(true); else onChoose(status); }}>
        <Icon size={17}/><span>{decisionChoiceLabel(status)}{status === 'migrate-now' ? ' · pending' : ''}</span>
        {chosen && <Check size={15}/>}
      </button>;
    })}
    <p className="row-menu-help">{disabledReason ?? 'Saved mark · copy runs follow the queue settings'}</p>
  </div>, portalRoot ?? window.document.body);
}
