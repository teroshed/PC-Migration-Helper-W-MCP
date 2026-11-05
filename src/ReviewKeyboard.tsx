import { useCallback, useEffect, useId, useImperativeHandle, useLayoutEffect, useRef, useState } from 'react';
import type { KeyboardEvent, Ref, RefObject } from 'react';
import { Check, ChevronDown, ListFilter } from 'lucide-react';
import { LABELS } from './classifications';
import { STATUSES } from './model';
import { reviewPathKey } from './ReviewActivity';
import './review-keyboard.css';

interface FocusRequest {
  depth: number;
  folder: string;
  target: 'heading' | 'first' | 'row';
  path?: string;
}

function focusElement(element: HTMLElement): void {
  element.focus({ preventScroll: true });
  element.scrollIntoView({ block: 'nearest', inline: 'nearest' });
}

export function useReviewKeyboardFocus(strip: RefObject<HTMLDivElement | null>) {
  const inputVersion = useRef(0);
  const pending = useRef<(() => void) | null>(null);

  useEffect(() => {
    function humanInput(): void {
      inputVersion.current += 1;
      pending.current?.();
    }
    window.document.addEventListener('pointerdown', humanInput, true);
    window.document.addEventListener('keydown', humanInput, true);
    return () => {
      window.document.removeEventListener('pointerdown', humanInput, true);
      window.document.removeEventListener('keydown', humanInput, true);
      pending.current?.();
    };
  }, []);

  const stillHere = useCallback((): (() => boolean) => {
    const version = inputVersion.current;
    const anchor = window.document.activeElement;
    return () => version === inputVersion.current && window.document.activeElement === anchor;
  }, []);

  const requestFocus = useCallback((request: FocusRequest): void => {
    pending.current?.();
    const anchor = window.document.activeElement;
    const region = strip.current;
    if (!region) return;
    let done = false;
    let timer = 0;
    let frame = 0;
    function cancel(): void {
      if (done) return;
      done = true;
      observer.disconnect();
      cancelAnimationFrame(frame);
      window.clearTimeout(timer);
      window.document.removeEventListener('focusin', onFocus);
      if (pending.current === cancel) pending.current = null;
    }
    function onFocus(event: FocusEvent): void {
      if (event.target !== anchor && event.target !== window.document.body) cancel();
    }
    function findTarget(fallback: boolean): void {
      if (done) return;
      const active = window.document.activeElement;
      if (active !== anchor && active !== window.document.body) {
        cancel();
        return;
      }
      const column = Array.from(region!.querySelectorAll<HTMLElement>('[data-column-depth]')).find(element =>
        element.dataset.columnDepth === String(request.depth) &&
        reviewPathKey(element.dataset.columnPath ?? '') === reviewPathKey(request.folder));
      if (!column) return;
      const heading = column.querySelector<HTMLButtonElement>('.column-review');
      const list = column.querySelector<HTMLElement>('.column-entry-list');
      let target: HTMLElement | null | undefined;
      if (request.target === 'heading') {
        target = heading;
      } else if (list?.getAttribute('aria-busy') !== 'true') {
        const rows = Array.from(list?.querySelectorAll<HTMLButtonElement>('.entry-select, .known-result') ?? []);
        target = request.target === 'first'
          ? rows[0] ?? heading
          : rows.find(row => reviewPathKey(row.dataset.entryPath ?? '') === reviewPathKey(request.path ?? ''));
      }
      target ??= fallback ? heading : null;
      if (!target) return;
      cancel();
      focusElement(target);
    }
    const observer = new MutationObserver(() => findTarget(false));
    observer.observe(region, { subtree: true, childList: true, attributes: true, attributeFilter: ['aria-busy', 'data-column-path', 'data-column-page'] });
    window.document.addEventListener('focusin', onFocus);
    frame = requestAnimationFrame(() => findTarget(false));
    timer = window.setTimeout(() => {
      findTarget(true);
      cancel();
    }, 8000);
    pending.current = cancel;
  }, [strip]);

  return { requestFocus, stillHere };
}

export function focusAdjacentRow(list: HTMLElement | null, current: HTMLElement, delta: number): HTMLButtonElement | null {
  const rows = Array.from(list?.querySelectorAll<HTMLButtonElement>('.entry-select, .known-result') ?? []);
  const index = rows.indexOf(current as HTMLButtonElement);
  if (index < 0) return null;
  const next = rows[index + delta];
  if (!next) return null;
  focusElement(next);
  return next;
}

export function isCaretBoundary(input: HTMLInputElement, direction: 'left' | 'right'): boolean {
  const { selectionStart, selectionEnd, value } = input;
  if (selectionStart === null || selectionStart !== selectionEnd) return false;
  return selectionStart === (direction === 'left' ? 0 : value.length);
}

const FILTER_OPTIONS = [
  { value: 'all', label: 'All decisions' },
  ...STATUSES.map(status => ({ value: status, label: LABELS[status] })),
  { value: 'mixed', label: 'Mixed / exceptions' },
];

export interface ReviewFilterHandle {
  openFirst: () => void;
}

interface ReviewFilterProps {
  ref?: Ref<ReviewFilterHandle>;
  value: string;
  onChange: (value: string) => void;
  onMoveLeft: () => void;
  onEscape: () => void;
}

export function ReviewFilter({ ref, value, onChange, onMoveLeft, onEscape }: ReviewFilterProps) {
  const id = useId();
  const [open, setOpen] = useState(false);
  const container = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const options = useRef<HTMLDivElement>(null);
  const focusIndex = useRef(0);
  const restoringFocus = useRef(false);
  const current = FILTER_OPTIONS.findIndex(option => option.value === value);
  const label = FILTER_OPTIONS[current]?.label ?? 'All decisions';

  function focusOption(index: number): void {
    const elements = options.current?.querySelectorAll<HTMLButtonElement>('[role="option"]');
    const option = elements?.[index];
    option?.focus({ preventScroll: true });
    option?.scrollIntoView({ block: 'nearest' });
  }
  function start(index: number): void {
    focusIndex.current = index;
    if (open) focusOption(index);
    else setOpen(true);
  }
  useImperativeHandle(ref, () => ({ openFirst: () => start(0) }), [open]);
  useLayoutEffect(() => {
    if (open) focusOption(focusIndex.current);
  }, [open]);
  useEffect(() => {
    if (!open) return;
    function outside(event: PointerEvent): void {
      if (event.target instanceof Node && !container.current?.contains(event.target)) setOpen(false);
    }
    window.document.addEventListener('pointerdown', outside, true);
    return () => window.document.removeEventListener('pointerdown', outside, true);
  }, [open]);

  function choose(next: string): void {
    onChange(next);
    setOpen(false);
    restoringFocus.current = true;
    trigger.current?.focus();
    restoringFocus.current = false;
  }
  function key(event: KeyboardEvent<HTMLDivElement>): void {
    if (event.nativeEvent.isComposing || event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
    const { key } = event;
    if (key === 'Escape' || key === 'ArrowLeft') {
      event.preventDefault();
      event.stopPropagation();
      setOpen(false);
      if (key === 'Escape') onEscape();
      else onMoveLeft();
      return;
    }
    if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(key)) return;
    event.preventDefault();
    event.stopPropagation();
    if (!open) {
      start(0);
      return;
    }
    const items = Array.from(options.current?.querySelectorAll<HTMLButtonElement>('[role="option"]') ?? []);
    const index = items.findIndex(item => item === window.document.activeElement);
    const next = key === 'Home' ? 0 : key === 'End' ? items.length - 1
      : Math.max(0, Math.min(items.length - 1, index + (key === 'ArrowDown' ? 1 : -1)));
    focusOption(next);
  }

  return <div className="review-filter" ref={container} onKeyDown={key} onBlur={event => {
    if (!(event.relatedTarget instanceof Node) || !event.currentTarget.contains(event.relatedTarget)) setOpen(false);
  }}>
    <button type="button" ref={trigger} className="review-filter-trigger" role="combobox"
      aria-label="Filter by decision" aria-haspopup="listbox" aria-expanded={open} aria-controls={id}
      data-filter-value={value} data-review-keyboard="filter"
      onFocus={event => {
        if (!restoringFocus.current && !container.current?.contains(event.relatedTarget) &&
          event.currentTarget.matches(':focus-visible')) start(0);
      }}
      onClick={event => { if (open) setOpen(false); else start(event.detail === 0 ? 0 : Math.max(0, current)); }}>
      <ListFilter size={15}/><span>{label}</span><ChevronDown size={13}/>
    </button>
    {open && <div id={id} ref={options} className="review-filter-options" role="listbox" aria-label="Decision filters">
      {FILTER_OPTIONS.map(option => <button type="button" key={option.value} role="option"
        aria-selected={option.value === value} tabIndex={-1} onClick={() => choose(option.value)}>
        <span>{option.label}</span>{option.value === value && <Check size={14}/>}
      </button>)}
    </div>}
  </div>;
}
