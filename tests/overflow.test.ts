import assert from 'node:assert/strict';
import test from 'node:test';
import { measureOverflow, NO_OVERFLOW } from '../src/overflow-state.ts';

const overflowing = {
  clientWidth: 300,
  clientHeight: 200,
  scrollWidth: 900,
  scrollHeight: 800,
  scrollLeft: 0,
  scrollTop: 0,
};

test('continuation follows each actual edge from start through middle to end', () => {
  assert.deepEqual(measureOverflow(overflowing), {
    top: false, bottom: true, left: false, right: true,
  });
  assert.deepEqual(measureOverflow({ ...overflowing, scrollLeft: 275, scrollTop: 300 }), {
    top: true, bottom: true, left: true, right: true,
  });
  assert.deepEqual(measureOverflow({ ...overflowing, scrollLeft: 600, scrollTop: 600 }), {
    top: true, bottom: false, left: true, right: false,
  });
});

test('empty, fitting and removed overflow clear every cue, including after resize', () => {
  assert.deepEqual(measureOverflow({ ...overflowing, scrollWidth: 300, scrollHeight: 200 }), NO_OVERFLOW);
  assert.deepEqual(measureOverflow({ ...overflowing, scrollWidth: 0, scrollHeight: 0 }), NO_OVERFLOW);
  assert.deepEqual(measureOverflow({ ...overflowing, clientWidth: 900, clientHeight: 800, scrollLeft: 250, scrollTop: 350 }), NO_OVERFLOW);
  assert.deepEqual(measureOverflow({ ...overflowing, clientWidth: 0, clientHeight: 0 }), NO_OVERFLOW);
});

test('fractional end positions and elastic overscroll do not leave false continuation', () => {
  assert.deepEqual(measureOverflow({ ...overflowing, scrollLeft: 599.4, scrollTop: 599.6 }), {
    top: true, bottom: false, left: true, right: false,
  });
  assert.deepEqual(measureOverflow({ ...overflowing, scrollLeft: -10, scrollTop: -30 }), {
    top: false, bottom: true, left: false, right: true,
  });
  assert.deepEqual(measureOverflow({ ...overflowing, scrollLeft: 640, scrollTop: 670 }), {
    top: true, bottom: false, left: true, right: false,
  });
  assert.deepEqual(measureOverflow({ ...overflowing, scrollWidth: 300.8, scrollHeight: 200.6 }), NO_OVERFLOW);
});

test('each independent axis ignores overflow that belongs to another scroll region', () => {
  assert.deepEqual(measureOverflow(overflowing, 'vertical'), {
    top: false, bottom: true, left: false, right: false,
  });
  assert.deepEqual(measureOverflow(overflowing, 'horizontal'), {
    top: false, bottom: false, left: false, right: true,
  });
});
