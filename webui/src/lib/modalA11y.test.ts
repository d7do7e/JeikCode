import assert from 'node:assert/strict';
import test from 'node:test';
import {
  isTopmostDialog,
  nextFocusTrapIndex,
  shouldIgnoreKeyboardActivateTarget,
} from './modalA11y.ts';

test('isTopmostDialog only returns true for the last open modal in stack order', () => {
  const parent = { id: 'parent' };
  const nested = { id: 'nested' };
  assert.equal(isTopmostDialog(parent, [parent]), true);
  assert.equal(isTopmostDialog(parent, [parent, nested]), false);
  assert.equal(isTopmostDialog(nested, [parent, nested]), true);
  assert.equal(isTopmostDialog(parent, []), false);
});

test('nextFocusTrapIndex wraps forward and backward across dialog focusables', () => {
  assert.equal(nextFocusTrapIndex(-1, 0, false), -1);
  assert.equal(nextFocusTrapIndex(-1, 3, false), 0);
  assert.equal(nextFocusTrapIndex(-1, 3, true), 2);
  assert.equal(nextFocusTrapIndex(2, 3, false), 0);
  assert.equal(nextFocusTrapIndex(0, 3, true), 2);
  assert.equal(nextFocusTrapIndex(1, 3, false), 2);
});

test('shouldIgnoreKeyboardActivateTarget skips nested buttons/inputs but allows row or span descendants', () => {
  const row = { id: 'row' };
  const childBtn = { id: 'btn' };
  const rowTarget = {
    closest: () => row,
  };
  const innerSpanTarget = {
    closest: () => row,
  };
  const nestedButtonTarget = {
    closest: () => childBtn,
  };
  assert.equal(shouldIgnoreKeyboardActivateTarget(rowTarget, row), false);
  assert.equal(shouldIgnoreKeyboardActivateTarget(innerSpanTarget, row), false);
  assert.equal(shouldIgnoreKeyboardActivateTarget(nestedButtonTarget, row), true);
});
