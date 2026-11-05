import { useEffect, useRef, useState } from 'react';
import type { CSSProperties, RefObject } from 'react';
import { measureOverflow, NO_OVERFLOW } from './overflow-state';
import type { OverflowAxis, OverflowState } from './overflow-state';
import './overflow.css';

export interface OverflowIndicatorsProps {
  targetRef: RefObject<HTMLElement | null>;
  axis?: OverflowAxis;
  refreshKey?: unknown;
}

interface IndicatorsState {
  edges: OverflowState;
  left: number;
  top: number;
  width: number;
  height: number;
}

const INITIAL_STATE: IndicatorsState = {
  edges: NO_OVERFLOW,
  left: 0,
  top: 0,
  width: 0,
  height: 0,
};

function unchanged(previous: IndicatorsState, next: IndicatorsState): boolean {
  return previous.edges.top === next.edges.top
    && previous.edges.bottom === next.edges.bottom
    && previous.edges.left === next.edges.left
    && previous.edges.right === next.edges.right
    && previous.left === next.left
    && previous.top === next.top
    && previous.width === next.width
    && previous.height === next.height;
}

/**
 * Put this beside the actual scroller in a position:relative wrapper. The overlay
 * follows the scrollport, excluding its native scrollbars, and never receives input.
 */
export function OverflowIndicators({
  targetRef,
  axis = 'both',
  refreshKey,
}: OverflowIndicatorsProps) {
  const overlayRef = useRef<HTMLSpanElement>(null);
  const [state, setState] = useState<IndicatorsState>(INITIAL_STATE);

  useEffect(() => {
    const target = targetRef.current;
    const host = overlayRef.current?.parentElement;
    if (!target || !host) return;
    const scrollElement = target;
    const frameElement = host;

    let frame: number | null = null;
    let disposed = false;

    function measure(): void {
      frame = null;
      if (disposed) return;
      const targetBounds = scrollElement.getBoundingClientRect();
      const hostBounds = frameElement.getBoundingClientRect();
      const horizontalRange = Math.max(0, scrollElement.scrollWidth - scrollElement.clientWidth);
      const left = getComputedStyle(scrollElement).direction === 'rtl'
        ? horizontalRange + scrollElement.scrollLeft
        : scrollElement.scrollLeft;
      const next: IndicatorsState = {
        edges: measureOverflow({
          clientWidth: scrollElement.clientWidth,
          clientHeight: scrollElement.clientHeight,
          scrollWidth: scrollElement.scrollWidth,
          scrollHeight: scrollElement.scrollHeight,
          scrollLeft: left,
          scrollTop: scrollElement.scrollTop,
        }, axis),
        left: targetBounds.left - hostBounds.left + scrollElement.clientLeft,
        top: targetBounds.top - hostBounds.top + scrollElement.clientTop,
        width: scrollElement.clientWidth,
        height: scrollElement.clientHeight,
      };
      setState(previous => unchanged(previous, next) ? previous : next);
    }

    function scheduleMeasurement(): void {
      if (disposed || frame !== null) return;
      frame = requestAnimationFrame(measure);
    }

    const resize = new ResizeObserver(scheduleMeasurement);
    function observeBounds(): void {
      resize.disconnect();
      resize.observe(scrollElement);
      // First/last child bounds also cover font and row-height changes in a fixed viewport.
      if (scrollElement.firstElementChild) resize.observe(scrollElement.firstElementChild);
      if (scrollElement.lastElementChild && scrollElement.lastElementChild !== scrollElement.firstElementChild) {
        resize.observe(scrollElement.lastElementChild);
      }
    }
    const content = new MutationObserver(() => {
      observeBounds();
      scheduleMeasurement();
    });
    scrollElement.addEventListener('scroll', scheduleMeasurement, { passive: true });
    window.addEventListener('resize', scheduleMeasurement, { passive: true });
    content.observe(scrollElement, { childList: true, subtree: true, characterData: true });
    observeBounds();
    scheduleMeasurement();

    return () => {
      disposed = true;
      if (frame !== null) cancelAnimationFrame(frame);
      scrollElement.removeEventListener('scroll', scheduleMeasurement);
      window.removeEventListener('resize', scheduleMeasurement);
      resize.disconnect();
      content.disconnect();
    };
  }, [targetRef, axis, refreshKey]);

  const style: CSSProperties = {
    left: state.left,
    top: state.top,
    width: state.width,
    height: state.height,
  };

  return <span ref={overlayRef} className="overflow-indicators" style={style} aria-hidden="true">
    {state.edges.top && <span className="overflow-edge overflow-edge-top"><span>⌃</span></span>}
    {state.edges.bottom && <span className="overflow-edge overflow-edge-bottom"><span>⌄</span></span>}
    {state.edges.left && <span className="overflow-edge overflow-edge-left"><span>‹</span></span>}
    {state.edges.right && <span className="overflow-edge overflow-edge-right"><span>›</span></span>}
  </span>;
}
