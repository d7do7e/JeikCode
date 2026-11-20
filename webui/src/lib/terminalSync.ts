export interface TerminalSyncGuardState {
  syncGeneration: number;
  expectedGeneration: number;
  activeCwd?: string;
  targetCwd?: string;
  activeScope: string;
  targetScope: string;
  closingAllCount: number;
}

export interface CloseAllTabDescriptor {
  id: string;
}

export interface CreatedTerminalTab {
  id: string;
  name: string;
  cwd: string;
  shell: string;
  timestamp: number;
}

export interface CloseAllTabsTransitionParams<T extends CloseAllTabDescriptor> {
  oldTabs: T[];
  preferredShell: string;
  targetCwd: string | undefined;
  targetScope: string;
  getClosingAllCount: () => number;
  incrementClosingAllCount: () => void;
  decrementClosingAllCount: () => void;
  bumpSyncGeneration: () => number;
  getSyncGeneration: () => number;
  getActiveCwd: () => string | undefined;
  getActiveScope: () => string;
  disposeAllActiveRecords: () => void;
  closeTerminalSession: (id: string) => Promise<unknown>;
  createTerminalSession: (params: {
    shell: string;
    cwd?: string;
  }) => Promise<{ terminal_id: string; name: string; cwd: string }>;
  commitReplacementTab: (tab: CreatedTerminalTab) => void;
  broadcastSync: (cwd: string | undefined) => void;
  notifyClosed: () => void;
  now?: () => number;
  randomSuffix?: () => string;
}

/**
 * Returns true when an in-flight `syncWithBackend` pass has been superseded by
 * a newer generation, workspace/scope switch, or an active `closeAllTabs` pass.
 */
export function isTerminalSyncStale(state: TerminalSyncGuardState): boolean {
  return (
    state.closingAllCount > 0 ||
    state.syncGeneration !== state.expectedGeneration ||
    state.activeCwd !== state.targetCwd ||
    state.activeScope !== state.targetScope
  );
}

/**
 * Returns true when an async `closeAllTabs` pass still owns the current
 * generation, workspace cwd, and session scope and may commit UI state.
 */
export function isTerminalCloseAllCommitSafe(
  state: Omit<TerminalSyncGuardState, 'closingAllCount'>
): boolean {
  return (
    state.syncGeneration === state.expectedGeneration &&
    state.activeCwd === state.targetCwd &&
    state.activeScope === state.targetScope
  );
}

export function buildCloseAllFallbackTabId(
  now = Date.now(),
  randomSuffix = Math.random().toString(36).slice(2, 6)
): string {
  return `term_${now}_${randomSuffix}`;
}

/**
 * Coordinate closing every terminal tab and provisioning a fresh replacement
 * session on the backend before `syncWithBackend` or intermediate re-renders
 * can observe an empty backend terminal list or resurrect closing WebSockets.
 */
export async function executeCloseAllTabsTransition<T extends CloseAllTabDescriptor>(
  params: CloseAllTabsTransitionParams<T>
): Promise<CreatedTerminalTab | null> {
  if (params.getClosingAllCount() > 0) {
    return null;
  }

  const gen = params.bumpSyncGeneration();
  params.incrementClosingAllCount();
  try {
    params.disposeAllActiveRecords();
    await Promise.allSettled(
      params.oldTabs.map((tab) => params.closeTerminalSession(tab.id))
    );

    if (
      !isTerminalCloseAllCommitSafe({
        syncGeneration: params.getSyncGeneration(),
        expectedGeneration: gen,
        activeCwd: params.getActiveCwd(),
        targetCwd: params.targetCwd,
        activeScope: params.getActiveScope(),
        targetScope: params.targetScope,
      })
    ) {
      return null;
    }

    const nowFn = params.now ?? (() => Date.now());
    let newTab: CreatedTerminalTab;
    try {
      const created = await params.createTerminalSession({
        shell: params.preferredShell,
        cwd: params.targetCwd,
      });
      newTab = {
        id: created.terminal_id,
        name: `${created.name}`,
        cwd: created.cwd || params.targetCwd || '.',
        shell: params.preferredShell,
        timestamp: nowFn(),
      };
    } catch {
      newTab = {
        id: buildCloseAllFallbackTabId(nowFn(), params.randomSuffix?.()),
        name: `${params.preferredShell} 1`,
        cwd: params.targetCwd || '.',
        shell: params.preferredShell,
        timestamp: nowFn(),
      };
    }

    if (
      !isTerminalCloseAllCommitSafe({
        syncGeneration: params.getSyncGeneration(),
        expectedGeneration: gen,
        activeCwd: params.getActiveCwd(),
        targetCwd: params.targetCwd,
        activeScope: params.getActiveScope(),
        targetScope: params.targetScope,
      })
    ) {
      // A newer project switch superseded this pass after `createTerminalSession`;
      // clean up the orphaned backend session if it was created on the server.
      void params.closeTerminalSession(newTab.id).catch(() => {});
      return null;
    }

    // Clear any stray records before releasing the closing guard and mounting
    // the new replacement tab.
    params.disposeAllActiveRecords();
    params.commitReplacementTab(newTab);
    params.broadcastSync(params.targetCwd);
    params.notifyClosed();
    return newTab;
  } finally {
    params.decrementClosingAllCount();
  }
}
