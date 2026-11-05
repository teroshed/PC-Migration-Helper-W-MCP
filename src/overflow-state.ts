export type OverflowAxis = 'vertical' | 'horizontal' | 'both';

export interface ScrollMeasurements {
  clientWidth: number;
  clientHeight: number;
  scrollWidth: number;
  scrollHeight: number;
  scrollLeft: number;
  scrollTop: number;
}

export interface OverflowState {
  top: boolean;
  bottom: boolean;
  left: boolean;
  right: boolean;
}

export const NO_OVERFLOW: OverflowState = {
  top: false,
  bottom: false,
  left: false,
  right: false,
};

const EDGE_TOLERANCE_PX = 1;

function dimension(value: number): number {
  return Number.isFinite(value) ? Math.max(0, value) : 0;
}

function position(value: number, maximum: number): number {
  if (!Number.isFinite(value)) return 0;
  return Math.max(0, Math.min(maximum, value));
}

/** Physical left/top positions are used, including a normalized RTL scrollLeft. */
export function measureOverflow(
  measurements: ScrollMeasurements,
  axis: OverflowAxis = 'both',
): OverflowState {
  const width = dimension(measurements.clientWidth);
  const height = dimension(measurements.clientHeight);
  const horizontalRange = Math.max(0, dimension(measurements.scrollWidth) - width);
  const verticalRange = Math.max(0, dimension(measurements.scrollHeight) - height);
  const horizontal = axis !== 'vertical' && width > 0 && horizontalRange > EDGE_TOLERANCE_PX;
  const vertical = axis !== 'horizontal' && height > 0 && verticalRange > EDGE_TOLERANCE_PX;
  const x = position(measurements.scrollLeft, horizontalRange);
  const y = position(measurements.scrollTop, verticalRange);

  return {
    top: vertical && y > EDGE_TOLERANCE_PX,
    bottom: vertical && verticalRange - y > EDGE_TOLERANCE_PX,
    left: horizontal && x > EDGE_TOLERANCE_PX,
    right: horizontal && horizontalRange - x > EDGE_TOLERANCE_PX,
  };
}
