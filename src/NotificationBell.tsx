import { useEffect, useRef, useState } from 'react';
import type { FocusEvent as ReactFocusEvent } from 'react';
import { createPortal } from 'react-dom';
import { invoke } from '@tauri-apps/api/core';
import { Bell, CheckCheck, ChevronLeft, ChevronRight, ExternalLink, RefreshCw, TriangleAlert, X } from 'lucide-react';
import { parseNotificationFeed } from './audit-model';
import type { AuditNotification, NotificationFeed } from './audit-model';
import { notificationLocation } from './model';
import type { Volume } from './model';
import { OverflowIndicators } from './OverflowIndicators';

export function NotificationBell({ enabled, unread: nativeUnread, volumes, onNavigate, onReadStateChanged, onError }: { enabled: boolean; unread: number; volumes: Volume[]; onNavigate: (path: string, selection?: string | null) => void; onReadStateChanged: () => Promise<void>; onError: (cause: unknown) => void }) {
  const [unread, setUnread] = useState(nativeUnread);
  const [preview, setPreview] = useState(false);
  const [full, setFull] = useState(false);
  const [previewFeed, setPreviewFeed] = useState<NotificationFeed | null>(null);
  const [fullFeed, setFullFeed] = useState<NotificationFeed | null>(null);
  const [offset, setOffset] = useState(0);
  const [loading, setLoading] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const [marking, setMarking] = useState(false);
  const [readReceipts, setReadReceipts] = useState(new Set<string>());
  const [newAvailable, setNewAvailable] = useState(false);
  const [position, setPosition] = useState({ top: 60, right: 16 });
  const triggerRef = useRef<HTMLButtonElement>(null);
  const previewRef = useRef<HTMLDivElement>(null);
  const previewListRef = useRef<HTMLDivElement>(null);
  const dialogRef = useRef<HTMLDialogElement>(null);
  const fullListRef = useRef<HTMLDivElement>(null);
  const previewOpen = useRef(false);
  const fullOpen = useRef(false);
  const latest = useRef<NotificationFeed | null>(null);
  const snapshot = useRef<NotificationFeed | null>(null);
  const leaveTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const mounted = useRef(true);
  const pageRequest = useRef(0);
  const focusPreview = useRef(false);
  const suppressFocusPreview = useRef(false);
  const previewInFlight = useRef(false);
  useEffect(() => { setUnread(nativeUnread); }, [nativeUnread]);
  function clearLeave(): void { if (leaveTimer.current) clearTimeout(leaveTimer.current); leaveTimer.current = null; }
  function hidePreview(restoreFocus = false): void {
    clearLeave(); previewOpen.current = false; setPreview(false); focusPreview.current = false;
    if (restoreFocus) { suppressFocusPreview.current = true; triggerRef.current?.focus(); }
  }
  function leave(): void {
    clearLeave();
    leaveTimer.current = setTimeout(() => {
      const focus = window.document.activeElement;
      if (focus instanceof Node && (previewRef.current?.contains(focus) || triggerRef.current?.contains(focus))) return;
      hidePreview();
    }, 180);
  }
  async function refreshLatest(): Promise<void> {
    if (!enabled || previewInFlight.current) return;
    previewInFlight.current = true;
    try {
      const feed = parseNotificationFeed(await invoke<unknown>('audit_notifications', { limit: 5, offset: 0 }));
      if (!mounted.current) return;
      latest.current = feed; setUnread(feed.unread); setProblem(null);
      if (!previewOpen.current && !fullOpen.current) { snapshot.current = feed; setPreviewFeed(feed); }
      else if (previewOpen.current && !snapshot.current) { snapshot.current = feed; setPreviewFeed(feed); }
      else if (JSON.stringify(snapshot.current?.items) !== JSON.stringify(feed.items)) setNewAvailable(true);
    } catch (cause) { if (mounted.current) setProblem(`Notifications are unavailable: ${String(cause)}`); }
    finally { previewInFlight.current = false; }
  }
  function showPreview(keyboard = false): void {
    if (fullOpen.current) return;
    clearLeave();
    const rectangle = triggerRef.current?.getBoundingClientRect();
    if (rectangle) setPosition({ top: rectangle.bottom + 7, right: Math.max(8, window.innerWidth - rectangle.right) });
    snapshot.current = latest.current;
    setPreviewFeed(latest.current);
    setNewAvailable(false);
    previewOpen.current = true; setPreview(true); focusPreview.current = keyboard;
    void refreshLatest();
  }
  function openFull(): void {
    hidePreview(); fullOpen.current = true; setOffset(0); setNewAvailable(false); setFull(true);
  }
  function closeFull(): void { fullOpen.current = false; suppressFocusPreview.current = true; setFull(false); requestAnimationFrame(() => triggerRef.current?.focus()); }
  async function loadPage(pageOffset: number): Promise<void> {
    const request = ++pageRequest.current;
    setLoading(true);
    try {
      const feed = parseNotificationFeed(await invoke<unknown>('audit_notifications', { limit: 50, offset: pageOffset }));
      if (!mounted.current || request !== pageRequest.current || !fullOpen.current) return;
      setFullFeed(feed); setUnread(feed.unread); setProblem(null); setNewAvailable(false);
      requestAnimationFrame(() => { if (fullListRef.current) fullListRef.current.scrollTop = 0; });
    } catch (cause) { if (mounted.current && request === pageRequest.current) setProblem(`Notifications are unavailable: ${String(cause)}`); }
    finally { if (mounted.current && request === pageRequest.current) setLoading(false); }
  }
  async function refreshPreview(): Promise<void> {
    try {
      const feed = parseNotificationFeed(await invoke<unknown>('audit_notifications', { limit: 5, offset: 0 }));
      if (!mounted.current) return;
      latest.current = feed; snapshot.current = feed; setPreviewFeed(feed); setUnread(feed.unread); setNewAvailable(false); setProblem(null);
      if (previewListRef.current) previewListRef.current.scrollTop = 0;
    } catch (cause) { setProblem(String(cause)); }
  }
  async function markRead(ids?: string[], all = false): Promise<void> {
    if (marking) return;
    setMarking(true);
    try {
      await invoke('audit_read_notifications', all ? { all: true } : { ids });
      if (!mounted.current) return;
      const acknowledged = all ? [...(fullFeed?.items ?? []), ...(previewFeed?.items ?? [])].map(item => item.id) : ids ?? [];
      setReadReceipts(previous => new Set([...previous, ...acknowledged]));
      await refreshLatest();
      await onReadStateChanged();
    } catch (cause) { if (mounted.current) { setProblem(`Read state was not saved: ${String(cause)}`); onError(cause); } }
    finally { if (mounted.current) setMarking(false); }
  }
  function activate(item: AuditNotification): void {
    const location = notificationLocation(item, volumes);
    if (location) { hidePreview(); if (fullOpen.current) closeFull(); onNavigate(location.path, location.selection); }
    else if (!fullOpen.current) openFull();
    if (!item.readAt && !readReceipts.has(item.id)) void markRead([item.id]);
  }
  useEffect(() => {
    mounted.current = true;
    if (!enabled) return;
    void refreshLatest();
    const timer = setInterval(() => { void refreshLatest(); }, 2000);
    return () => { mounted.current = false; clearInterval(timer); clearLeave(); };
  }, [enabled]);
  useEffect(() => {
    if (!full) return;
    dialogRef.current?.showModal();
    return () => { pageRequest.current += 1; dialogRef.current?.close(); };
  }, [full]);
  useEffect(() => { if (full) void loadPage(offset); }, [full, offset]);
  useEffect(() => {
    if (preview && focusPreview.current && previewFeed) {
      previewRef.current?.querySelector<HTMLButtonElement>('.notification-item')?.focus(); focusPreview.current = false;
    }
  }, [preview, previewFeed]);
  useEffect(() => {
    if (!preview) return;
    function outside(event: PointerEvent): void { if (event.target instanceof Node && !previewRef.current?.contains(event.target) && !triggerRef.current?.contains(event.target)) hidePreview(); }
    function key(event: KeyboardEvent): void { if (event.key === 'Escape') { event.preventDefault(); hidePreview(true); } }
    function resized(): void { hidePreview(); }
    window.document.addEventListener('pointerdown', outside, true); window.document.addEventListener('keydown', key); window.addEventListener('resize', resized);
    return () => { window.document.removeEventListener('pointerdown', outside, true); window.document.removeEventListener('keydown', key); window.removeEventListener('resize', resized); };
  }, [preview]);
  function blur(event: ReactFocusEvent<HTMLElement>): void {
    const next = event.relatedTarget;
    if (next instanceof Node && (previewRef.current?.contains(next) || triggerRef.current?.contains(next))) return;
    requestAnimationFrame(() => { if (!mounted.current) return; const focus = window.document.activeElement; if (focus instanceof Node && !previewRef.current?.contains(focus) && !triggerRef.current?.contains(focus)) hidePreview(); });
  }
  const notificationItems = (items: AuditNotification[], compact = false) => items.map(item => <NotificationItem key={item.id} item={item} compact={compact} read={Boolean(item.readAt || readReceipts.has(item.id))} navigable={Boolean(notificationLocation(item, volumes))} onActivate={() => activate(item)}/>);
  return <>
    <button ref={triggerRef} className="notification-bell" aria-label={`Notifications, ${unread.toLocaleString()} unread`} aria-haspopup="dialog" aria-expanded={preview || full} aria-controls={full ? 'notifications-dialog' : preview ? 'notifications-preview' : undefined} onPointerEnter={event => { if (event.pointerType !== 'touch') showPreview(); }} onPointerLeave={leave} onFocus={() => { if (suppressFocusPreview.current) { suppressFocusPreview.current = false; return; } showPreview(); }} onBlur={event => { suppressFocusPreview.current = false; blur(event); }} onClick={openFull} onKeyDown={event => { if (event.key === 'ArrowDown') { event.preventDefault(); showPreview(true); } }}><Bell size={18}/>{unread > 0 && <span className="notification-count" aria-hidden="true">{unread > 99 ? '99+' : unread}</span>}</button>
    {preview && !full && createPortal(<div id="notifications-preview" ref={previewRef} role="dialog" aria-label="Recent notifications preview" data-notifications-preview="true" className="notifications-preview" style={position} onPointerEnter={clearLeave} onPointerLeave={leave} onFocus={clearLeave} onBlur={blur}><div className="notifications-preview-heading"><strong>Recent notifications</strong><span>{unread.toLocaleString()} unread</span></div><div className="notifications-list-frame"><div ref={previewListRef} className="notifications-preview-list">{previewFeed?.items.length ? notificationItems(previewFeed.items, true) : <p className="notifications-empty">{problem ?? (previewFeed ? 'No notifications recorded yet.' : 'Reading notifications…')}</p>}</div><OverflowIndicators targetRef={previewListRef} axis="vertical" refreshKey={previewFeed}/></div><div className="notifications-preview-footer"><button onClick={() => void refreshPreview()} title="Refresh the frozen preview list"><RefreshCw size={13}/>{newAvailable ? 'New updates' : 'Refresh'}</button><button onClick={openFull}>View all<ChevronRight size={14}/></button></div></div>, window.document.body)}
    {full && createPortal(<dialog id="notifications-dialog" ref={dialogRef} className="notifications-dialog" aria-label="Notifications" onCancel={event => { event.preventDefault(); closeFull(); }}><div className="notifications-dialog-heading"><div><Bell size={20}/><h2>Notifications</h2><span>{unread.toLocaleString()} unread</span></div><button className="icon-button" aria-label="Close notifications" onClick={closeFull}><X size={19}/></button></div><div className="notifications-dialog-tools"><button disabled={marking || !unread} onClick={() => void markRead(undefined, true)}><CheckCheck size={14}/>{marking ? 'Saving read state…' : 'Mark all read'}</button><button disabled={loading} onClick={() => void loadPage(offset)}><RefreshCw size={14}/>{newAvailable ? 'Refresh · new updates' : 'Refresh'}</button><span>{fullFeed ? `${fullFeed.total.toLocaleString()} recorded` : 'Reading…'}</span></div>{problem && <p className="notifications-problem" role="alert"><TriangleAlert size={15}/>{problem}</p>}<div className="notifications-list-frame notifications-full-frame"><div ref={fullListRef} className="notifications-full-list" aria-busy={loading}>{fullFeed?.items.length ? notificationItems(fullFeed.items) : <p className="notifications-empty">{loading ? 'Reading notifications…' : problem ?? 'No notifications recorded yet. Source audit changes will appear here.'}</p>}</div><OverflowIndicators targetRef={fullListRef} axis="vertical" refreshKey={fullFeed}/></div><div className="notifications-dialog-footer"><span>Read state is saved in the native app.</span><div><button disabled={offset === 0 || loading} onClick={() => setOffset(Math.max(0, offset - 50))}><ChevronLeft size={14}/>Newer</button><span>{Math.floor(offset / 50) + 1} / {Math.max(1, Math.ceil((fullFeed?.total ?? 0) / 50))}</span><button disabled={offset + 50 >= (fullFeed?.total ?? 0) || loading} onClick={() => setOffset(offset + 50)}>Older<ChevronRight size={14}/></button></div></div></dialog>, window.document.body)}
  </>;
}

function NotificationItem({ item, compact, read, navigable, onActivate }: { item: AuditNotification; compact: boolean; read: boolean; navigable: boolean; onActivate: () => void }) {
  return <button className={`notification-item ${compact ? 'notification-item-compact' : ''} ${read ? 'is-read' : 'is-unread'}`} onClick={onActivate} title={item.originalPath ?? item.path ?? item.message}><span className="notification-read-dot" aria-label={read ? 'Read' : 'Unread'}/><span className="notification-copy"><strong>{item.title}</strong><span className="notification-message">{item.message}</span>{!compact && (item.originalPath || item.path) && <span className="notification-path">{item.originalPath ?? item.path}</span>}<small>{new Date(item.createdAt).toLocaleString()}{item.changeCount > 1 ? ` · ${item.changeCount.toLocaleString()} changes` : ''}{!compact && !navigable ? ' · details only' : ''}</small></span>{navigable && <ExternalLink size={13} aria-label="Review source location"/>}</button>;
}
