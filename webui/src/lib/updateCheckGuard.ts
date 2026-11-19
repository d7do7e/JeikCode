export interface LatestRequestGuard {
  next: () => number;
  isCurrent: (seq: number) => boolean;
  invalidate: () => void;
}

/**
 * Monotonic sequence guard so superseded async update checks (e.g. fast
 * stable/beta channel switches or unmounted dialogs) never overwrite newer state.
 */
export function createLatestRequestGuard(): LatestRequestGuard {
  let latestSeq = 0;
  return {
    next() {
      latestSeq += 1;
      return latestSeq;
    },
    isCurrent(seq: number) {
      return seq === latestSeq;
    },
    invalidate() {
      latestSeq += 1;
    },
  };
}
