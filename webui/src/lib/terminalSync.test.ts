import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  buildCloseAllFallbackTabId,
  executeCloseAllTabsTransition,
  isTerminalCloseAllCommitSafe,
  isTerminalSyncStale,
  type CreatedTerminalTab,
} from './terminalSync.ts';

test('isTerminalSyncStale flags active closeAllTabs pass, generation skew, and scope mismatch', () => {
  const base = {
    syncGeneration: 2,
    expectedGeneration: 2,
    activeCwd: '/repo/a',
    targetCwd: '/repo/a',
    activeScope: '/repo/a',
    targetScope: '/repo/a',
    closingAllCount: 0,
  };
  assert.equal(isTerminalSyncStale(base), false);
  assert.equal(isTerminalSyncStale({ ...base, closingAllCount: 1 }), true);
  assert.equal(isTerminalSyncStale({ ...base, syncGeneration: 3 }), true);
  assert.equal(isTerminalSyncStale({ ...base, activeCwd: '/repo/b' }), true);
  assert.equal(isTerminalSyncStale({ ...base, activeScope: '/repo/b' }), true);
});

test('executeCloseAllTabsTransition closes old tabs before creating backend replacement and blocks concurrent sync/reentry', async () => {
  const events: string[] = [];
  let closingAllCount = 0;
  let syncGeneration = 1;
  let committed: CreatedTerminalTab | null = null;
  let releaseClose!: () => void;
  const closeGate = new Promise<void>((resolve) => {
    releaseClose = resolve;
  });

  const transition = executeCloseAllTabsTransition({
    oldTabs: [{ id: 'old-1' }, { id: 'old-2' }],
    preferredShell: 'powershell',
    targetCwd: 'C:/repo',
    targetScope: 'c:/repo',
    getClosingAllCount: () => closingAllCount,
    incrementClosingAllCount: () => {
      closingAllCount += 1;
    },
    decrementClosingAllCount: () => {
      closingAllCount = Math.max(0, closingAllCount - 1);
    },
    bumpSyncGeneration: () => {
      syncGeneration += 1;
      return syncGeneration;
    },
    getSyncGeneration: () => syncGeneration,
    getActiveCwd: () => 'C:/repo',
    getActiveScope: () => 'c:/repo',
    disposeAllActiveRecords: () => {
      events.push('dispose');
    },
    closeTerminalSession: async (id) => {
      events.push(`close-start:${id}`);
      await closeGate;
      events.push(`close-done:${id}`);
    },
    createTerminalSession: async ({ shell, cwd }) => {
      events.push(`create:${shell}:${cwd}`);
      return {
        terminal_id: 'backend-new-1',
        name: 'powershell 1',
        cwd: cwd || '.',
      };
    },
    commitReplacementTab: (tab) => {
      committed = tab;
      events.push(`commit:${tab.id}`);
    },
    broadcastSync: (cwd) => {
      events.push(`broadcast:${cwd}`);
    },
    notifyClosed: () => {
      events.push('toast');
    },
    now: () => 1700000000000,
  });

  // While closes are in-flight, closingAllCount > 0 blocks both stale syncs and re-entrant clicks.
  assert.equal(closingAllCount, 1);
  assert.equal(
    isTerminalSyncStale({
      syncGeneration,
      expectedGeneration: syncGeneration,
      activeCwd: 'C:/repo',
      targetCwd: 'C:/repo',
      activeScope: 'c:/repo',
      targetScope: 'c:/repo',
      closingAllCount,
    }),
    true
  );
  const reentrant = await executeCloseAllTabsTransition({
    oldTabs: [{ id: 'old-1' }],
    preferredShell: 'powershell',
    targetCwd: 'C:/repo',
    targetScope: 'c:/repo',
    getClosingAllCount: () => closingAllCount,
    incrementClosingAllCount: () => {
      closingAllCount += 1;
    },
    decrementClosingAllCount: () => {
      closingAllCount = Math.max(0, closingAllCount - 1);
    },
    bumpSyncGeneration: () => ++syncGeneration,
    getSyncGeneration: () => syncGeneration,
    getActiveCwd: () => 'C:/repo',
    getActiveScope: () => 'c:/repo',
    disposeAllActiveRecords: () => {},
    closeTerminalSession: async () => {},
    createTerminalSession: async () => ({ terminal_id: 'never', name: 'never', cwd: '.' }),
    commitReplacementTab: () => {},
    broadcastSync: () => {},
    notifyClosed: () => {},
  });
  assert.equal(reentrant, null);

  releaseClose();
  const result = await transition;
  assert.equal(closingAllCount, 0);
  assert.equal(result?.id, 'backend-new-1');
  assert.equal(committed?.id, 'backend-new-1');

  const createIdx = events.indexOf('create:powershell:C:/repo');
  const closeDone1 = events.indexOf('close-done:old-1');
  const closeDone2 = events.indexOf('close-done:old-2');
  assert.ok(createIdx > closeDone1 && createIdx > closeDone2);
  assert.deepEqual(events.slice(-4), [
    'dispose',
    'commit:backend-new-1',
    'broadcast:C:/repo',
    'toast',
  ]);
});

test('executeCloseAllTabsTransition falls back to unique local tab on create failure and aborts if workspace switches mid-flight', async () => {
  let closingAllCount = 0;
  let syncGeneration = 1;
  let committed: CreatedTerminalTab | null = null;

  const fallbackResult = await executeCloseAllTabsTransition({
    oldTabs: [{ id: 'old-1' }],
    preferredShell: 'gitbash',
    targetCwd: 'C:/repo-1',
    targetScope: 'c:/repo-1',
    getClosingAllCount: () => closingAllCount,
    incrementClosingAllCount: () => {
      closingAllCount += 1;
    },
    decrementClosingAllCount: () => {
      closingAllCount = Math.max(0, closingAllCount - 1);
    },
    bumpSyncGeneration: () => ++syncGeneration,
    getSyncGeneration: () => syncGeneration,
    getActiveCwd: () => 'C:/repo-1',
    getActiveScope: () => 'c:/repo-1',
    disposeAllActiveRecords: () => {},
    closeTerminalSession: async () => {},
    createTerminalSession: async () => {
      throw new Error('offline');
    },
    commitReplacementTab: (tab) => {
      committed = tab;
    },
    broadcastSync: () => {},
    notifyClosed: () => {},
    now: () => 42,
    randomSuffix: () => 'abcd',
  });
  assert.equal(closingAllCount, 0);
  assert.equal(fallbackResult?.id, buildCloseAllFallbackTabId(42, 'abcd'));
  assert.equal(committed?.id, 'term_42_abcd');

  // Workspace switch mid-flight must abort commit and clean up newly created backend tab.
  let activeCwd = 'C:/repo-1';
  let activeScope = 'c:/repo-1';
  const cleanedUp: string[] = [];
  let committedAfterSwitch = false;
  const aborted = await executeCloseAllTabsTransition({
    oldTabs: [{ id: 'old-1' }],
    preferredShell: 'powershell',
    targetCwd: 'C:/repo-1',
    targetScope: 'c:/repo-1',
    getClosingAllCount: () => closingAllCount,
    incrementClosingAllCount: () => {
      closingAllCount += 1;
    },
    decrementClosingAllCount: () => {
      closingAllCount = Math.max(0, closingAllCount - 1);
    },
    bumpSyncGeneration: () => ++syncGeneration,
    getSyncGeneration: () => syncGeneration,
    getActiveCwd: () => activeCwd,
    getActiveScope: () => activeScope,
    disposeAllActiveRecords: () => {},
    closeTerminalSession: async (id) => {
      cleanedUp.push(id);
    },
    createTerminalSession: async () => {
      activeCwd = 'C:/repo-2';
      activeScope = 'c:/repo-2';
      return { terminal_id: 'orphaned-id', name: 'powershell 1', cwd: 'C:/repo-1' };
    },
    commitReplacementTab: () => {
      committedAfterSwitch = true;
    },
    broadcastSync: () => {},
    notifyClosed: () => {},
  });
  assert.equal(aborted, null);
  assert.equal(committedAfterSwitch, false);
  assert.equal(
    isTerminalCloseAllCommitSafe({
      syncGeneration,
      expectedGeneration: syncGeneration,
      activeCwd,
      targetCwd: 'C:/repo-1',
      activeScope,
      targetScope: 'c:/repo-1',
    }),
    false
  );
  assert.deepEqual(cleanedUp, ['old-1', 'orphaned-id']);
});
