/**
 * 编辑器标签现场簿记（主画布与右侧预览共用）。
 *
 * 文件标签必须和浏览器标签一样：切走再切回，编辑/预览模式、未保存草稿、
 * 光标选区与滚动位置都不能丢。
 * - 保活 DOM（display 显隐切换）承担常规的标签互切，零重挂载；
 * - 落地页 / 会话画布整棵树互换等无法保活的场景，则从这里恢复最后现场。
 *
 * 现场只驻留内存并按标签数量做 LRU 上限，关闭标签后写入会被墓碑拦截，
 * 避免卸载清理把刚关闭标签的草稿又写回缓存。
 */

import type { FileStamp } from './fileStamp';
import type { OpenDiffTab } from './openDiffTab';

export type TextEditorMode = 'edit' | 'preview';
export type EditorViewMode = TextEditorMode | 'image' | 'binary';

export interface EditorTabSnapshot {
  content: string;
  isDirty: boolean;
  viewMode: EditorViewMode;
  imgScale: number;
  selectionStart: number;
  selectionEnd: number;
  scrollTop: number;
  scrollLeft: number;
  gutterScrollTop: number;
  previewScrollTop: number;
  /** 最后一次从磁盘读到的文件戳：重新挂载后仍能判断“当前看到的版本是不是已经旧了”。 */
  diskStamp: FileStamp | null;
}

const MAX_SNAPSHOTS = 32;
const MAX_TOMBSTONES = 256;

const snapshots = new Map<string, EditorTabSnapshot>();
// 已关闭标签的墓碑：卸载清理可能晚于关闭动作，必须在写入侧拦截，重新打开时再解除。
const closedKeys = new Set<string>();

/** 主画布标签的现场键：同一文件在主画布与右侧预览各自独立，互不串草稿。 */
export function mainEditorStateKey(filePath: string): string {
  return `main:${filePath}`;
}

/** 右侧预览标签的现场键。 */
export function previewEditorStateKey(filePath: string): string {
  return `preview:${filePath}`;
}

export function readEditorTabSnapshot(stateKey: string | null | undefined): EditorTabSnapshot | null {
  if (!stateKey) return null;
  closedKeys.delete(stateKey);
  return snapshots.get(stateKey) ?? null;
}

export function writeEditorTabSnapshot(
  stateKey: string | null | undefined,
  snapshot: EditorTabSnapshot,
): void {
  if (!stateKey || closedKeys.has(stateKey)) return;
  // 重新插入以维持 LRU 顺序：最近活跃的标签在容量淘汰时最后被丢弃。
  snapshots.delete(stateKey);
  snapshots.set(stateKey, snapshot);
  while (snapshots.size > MAX_SNAPSHOTS) {
    const oldest = snapshots.keys().next();
    if (oldest.done) break;
    snapshots.delete(oldest.value);
  }
}

/** 关闭标签：丢弃现场，并在该键重新挂载（read）前拒绝写回。 */
export function dropEditorTabSnapshot(stateKey: string | null | undefined): void {
  if (!stateKey) return;
  snapshots.delete(stateKey);
  closedKeys.add(stateKey);
  while (closedKeys.size > MAX_TOMBSTONES) {
    const oldest = closedKeys.values().next();
    if (oldest.done) break;
    closedKeys.delete(oldest.value);
  }
}

/** 文本标签的编辑/预览互切；图片与二进制视图切到源码编辑。 */
export function toggleTextMode(mode: EditorViewMode): TextEditorMode {
  return mode === 'edit' ? 'preview' : 'edit';
}

/** 记录标签自身的文本模式；值未变化时返回原引用，避免无谓重渲染。 */
export function withTabViewMode<T extends string>(
  map: Record<string, T>,
  tabId: string,
  mode: T,
): Record<string, T> {
  if (!tabId || map[tabId] === mode) return map;
  return { ...map, [tabId]: mode };
}

/** 首次激活即加入常驻挂载集合（lazy mount），之后切走只隐藏、不卸载。 */
export function withVisitedTab(
  visited: Record<string, true>,
  tabId: string,
): Record<string, true> {
  if (!tabId || visited[tabId]) return visited;
  return { ...visited, [tabId]: true };
}

/** 关闭标签时同步清掉它的模式 / 刷新令牌 / 挂载记录。 */
export function withoutTabEntry<T>(map: Record<string, T>, tabId: string): Record<string, T> {
  if (!tabId || !(tabId in map)) return map;
  const next = { ...map };
  delete next[tabId];
  return next;
}

/** 自增某个标签的强制重载令牌（顶栏刷新按钮）。 */
export function bumpReloadToken(
  tokens: Record<string, number>,
  tabId: string,
): Record<string, number> {
  if (!tabId) return tokens;
  return { ...tokens, [tabId]: (tokens[tabId] ?? 0) + 1 };
}

/**
 * 是否允许用磁盘内容覆盖编辑器：
 * 未保存草稿只有在用户显式刷新时才让位，后台重新挂载 / cwd 变更一律不得静默清空。
 */
export function shouldReloadFromDisk(isDirty: boolean, forced: boolean): boolean {
  return forced || !isDirty;
}

export interface UpsertEditorTabInput {
  normPath: string;
  startLine?: number;
  endLine?: number;
  insertIndex?: number;
}

/**
 * Pure reducer for inserting or updating an `'editor'` tab inside `diffTabs`.
 * Guarantees consecutive `setDiffTabs((prev) => ...)` calls in the same render tick
 * never clobber earlier insertions and clamps `insertIndex` consistently.
 */
export function upsertEditorTab(
  tabs: OpenDiffTab[],
  input: UpsertEditorTabInput,
): OpenDiffTab[] {
  const { normPath, startLine, endLine, insertIndex } = input;
  const tabId = `editor_${normPath}`;
  const existingIdx = tabs.findIndex(
    (t) => t.id === tabId || (t.type === 'editor' && t.filePath === normPath),
  );
  if (existingIdx !== -1) {
    const next = [...tabs];
    const existing = next[existingIdx]!;
    next[existingIdx] = {
      ...existing,
      startLine,
      endLine,
    };
    if (insertIndex !== undefined) {
      const [item] = next.splice(existingIdx, 1);
      const rawTarget = insertIndex > existingIdx ? insertIndex - 1 : insertIndex;
      const targetSlot = Math.max(0, Math.min(rawTarget, next.length));
      if (targetSlot !== existingIdx) {
        next.splice(targetSlot, 0, item!);
      } else {
        next.splice(existingIdx, 0, item!);
      }
    }
    return next;
  }

  const fileName = normPath.split(/[\\/]/).pop() || normPath;
  const ext = fileName.split('.').pop() || 'FILE';
  const newTab: OpenDiffTab = {
    id: tabId,
    type: 'editor',
    commitHash: '',
    commitShortHash: '',
    commitMessage: '',
    fileName,
    filePath: normPath,
    fileStatus: ext.slice(0, 3).toUpperCase(),
    diffText: '',
    loading: false,
    startLine,
    endLine,
  };
  if (insertIndex !== undefined) {
    const clamped = Math.max(0, Math.min(insertIndex, tabs.length));
    const next = [...tabs];
    next.splice(clamped, 0, newTab);
    return next;
  }
  return [...tabs, newTab];
}
