import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import type { RefObject } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { baseName, record } from './model';
import type { ViewState } from './model';

type ActivityKind = 'read' | 'mark' | 'reveal';
export interface ReviewActivity {
  sequence: number;
  kind: ActivityKind;
  path: string;
  atMs: number;
  revision?: number;
}
export interface ActivityRow { pulse?: ReviewActivity; pulseUntilMs?: number; readAtMs?: number }
export interface ReviewActivitySnapshot { paths: ReadonlyMap<string, ActivityRow>; nowMs: number }
interface SurfaceSnapshot { view: ViewState; appliedRevision: number; followAi: boolean; following: boolean; knownSearch: boolean; loading: boolean }
interface Bounds { left: number; top: number; right: number; bottom: number }
const POLL_MS = 250;
const READ_AGE_MS = 60_000;
export const PULSE_MS = 1000;
const REPORT_MS = 250;
const REPORT_HEARTBEAT_MS = 2000;
const MAX_COLUMNS = 48;
const MAX_ROWS = 512;

export function reviewPathKey(path: string): string {
  return path.replace(/\\$/, '').toLowerCase();
}

export function reviewActivityLabel(row: ActivityRow | undefined, nowMs: number): string | null {
  if (row?.pulse?.kind === 'mark') return 'AI marked';
  if (row?.pulse?.kind === 'reveal') return 'AI revealed';
  if (row?.readAtMs === undefined) return null;
  const seconds = Math.max(0, Math.floor((nowMs - row.readAtMs) / 1000));
  return `Viewed by MCP ${seconds} ${seconds === 1 ? 'second' : 'seconds'} ago`;
}

function activityItem(value: unknown): ReviewActivity | null {
  const item = record(value);
  if (item.kind !== 'read' && item.kind !== 'mark' && item.kind !== 'reveal') return null;
  if (typeof item.path !== 'string' || typeof item.atMs !== 'number' || !Number.isFinite(item.atMs) ||
    typeof item.sequence !== 'number' || !Number.isSafeInteger(item.sequence)) return null;
  const revision = typeof item.revision === 'number' && Number.isSafeInteger(item.revision) ? item.revision : undefined;
  return { kind: item.kind, path: item.path, atMs: item.atMs, sequence: item.sequence, revision };
}

export function useReviewActivity(
  enabled: boolean,
  onPoll: (items: ReviewActivity[]) => void,
): ReviewActivitySnapshot {
  const callback = useRef(onPoll);
  const [snapshot, setSnapshot] = useState<ReviewActivitySnapshot>(() => ({ paths: new Map(), nowMs: Date.now() }));
  useLayoutEffect(() => { callback.current = onPoll; }, [onPoll]);
  useEffect(() => {
    if (!enabled) return;
    let active = true;
    let inFlight = false;
    let after = 0;
    let initial = true;
    const openedAt = Date.now();
    const paths = new Map<string, ActivityRow>();
    let ageSecond = 0;

    function publish(changed: boolean, nowMs: number): void {
      const nextAgeSecond = Math.floor(nowMs / 1000);
      for (const [key, row] of paths) {
        const pulse = row.pulseUntilMs !== undefined && nowMs < row.pulseUntilMs ? row.pulse : undefined;
        const readAtMs = row.readAtMs !== undefined && nowMs - row.readAtMs < READ_AGE_MS ? row.readAtMs : undefined;
        if (pulse === row.pulse && readAtMs === row.readAtMs) continue;
        if (!pulse && readAtMs === undefined) paths.delete(key);
        else paths.set(key, { pulse, pulseUntilMs: pulse ? row.pulseUntilMs : undefined, readAtMs });
        changed = true;
      }
      if (changed || (paths.size > 0 && nextAgeSecond !== ageSecond)) {
        setSnapshot({ paths: new Map(paths), nowMs });
        ageSecond = nextAgeSecond;
      }
    }

    async function poll(): Promise<void> {
      publish(false, Date.now());
      if (inFlight) return;
      inFlight = true;
      try {
        const result = record(await invoke<unknown>('poll_review_activity', { after }));
        if (!active || typeof result.sequence !== 'number' || !Number.isSafeInteger(result.sequence) ||
          !Array.isArray(result.items)) return;
        if (result.sequence < after) {
          paths.clear();
          after = 0;
          initial = true;
          publish(true, Date.now());
          return;
        }
        const nowMs = Date.now();
        const items: ReviewActivity[] = [];
        for (const value of result.items) {
          const item = activityItem(value);
          if (!item || item.sequence <= after) continue;
          if (nowMs - item.atMs >= READ_AGE_MS) continue;
          const key = reviewPathKey(item.path);
          const previous = paths.get(key);
          const freshPulse = nowMs - item.atMs < PULSE_MS;
          paths.delete(key);
          paths.set(key, {
            pulse: freshPulse ? item : previous?.pulse,
            pulseUntilMs: freshPulse ? nowMs + PULSE_MS : previous?.pulseUntilMs,
            readAtMs: item.kind === 'read' ? item.atMs : previous?.readAtMs,
          });
          if (!initial || item.atMs >= openedAt) items.push(item);
        }
        for (const key of paths.keys()) {
          if (paths.size <= MAX_ROWS) break;
          paths.delete(key);
        }
        const changed = result.sequence > after;
        after = result.sequence;
        initial = false;
        publish(changed, nowMs);
        callback.current(items);
      } catch {
        // Transient activity delivery cannot invalidate the saved journal or the folder controls.
      } finally { inFlight = false; }
    }
    const interval = setInterval(() => { void poll(); }, POLL_MS);
    void poll();
    return () => { active = false; clearInterval(interval); };
  }, [enabled]);
  return snapshot;
}

function clientBounds(element: HTMLElement): Bounds {
  const bounds = element.getBoundingClientRect();
  const left = bounds.left + element.clientLeft;
  const top = bounds.top + element.clientTop;
  return { left, top, right: left + element.clientWidth, bottom: top + element.clientHeight };
}

function intersection(a: Bounds, b: Bounds): Bounds {
  return {
    left: Math.max(a.left, b.left), top: Math.max(a.top, b.top),
    right: Math.min(a.right, b.right), bottom: Math.min(a.bottom, b.bottom),
  };
}

function hasArea(bounds: Bounds): boolean {
  return bounds.right > bounds.left && bounds.bottom > bounds.top;
}

function visibleBounds(strip: HTMLElement): Bounds {
  return intersection(clientBounds(strip), { left: 0, top: 0, right: window.innerWidth, bottom: window.innerHeight });
}

export function isReviewRowVisible(strip: HTMLElement | null, path: string): boolean {
  if (!strip) return false;
  const key = reviewPathKey(path);
  const clip = visibleBounds(strip);
  for (const row of strip.querySelectorAll<HTMLElement>('[data-entry-kind]')) {
    if (!row.dataset.entryPath || reviewPathKey(row.dataset.entryPath) !== key) continue;
    const list = row.closest<HTMLElement>('.column-entry-list');
    if (list && hasArea(intersection(intersection(clientBounds(list), clip), row.getBoundingClientRect()))) return true;
  }
  return false;
}

function collectSurface(strip: HTMLElement, snapshot: SurfaceSnapshot) {
  const document = window.document;
  const dialogOpen = Boolean(document.querySelector('dialog[open]'));
  const activeElement = document.activeElement;
  const focusRow = activeElement?.closest<HTMLElement>('[data-entry-kind], [data-column-path]');
  const focusedPath = activeElement && strip.contains(activeElement)
    ? focusRow?.dataset.entryPath ?? focusRow?.dataset.columnPath ?? null
    : null;
  const clip = visibleBounds(strip);
  let remainingRows = MAX_ROWS;
  let loading = snapshot.loading;
  const columns = [];
  for (const column of strip.querySelectorAll<HTMLElement>('[data-column-path]')) {
    if (columns.length >= MAX_COLUMNS) break;
    const columnBounds = column.getBoundingClientRect();
    if (columnBounds.left >= clip.right) break;
    if (!hasArea(intersection(clip, columnBounds))) continue;
    const list = column.querySelector<HTMLElement>('.column-entry-list');
    const path = column.dataset.columnPath;
    if (!list || !path) continue;
    const listClip = intersection(clip, clientBounds(list));
    const pending = list.getAttribute('aria-busy') === 'true';
    loading ||= pending;
    const visibleRows = [];
    if (!dialogOpen && !pending && remainingRows && hasArea(listClip)) {
      for (const row of list.querySelectorAll<HTMLElement>('[data-entry-kind]')) {
        if (!remainingRows || visibleRows.length >= 101) break;
        const rowBounds = row.getBoundingClientRect();
        if (rowBounds.top >= listClip.bottom) break;
        if (!row.dataset.entryPath || !hasArea(intersection(listClip, rowBounds))) continue;
        visibleRows.push({
          path: row.dataset.entryPath,
          name: row.dataset.entryName ?? baseName(row.dataset.entryPath),
          kind: row.dataset.entryKind ?? 'unavailable',
          status: row.dataset.entryStatus ?? 'unknown',
          ...(row.dataset.reviewPinned === 'true' ? { pinned: true } : {}),
        });
        remainingRows -= 1;
      }
    }
    columns.push({
      path, depth: Number(column.dataset.columnDepth), page: Number(column.dataset.columnPage),
      scrollTop: list.scrollTop, total: Number(column.dataset.columnTotal), visibleRows,
    });
  }
  const root = strip.querySelector<HTMLElement>('.root-column .column-entry-list');
  const view = snapshot.view;
  const visibleColumnScroll = new Map(columns.map(column => [column.depth, column.scrollTop]));
  const previewColumns = view.previewColumns.slice(0, MAX_COLUMNS).map((column, index) => ({
    ...column, scrollTop: visibleColumnScroll.get(index + 1) ?? column.scrollTop,
  }));
  return {
    path: view.path, selectedPath: view.selectedPath ?? view.path, focusedPath,
    search: view.search, filter: view.filter, page: view.page, scrollTop: root?.scrollTop ?? 0,
    previewScrollLeft: strip.scrollLeft, previewColumns,
    previewColumnsTruncated: view.previewColumns.length > MAX_COLUMNS,
    columns, followAi: snapshot.followAi, following: snapshot.following, revealPath: view.revealPath, appliedRevision: snapshot.appliedRevision,
    loading, searchMode: snapshot.knownSearch ? 'known-folders' : 'folder',
    windowFocused: document.hasFocus(), visibilityState: document.visibilityState, dialogOpen,
  };
}

export function useReviewSurface(stripRef: RefObject<HTMLDivElement | null>, snapshot: SurfaceSnapshot | null): void {
  const latest = useRef(snapshot);
  const requestReport = useRef<() => void>(() => {});
  const enabled = snapshot !== null;
  useLayoutEffect(() => {
    latest.current = snapshot;
    requestReport.current();
  }, [snapshot]);
  useEffect(() => {
    const strip = stripRef.current;
    if (!strip || !enabled) return;
    const document = window.document;
    let inFlight = false;
    let queued = false;
    let active = true;
    let pending: ReturnType<typeof setTimeout> | null = null;
    let lastSentAt = 0;
    let previous = '';
    function schedule(): void {
      if (!active || pending !== null) return;
      if (inFlight) { queued = true; return; }
      pending = setTimeout(() => { pending = null; void report(); }, REPORT_MS);
    }
    async function report(): Promise<void> {
      if (!latest.current || !strip || !active) return;
      const surface = collectSurface(strip, latest.current);
      let key = JSON.stringify(surface);
      const encoder = new TextEncoder();
      if (encoder.encode(key).byteLength > 360 * 1024) {
        // Extremely deep, long Windows paths can exceed the native snapshot byte budget.
        for (const column of surface.columns) column.visibleRows = column.visibleRows.slice(0, 4);
        key = JSON.stringify(surface);
        if (encoder.encode(key).byteLength > 360 * 1024) return;
      }
      if (key === previous && Date.now() - lastSentAt < REPORT_HEARTBEAT_MS) return;
      inFlight = true;
      try {
        await invoke('report_review_surface', { surface });
        previous = key;
        lastSentAt = Date.now();
      } catch {
        // The gateway may be unavailable while the desktop view remains usable.
      } finally {
        inFlight = false;
        if (queued) { queued = false; schedule(); }
      }
    }
    requestReport.current = schedule;
    const observer = new MutationObserver(schedule);
    observer.observe(strip, { subtree: true, childList: true, attributes: true, attributeFilter: ['aria-busy', 'data-column-total', 'data-entry-status'] });
    const resize = new ResizeObserver(schedule);
    resize.observe(strip);
    strip.addEventListener('scroll', schedule, true);
    document.addEventListener('focusin', schedule);
    document.addEventListener('focusout', schedule);
    document.addEventListener('visibilitychange', schedule);
    window.addEventListener('focus', schedule);
    window.addEventListener('blur', schedule);
    const heartbeat = setInterval(schedule, REPORT_HEARTBEAT_MS);
    schedule();
    return () => {
      active = false;
      requestReport.current = () => {};
      if (pending !== null) clearTimeout(pending);
      clearInterval(heartbeat);
      observer.disconnect();
      resize.disconnect();
      strip.removeEventListener('scroll', schedule, true);
      document.removeEventListener('focusin', schedule);
      document.removeEventListener('focusout', schedule);
      document.removeEventListener('visibilitychange', schedule);
      window.removeEventListener('focus', schedule);
      window.removeEventListener('blur', schedule);
    };
  }, [enabled, stripRef]);
}

export function restoreReviewScroll(element: HTMLElement, axis: 'scrollTop' | 'scrollLeft', position: number): void {
  if (Math.abs(element[axis] - position) < 1) return;
  element[axis] = position;
  element.dataset[axis] = String(element[axis]);
}

export function isRestoredScroll(element: HTMLElement, axis: 'scrollTop' | 'scrollLeft'): boolean {
  const restored = element.dataset[axis];
  delete element.dataset[axis];
  return restored !== undefined && Math.abs(Number(restored) - element[axis]) < 1;
}

export function showReviewRow(list: HTMLElement, row: HTMLElement): void {
  const strip = list.closest<HTMLElement>('.folder-columns-strip');
  const column = list.closest<HTMLElement>('.folder-column');
  if (strip && column) {
    const stripBounds = clientBounds(strip);
    const columnBounds = column.getBoundingClientRect();
    const rightGap = columnBounds.right - stripBounds.right;
    const leftGap = columnBounds.left - stripBounds.left;
    if (leftGap < 0 || rightGap > 0) {
      const shift = columnBounds.right - columnBounds.left > strip.clientWidth ? leftGap : leftGap < 0 ? leftGap : rightGap;
      restoreReviewScroll(strip, 'scrollLeft', strip.scrollLeft + shift);
    }
  }
  if (!list.contains(row)) return;
  const listBounds = clientBounds(list);
  const rowBounds = row.getBoundingClientRect();
  if (rowBounds.top < listBounds.top) restoreReviewScroll(list, 'scrollTop', list.scrollTop + rowBounds.top - listBounds.top);
  else if (rowBounds.bottom > listBounds.bottom) restoreReviewScroll(list, 'scrollTop', list.scrollTop + rowBounds.bottom - listBounds.bottom);
}
