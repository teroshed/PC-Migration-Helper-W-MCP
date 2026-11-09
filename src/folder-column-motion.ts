import { useLayoutEffect, useRef } from 'react';
import type { RefObject } from 'react';
import { parentPath } from './model';
import type { ViewState } from './model';
import { reviewPathKey } from './ReviewActivity';
import './folder-column-motion.css';

const DURATION_MS = 190;
const EASE = 'cubic-bezier(.2,.7,.3,1)';
const REDUCED_MOTION = '(prefers-reduced-motion: reduce)';
type Movement = 'back' | 'center' | 'preview';

interface ColumnPosition {
  left: number;
  top: number;
  width: number;
  height: number;
  opacity: number;
}
interface Transition {
  nextPaths: string[];
  movement: Movement;
  positions: Map<string, ColumnPosition>;
  exiting: Array<{ snapshot: HTMLElement; position: ColumnPosition }>;
}

function columnPaths(view: ViewState): string[] {
  return [view.path, ...view.previewColumns.map(column => column.path)].map(reviewPathKey);
}

function samePaths(left: string[], right: string[]): boolean {
  return left.length === right.length && left.every((path, index) => path === right[index]);
}

function stripColumns(strip: HTMLElement): HTMLElement[] {
  return Array.from(strip.querySelectorAll<HTMLElement>(':scope > [data-column-path]'));
}

function copyForExit(element: HTMLElement): HTMLElement {
  const snapshot = element.cloneNode(true) as HTMLElement;
  snapshot.dataset.motionExitPath = reviewPathKey(element.dataset.columnPath ?? '');
  snapshot.removeAttribute('id');
  snapshot.removeAttribute('data-column-depth');
  snapshot.removeAttribute('data-column-path');
  snapshot.querySelectorAll('[id]').forEach(child => child.removeAttribute('id'));
  const originalList = element.querySelector<HTMLElement>('.column-entry-list');
  const exitList = snapshot.querySelector<HTMLElement>('.column-entry-list');
  // Detached elements cannot keep scroll offsets. Apply the offset after insertion.
  if (originalList && exitList) exitList.dataset.motionScroll = String(originalList.scrollTop);
  return snapshot;
}

export function useFolderColumnMotion(stripRef: RefObject<HTMLDivElement | null>, view: ViewState | null): (previous: ViewState, next: ViewState) => void {
  const pending = useRef<Transition | null>(null);
  const running = useRef(new Map<HTMLElement, Animation>());
  const exits = useRef(new Map<HTMLElement, Animation[]>());
  const branch = view ? JSON.stringify(columnPaths(view)) : '';

  useLayoutEffect(() => {
    const media = window.matchMedia(REDUCED_MOTION);
    function stop(): void {
      pending.current = null;
      for (const animation of running.current.values()) animation.cancel();
      running.current.clear();
      for (const [overlay, animations] of exits.current) {
        for (const animation of animations) animation.cancel();
        overlay.remove();
      }
      exits.current.clear();
    }
    function preferenceChanged(event: MediaQueryListEvent): void {
      if (event.matches) stop();
    }
    media.addEventListener('change', preferenceChanged);
    return () => {
      media.removeEventListener('change', preferenceChanged);
      stop();
    };
  }, []);

  useLayoutEffect(() => {
    const plan = pending.current;
    pending.current = null;
    const strip = stripRef.current;
    if (!plan || !view || !strip || !samePaths(plan.nextPaths, columnPaths(view))) return;
    const frame = strip.parentElement;
    if (!frame || window.matchMedia(REDUCED_MOTION).matches) return;

    // The scroll-restoration effect runs first. Measure the committed viewport,
    // then move each live column from its previous visible position.
    const bounds = strip.getBoundingClientRect();
    const columns = stripColumns(strip);
    const targets = columns.map(element => {
      const position = element.getBoundingClientRect();
      return { element, left: position.left - bounds.left, right: position.right - bounds.left };
    });
    for (const { element, left, right } of targets) {
      const previous = plan.positions.get(reviewPathKey(element.dataset.columnPath ?? ''));
      const offset = previous ? previous.left - left : plan.movement === 'back' ? -36 : 36;
      const opacity = previous?.opacity ?? .15;
      const start = left + offset;
      const end = right + offset;
      if (Math.max(right, end) <= 0 || Math.min(left, start) >= strip.clientWidth) continue;
      if (Math.abs(offset) < 1 && opacity > .99) continue;
      const animation = element.animate(
        [{ transform: `translateX(${offset}px)`, opacity }, { transform: 'translateX(0px)', opacity: 1 }],
        { duration: DURATION_MS, easing: EASE },
      );
      running.current.set(element, animation);
      void animation.finished.catch(() => {}).then(() => {
        if (running.current.get(element) === animation) running.current.delete(element);
      });
    }

    if (plan.exiting.length === 0) return;
    const overlay = window.document.createElement('div');
    overlay.className = 'folder-column-exits';
    overlay.inert = true;
    overlay.setAttribute('aria-hidden', 'true');
    overlay.style.width = `${strip.clientWidth}px`;
    overlay.style.height = `${strip.clientHeight}px`;
    frame.appendChild(overlay);
    // Centering sends the discarded ancestors left as a group. A closed or
    // replaced preview takes the shorter fade to the right.
    const exitDistance = plan.movement === 'center'
      ? -Math.max(...plan.exiting.map(({ position }) => position.left + position.width), 36) - 12
      : 38;
    const animations = plan.exiting.map(({ snapshot, position }) => {
      snapshot.style.left = `${position.left}px`;
      snapshot.style.top = `${position.top}px`;
      snapshot.style.width = `${position.width}px`;
      snapshot.style.height = `${position.height}px`;
      overlay.appendChild(snapshot);
      const list = snapshot.querySelector<HTMLElement>('[data-motion-scroll]');
      if (list) list.scrollTop = Number(list.dataset.motionScroll);
      return snapshot.animate(
        [{ transform: 'translateX(0px)', opacity: position.opacity },
          { transform: `translateX(${exitDistance}px)`, opacity: 0 }],
        { duration: DURATION_MS, easing: EASE },
      );
    });
    exits.current.set(overlay, animations);
    void Promise.allSettled(animations.map(animation => animation.finished)).then(() => {
      exits.current.delete(overlay);
      overlay.remove();
    });
  }, [branch, stripRef]);

  return (previous, next) => {
    const previousPaths = columnPaths(previous);
    const nextPaths = columnPaths(next);
    if (samePaths(previousPaths, nextPaths)) return;
    pending.current = null;
    const strip = stripRef.current;
    if (!strip || typeof strip.animate !== 'function' || window.matchMedia(REDUCED_MOTION).matches) return;
    const parent = parentPath(next.path);
    let movement: Movement | null = null;
    if (previousPaths[0] === nextPaths[0]) movement = 'preview';
    else if (previousPaths[0] === nextPaths[1]) movement = 'back';
    else if (previousPaths.includes(nextPaths[0] ?? '') || parent && previousPaths.includes(reviewPathKey(parent))) {
      movement = 'center';
    }
    const bounds = strip.getBoundingClientRect();
    const surviving = new Set(nextPaths);
    const positions: Transition['positions'] = new Map();
    const exiting: Transition['exiting'] = [];
    // Sample current animated transforms before cancellation, so rapid
    // navigation starts at the visible position of each surviving column.
    if (movement) for (const element of stripColumns(strip)) {
      const key = reviewPathKey(element.dataset.columnPath ?? '');
      const rect = element.getBoundingClientRect();
      const position = {
        left: rect.left - bounds.left,
        top: rect.top - bounds.top,
        width: rect.width,
        height: rect.height,
        opacity: running.current.has(element) ? Number(getComputedStyle(element).opacity) : 1,
      };
      positions.set(key, position);
      if (!surviving.has(key) && position.left + position.width > 0 && position.left < strip.clientWidth) {
        exiting.push({ snapshot: copyForExit(element), position });
      }
    }
    // A folder reopened mid-fade becomes interactive immediately and starts
    // from the position of its fading snapshot.
    for (const overlay of exits.current.keys()) {
      for (const snapshot of Array.from(overlay.children)) {
        if (!(snapshot instanceof HTMLElement)) continue;
        const key = snapshot.dataset.motionExitPath;
        if (!key || !surviving.has(key)) continue;
        if (movement && !positions.has(key)) {
          const rect = snapshot.getBoundingClientRect();
          positions.set(key, {
            left: rect.left - bounds.left, top: rect.top - bounds.top,
            width: rect.width, height: rect.height, opacity: Number(getComputedStyle(snapshot).opacity),
          });
        }
        for (const animation of snapshot.getAnimations()) animation.cancel();
        snapshot.remove();
      }
      if (!overlay.childElementCount) {
        exits.current.delete(overlay);
        overlay.remove();
      }
    }
    for (const animation of running.current.values()) animation.cancel();
    running.current.clear();
    if (movement) pending.current = { nextPaths, movement, positions, exiting };
  };
}
