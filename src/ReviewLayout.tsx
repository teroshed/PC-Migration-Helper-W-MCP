import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import type { KeyboardEvent, PointerEvent as ReactPointerEvent, ReactNode } from 'react';
import { ChevronLeft, ChevronRight } from 'lucide-react';
import { restoreReviewScroll } from './ReviewActivity';

export const MIN_DRIVES = 148;
export const MIN_DECISION = 246;
export const MAX_SIDEBAR = 520;

type Edge = (event: PointerEvent, rawSize: number) => boolean;

interface GripProps {
  label: string;
  controls: string;
  min: number;
  max: number;
  direction?: 1 | -1;
  axis?: 'x' | 'y';
  step?: number;
  size: () => number;
  target?: () => HTMLElement | null;
  variable?: string;
  edge?: Edge;
  edgeLabel?: string;
  edgeShortcut?: boolean;
  onAutoFit?: () => void;
  onCommit: (size: number, edge: boolean) => void;
}

interface Drag {
  pointer: number;
  start: number;
  initial: number;
  size: number;
  edge: boolean;
  moved: boolean;
  target: HTMLElement | null;
  previous: string;
  app: HTMLElement | null;
  strip: HTMLElement | null;
  scrollLeft: number;
  listeners: AbortController;
}

export function ResizeGrip({ label, controls, min, max, direction = 1, axis = 'x', step = 16, size, target, variable, edge, edgeLabel, edgeShortcut, onAutoFit, onCommit }: GripProps) {
  const drag = useRef<Drag | null>(null);
  const frame = useRef<number | null>(null);
  const [preview, setPreview] = useState<{ size: number; edge: boolean } | null>(null);
  const [committed, setCommitted] = useState(min);
  const bound = (value: number): number => Math.round(Math.max(min, Math.min(max, value)));
  const coordinate = (event: PointerEvent | ReactPointerEvent<HTMLDivElement>): number => axis === 'x' ? event.clientX : event.clientY;

  function reset(): void {
    if (frame.current !== null) cancelAnimationFrame(frame.current);
    frame.current = null;
    const active = drag.current;
    drag.current = null;
    active?.listeners.abort();
    if (variable && active?.target) {
      if (active.previous) active.target.style.setProperty(variable, active.previous);
      else active.target.style.removeProperty(variable);
    }
    if (active?.strip) restoreReviewScroll(active.strip, 'scrollLeft', active.scrollLeft);
    active?.app?.removeAttribute('data-resize-active');
  }

  useEffect(() => reset, [variable]);
  useLayoutEffect(() => {
    if (!drag.current) setCommitted(bound(size()));
  });

  function start(event: ReactPointerEvent<HTMLDivElement>): void {
    const app = event.currentTarget.closest<HTMLElement>('.review-layout');
    if (event.button !== 0 || drag.current || app?.hasAttribute('data-resize-active')) return;
    event.preventDefault();
    event.currentTarget.focus({ preventScroll: true });
    event.currentTarget.setPointerCapture(event.pointerId);
    const element = target?.() ?? null;
    const initial = size();
    const strip = app?.querySelector<HTMLElement>('.folder-columns-strip') ?? null;
    const listeners = new AbortController();
    app?.setAttribute('data-resize-active', '');
    drag.current = {
      pointer: event.pointerId, start: coordinate(event), initial, size: initial,
      edge: false, moved: false, target: element,
      previous: variable ? element?.style.getPropertyValue(variable) ?? '' : '',
      app, strip, scrollLeft: strip?.scrollLeft ?? 0, listeners,
    };
    const options = { capture: true, signal: listeners.signal };
    window.addEventListener('pointermove', move, options);
    window.addEventListener('pointerup', event => stop(event, true), options);
    window.addEventListener('pointercancel', event => stop(event, false), options);
  }

  function sample(event: PointerEvent): Drag | null {
    const active = drag.current;
    if (!active || active.pointer !== event.pointerId) return null;
    const delta = coordinate(event) - active.start;
    active.moved ||= Math.abs(delta) >= 3;
    const rawSize = active.initial + direction * delta;
    active.size = bound(rawSize);
    active.edge = active.moved && (edge?.(event, rawSize) ?? false);
    return active;
  }

  function move(event: PointerEvent): void {
    const active = sample(event);
    if (!active?.moved || frame.current !== null) return;
    frame.current = requestAnimationFrame(() => {
      frame.current = null;
      if (variable) active.target?.style.setProperty(variable, active.size + 'px');
      setPreview({ size: active.size, edge: active.edge });
    });
  }

  function stop(event: PointerEvent, commit: boolean): void {
    const active = commit ? sample(event) : drag.current;
    if (!active || active.pointer !== event.pointerId) return;
    reset();
    setPreview(null);
    if (commit && active.moved) onCommit(active.size, active.edge);
  }

  function key(event: KeyboardEvent<HTMLDivElement>): void {
    if (event.key === 'Escape' && drag.current) {
      event.preventDefault();
      const pointer = drag.current.pointer;
      reset();
      setPreview(null);
      if (event.currentTarget.hasPointerCapture(pointer)) event.currentTarget.releasePointerCapture(pointer);
      return;
    }
    if (event.key === 'End' && event.ctrlKey && edgeShortcut) {
      event.preventDefault();
      onCommit(size(), true);
      return;
    }
    if (event.key === 'Home' && event.ctrlKey && onAutoFit) {
      event.preventDefault();
      onAutoFit();
      return;
    }
    if (event.ctrlKey || event.altKey || event.metaKey) return;
    const increase = axis === 'x' ? 'ArrowRight' : 'ArrowDown';
    const decrease = axis === 'x' ? 'ArrowLeft' : 'ArrowUp';
    if (![increase, decrease, 'Home', 'End'].includes(event.key)) return;
    event.preventDefault();
    if (event.key === 'Home' || event.key === 'End') {
      onCommit(event.key === 'Home' ? min : max, false);
      return;
    }
    const increment = event.shiftKey ? step * 4 : step;
    onCommit(bound(size() + (event.key === increase ? 1 : -1) * direction * increment), false);
  }

  return <div className={'review-resize-grip resize-' + axis} role="separator" tabIndex={0}
    aria-label={label} aria-controls={controls} aria-orientation={axis === 'x' ? 'vertical' : 'horizontal'}
    aria-valuemin={min} aria-valuemax={max} aria-valuenow={preview?.size ?? committed}
    aria-valuetext={preview?.edge ? edgeLabel : undefined}
    aria-keyshortcuts={onAutoFit ? 'Control+Home Control+End' : edgeShortcut ? 'Control+End' : undefined}
    data-dragging={preview ? 'true' : undefined} data-edge={preview?.edge ? 'true' : undefined}
    data-edge-label={edgeLabel}
    title={label + '. Drag or use the arrow keys.' + (edgeShortcut ? ' Ctrl+End fills the folder pane.' : '') +
      (onAutoFit ? ' Double-click or Ctrl+Home to fit names.' : '')}
    onPointerDown={start} onLostPointerCapture={event => stop(event.nativeEvent, false)}
    onDoubleClick={onAutoFit}
    onKeyDown={key}/>;
}

export function ReviewSidebar({ side, name, collapsed, app, children, onResize, onToggle }: {
  side: 'drives' | 'decision';
  name: string;
  collapsed: boolean;
  app: () => HTMLElement | null;
  children: ReactNode;
  onResize: (width: number, collapse: boolean) => void;
  onToggle: (collapsed: boolean) => void;
}) {
  const slot = useRef<HTMLDivElement>(null);
  const collapse = useRef<HTMLButtonElement>(null);
  const reopen = useRef<HTMLButtonElement>(null);
  const restoreFocus = useRef(false);
  const left = side === 'drives';
  const minimum = left ? MIN_DRIVES : MIN_DECISION;
  const CloseIcon = left ? ChevronLeft : ChevronRight;
  const OpenIcon = left ? ChevronRight : ChevronLeft;

  function toggle(value: boolean): void {
    restoreFocus.current = true;
    onToggle(value);
  }

  useLayoutEffect(() => {
    if (!restoreFocus.current) return;
    restoreFocus.current = false;
    (collapsed ? reopen : collapse).current?.focus({ preventScroll: true });
  }, [collapsed]);

  return <div ref={slot} className={'review-side-slot review-side-' + side}
    data-collapsed={collapsed ? 'true' : undefined}>
    <div id={'review-pane-' + side} className="review-sidebar-content" inert={collapsed}>{children}</div>
    {collapsed ? <button className="review-reopen-sidebar" ref={reopen} data-reopen={side}
      aria-label={'Expand ' + name} title={'Expand ' + name} onClick={() => toggle(false)}>
      <OpenIcon size={20} aria-hidden="true"/>
    </button> : <>
      <button className="review-collapse-sidebar" ref={collapse} aria-label={'Collapse ' + name}
        title={'Collapse ' + name} onClick={() => toggle(true)}>
        <CloseIcon size={15} aria-hidden="true"/>
      </button>
      <ResizeGrip label={'Resize ' + name} controls={'review-pane-' + side} min={minimum} max={MAX_SIDEBAR}
        direction={left ? 1 : -1} size={() => slot.current?.getBoundingClientRect().width ?? minimum}
        target={app} variable={'--drag-' + side + '-width'} edgeLabel={'Release to collapse ' + name}
        edge={(_, raw) => raw < 96}
        onCommit={(width, edge) => { if (edge) restoreFocus.current = true; onResize(width, edge); }}/>
    </>}
  </div>;
}
