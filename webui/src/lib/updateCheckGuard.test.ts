import assert from 'node:assert/strict';
import test from 'node:test';
import { createLatestRequestGuard } from './updateCheckGuard.ts';

test('supersedes stale responses when a newer channel check starts', () => {
  const guard = createLatestRequestGuard();
  const stableSeq = guard.next();
  assert.equal(guard.isCurrent(stableSeq), true);

  const betaSeq = guard.next();
  assert.equal(guard.isCurrent(stableSeq), false);
  assert.equal(guard.isCurrent(betaSeq), true);
});

test('invalidate rejects in-flight checks without locking future checks', () => {
  const guard = createLatestRequestGuard();
  const inflight = guard.next();
  guard.invalidate();
  assert.equal(guard.isCurrent(inflight), false);

  const nextMount = guard.next();
  assert.equal(guard.isCurrent(nextMount), true);
});
