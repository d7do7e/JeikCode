import { useState, useEffect, useCallback, useMemo, useRef } from 'preact/hooks';
import { createPortal } from 'preact/compat';
import {
  fetchFsTree,
  fetchFsSearch,
  fetchFsLocate,
  revealInFileExplorer,
  fetchFsAction,
  type FsEntry,
  type FsSearchResult,
  type FsLocateItem,
} from '../api';
import { useSettings } from '../settings';
import { showToast } from './Toast';
import { ConfirmDialog } from './ConfirmDialog';
import { menuPointFromEvent } from '../lib/menuPosition';
import { isLoopbackHost } from '../lib/displayPath';
import { getCachedDirectoryTree, saveCachedDirectoryTree } from '../lib/sessionCache';

export interface FileExplorerProps {
  cwd?: string;
  onOpenFile?: (filePath: string, line?: number) => void;
  activeFilePath?: string | null;
}

interface TreeNode {
  name: string;
  path: string;
  is_dir: boolean;
  expanded?: boolean;
  loading?: boolean;
  children?: TreeNode[];
  loaded?: boolean;
  is_ignored?: boolean;
}

// 模块级内存热缓存（按 cwd 索引，提供 0ms 瞬间秒开）
const explorerMemoryCache = new Map<string, TreeNode[]>();
const STORAGE_PREFIX = 'jeikcode:explorer_tree:';
const STORAGE_KEY_EXPANDED_PATHS = 'jeikcode:explorer_expanded:';
const STORAGE_KEY_SELECTED_PATHS = 'jeikcode:explorer_selected:';

function getSavedExpandedPaths(rootPath: string): Set<string> {
  try {
    const raw = localStorage.getItem(STORAGE_KEY_EXPANDED_PATHS + rootPath);
    if (raw) {
      const list = JSON.parse(raw);
      if (Array.isArray(list)) return new Set(list);
    }
  } catch {}
  return new Set();
}

function saveExpandedPaths(rootPath: string, set: Set<string>) {
  try {
    localStorage.setItem(STORAGE_KEY_EXPANDED_PATHS + rootPath, JSON.stringify(Array.from(set)));
  } catch {}
}

function getSavedSelectedPaths(rootPath: string): { paths: string[]; lastPath: string | null } | null {
  try {
    const raw = localStorage.getItem(STORAGE_KEY_SELECTED_PATHS + rootPath);
    if (raw) {
      const parsed = JSON.parse(raw);
      if (parsed && Array.isArray(parsed.paths)) return parsed;
    }
  } catch {}
  return null;
}

function saveSelectedPaths(rootPath: string, paths: string[], lastPath: string | null) {
  try {
    localStorage.setItem(STORAGE_KEY_SELECTED_PATHS + rootPath, JSON.stringify({ paths, lastPath }));
  } catch {}
}

function getCachedTree(cwd: string): TreeNode[] | null {
  const mem = explorerMemoryCache.get(cwd);
  if (mem) return mem;
  try {
    if (typeof window !== 'undefined' && window.sessionStorage) {
      const raw = window.sessionStorage.getItem(STORAGE_PREFIX + cwd);
      if (raw) {
        const parsed = JSON.parse(raw);
        if (Array.isArray(parsed)) {
          explorerMemoryCache.set(cwd, parsed);
          return parsed;
        }
      }
    }
  } catch {}
  return null;
}

function setCachedTree(cwd: string, tree: TreeNode[]) {
  explorerMemoryCache.set(cwd, tree);
  try {
    if (typeof window !== 'undefined' && window.sessionStorage) {
      window.sessionStorage.setItem(STORAGE_PREFIX + cwd, JSON.stringify(tree));
    }
  } catch {}
}

function getFileIcon(name: string, isDir: boolean, expanded = false) {
  if (isDir) {
    return expanded ? '📂' : '📁';
  }
  const ext = name.split('.').pop()?.toLowerCase() || '';
  switch (ext) {
    case 'rs':
      return '🦀';
    case 'ts':
    case 'tsx':
      return '🔷';
    case 'js':
    case 'jsx':
      return '🟨';
    case 'json':
      return '📋';
    case 'md':
    case 'markdown':
      return '📝';
    case 'css':
    case 'scss':
      return '🎨';
    case 'html':
      return '🌐';
    case 'toml':
    case 'yaml':
    case 'yml':
      return '⚙️';
    case 'png':
    case 'jpg':
    case 'svg':
      return '🖼️';
    default:
      return '📄';
  }
}

interface GroupedSearchResults {
  file: string;
  fileName: string;
  items: FsSearchResult[];
  expanded: boolean;
}

export function FileExplorer({ cwd, onOpenFile, activeFilePath }: FileExplorerProps) {
  const { t } = useSettings();
  const tRef = useRef(t);
  tRef.current = t;
  const rootPath = cwd || '.';

  const [tree, setTree] = useState<TreeNode[]>(() => getCachedTree(rootPath) || []);
  const [loading, setLoading] = useState(false);
  const [filterQuery, setFilterQuery] = useState('');
  const [searchMode, setSearchMode] = useState<'tree' | 'search'>('tree');

  // 高性能全项目文件/目录定位与相对路径搜索状态
  const [locateResults, setLocateResults] = useState<FsLocateItem[]>([]);
  const [locateLoading, setLocateLoading] = useState(false);
  const [selectedLocateIdx, setSelectedLocateIdx] = useState(0);

  // 全局内容搜索状态
  const [searchQuery, setSearchQuery] = useState('');

  // 搜索结果
  const [searchLoading, setSearchLoading] = useState(false);
  const [searchResults, setSearchResults] = useState<FsSearchResult[]>([]);
  const [expandedFiles, setExpandedFiles] = useState<Set<string>>(new Set());

  const searchInputRef = useRef<HTMLInputElement>(null);
  const containerRef = useRef<HTMLDivElement>(null);
  const activeRootRef = useRef(rootPath);
  activeRootRef.current = rootPath;
  const selectionHydratedRootRef = useRef(rootPath);

  // 右键上下文菜单与多选状态 (支持持久化记忆选中项与展开项)
  const [contextMenu, setContextMenu] = useState<{
    x: number;
    y: number;
    node: TreeNode;
    targets: string[];
  } | null>(null);
  const [expandedPaths, setExpandedPaths] = useState<Set<string>>(() => getSavedExpandedPaths(rootPath));
  const savedSelected = useMemo(() => getSavedSelectedPaths(rootPath), [rootPath]);
  const [selectedPaths, setSelectedPaths] = useState<Set<string>>(() => {
    if (savedSelected?.paths && savedSelected.paths.length > 0) {
      return new Set(savedSelected.paths);
    }
    if (activeFilePath) return new Set([activeFilePath]);
    return new Set();
  });
  const [lastSelectedPath, setLastSelectedPath] = useState<string | null>(() => {
    return savedSelected?.lastPath || activeFilePath || null;
  });
  const [deleteModal, setDeleteModal] = useState<{
    open: boolean;
    paths: string[];
  }>({ open: false, paths: [] });
  const [renameModal, setRenameModal] = useState<{
    open: boolean;
    node: TreeNode | null;
    currentName: string;
  }>({ open: false, node: null, currentName: '' });

  // 切换工作区根目录时先重水合并跳过当轮持久化，待重水合后的下一帧再写回 localStorage，
  // 绝不在同一次 effect flush 中把旧项目的 selectedPaths 写进新项目存储键
  useEffect(() => {
    if (selectionHydratedRootRef.current !== rootPath) {
      selectionHydratedRootRef.current = rootPath;
      setExpandedPaths(getSavedExpandedPaths(rootPath));
      const nextSaved = getSavedSelectedPaths(rootPath);
      if (nextSaved?.paths && nextSaved.paths.length > 0) {
        setSelectedPaths(new Set(nextSaved.paths));
        setLastSelectedPath(nextSaved.lastPath || null);
      } else if (activeFilePath) {
        setSelectedPaths(new Set([activeFilePath]));
        setLastSelectedPath(activeFilePath);
      } else {
        setSelectedPaths(new Set());
        setLastSelectedPath(null);
      }
      return;
    }
    saveSelectedPaths(rootPath, Array.from(selectedPaths), lastSelectedPath);
  }, [selectedPaths, lastSelectedPath, rootPath, activeFilePath]);

  // 高性能文件/目录全量索引与相对路径搜索过滤 (支持绝对/相对路径定位与文件名搜索双模)
  useEffect(() => {
    const q = filterQuery.trim();
    if (!q) {
      setLocateResults([]);
      setLocateLoading(false);
      return;
    }
    let cancelled = false;
    setLocateLoading(true);
    const timer = setTimeout(() => {
      fetchFsLocate(q, rootPath)
        .then((res) => {
          if (!cancelled) {
            setLocateResults(res.results || []);
            setSelectedLocateIdx(0);
          }
        })
        .catch(() => {
          if (!cancelled) setLocateResults([]);
        })
        .finally(() => {
          if (!cancelled) setLocateLoading(false);
        });
    }, 80);

    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [filterQuery, rootPath]);

  // 响应外部传入的激活文件路径
  useEffect(() => {
    if (activeFilePath) {
      setSelectedPaths((prev) => {
        const next = new Set(prev);
        next.add(activeFilePath);
        return next;
      });
      setLastSelectedPath(activeFilePath);
    }
  }, [activeFilePath]);

  // 智能递归轻量同步已展开的各层节点（对齐 Git 面板的 Single-flight 与 0ms 热恢复机制）
  const treeRef = useRef(tree);
  treeRef.current = tree;

  const refreshTree = useCallback(async (isSilent = true) => {
    if (!isSilent) setLoading(true);
    try {
      const res = await fetchFsTree(rootPath);
      const currentTree = treeRef.current;

      const syncNodes = async (entries: FsEntry[], oldNodes: TreeNode[], parentIgnored = false): Promise<TreeNode[]> => {
        const result: TreeNode[] = [];
        for (const e of entries) {
          const old = oldNodes.find((n) => n.path === e.path);
          const isExpanded = Boolean(old?.expanded);
          let children = old?.children;
          let loaded = Boolean(old?.loaded);

          const isBuiltinIgnored = e.name === 'target' || e.name === 'node_modules' || e.name === 'dist' || e.name.startsWith('.');
          const isIgnored = e.is_ignored === true
            ? true
            : e.is_ignored === false
              ? false
              : (parentIgnored || isBuiltinIgnored);

          if (e.is_dir && isExpanded) {
            try {
              const subRes = await fetchFsTree(e.path);
              children = await syncNodes(subRes.entries, old?.children || [], isIgnored);
              loaded = true;
            } catch {}
          }

          result.push({
            name: e.name,
            path: e.path,
            is_dir: e.is_dir,
            expanded: isExpanded,
            loading: false,
            children: e.is_dir ? (children || []) : undefined,
            loaded,
            is_ignored: isIgnored,
          });
        }
        return result;
      };

      const updated = await syncNodes(res.entries, currentTree);
      setTree(updated);
      setCachedTree(rootPath, updated);
    } catch {
      // 容错静默
    } finally {
      if (!isSilent) setLoading(false);
    }
  }, [rootPath]);

  // 对齐 Git 面板：监听窗口回焦 (focus)、页面可见性恢复 (visibilitychange)、Agent 写文件完成 (jeikcode:worktree_touch)、.gitignore 变动 (jeikcode:gitignore_touch)
  useEffect(() => {
    let timer: number | null = null;
    const scheduleRefresh = (forceCleanCache = false) => {
      if (typeof document !== 'undefined' && document.visibilityState !== 'visible') return;
      if (timer !== null) window.clearTimeout(timer);
      timer = window.setTimeout(() => {
        timer = null;
        if (forceCleanCache) {
          setCachedTree(rootPath, []);
          void saveCachedDirectoryTree(rootPath, []);
        }
        void refreshTree(true);
      }, forceCleanCache ? 200 : 500); // 针对 gitignore 变更给予 200ms 高优先级刷新
    };

    const onWorktreeOrFocus = () => scheduleRefresh(false);
    const onGitIgnoreTouch = () => scheduleRefresh(true);

    window.addEventListener('focus', onWorktreeOrFocus);
    document.addEventListener('visibilitychange', onWorktreeOrFocus);
    window.addEventListener('jeikcode:worktree_touch', onWorktreeOrFocus);
    window.addEventListener('jeikcode:gitignore_touch', onGitIgnoreTouch);

    return () => {
      if (timer !== null) window.clearTimeout(timer);
      window.removeEventListener('focus', onWorktreeOrFocus);
      document.removeEventListener('visibilitychange', onWorktreeOrFocus);
      window.removeEventListener('jeikcode:worktree_touch', onWorktreeOrFocus);
      window.removeEventListener('jeikcode:gitignore_touch', onGitIgnoreTouch);
    };
  }, [refreshTree, rootPath]);

  // 加载根目录
  const loadRoot = useCallback(async (force = false) => {
    const reqRoot = rootPath;
    if (!force) {
      const cached = getCachedTree(reqRoot);
      if (cached && cached.length > 0) {
        setTree(cached);
        setLoading(false);
        return;
      }
      void getCachedDirectoryTree<TreeNode[]>(reqRoot).then((idbTree) => {
        if (activeRootRef.current === reqRoot && idbTree && idbTree.length > 0) {
          setTree(idbTree);
          setLoading(false);
        }
      });
    }
    setLoading(true);
    try {
      const res = await fetchFsTree(reqRoot);
      if (activeRootRef.current !== reqRoot) return;
      const populateNodes = async (entries: FsEntry[], parentIgnored = false): Promise<TreeNode[]> => {
        const list: TreeNode[] = [];
        for (const e of entries) {
          const isBuiltinIgnored = e.name === 'target' || e.name === 'node_modules' || e.name === 'dist' || e.name.startsWith('.');
          // 若父目录被忽略，子项默认继承忽略；若后端显式返回 is_ignored === false (命中 ! 白名单)，则恢复正常色
          const isIgnored = e.is_ignored === true
            ? true
            : e.is_ignored === false
              ? false
              : (parentIgnored || isBuiltinIgnored);

          const isExp = e.is_dir && expandedPaths.has(e.path);
          let subChildren: TreeNode[] | undefined = e.is_dir ? [] : undefined;
          let isLoaded = false;
          if (isExp) {
            try {
              const subRes = await fetchFsTree(e.path);
              subChildren = await populateNodes(subRes.entries, isIgnored);
              isLoaded = true;
            } catch {}
          }
          list.push({
            name: e.name,
            path: e.path,
            is_dir: e.is_dir,
            expanded: isExp,
            loading: false,
            children: subChildren,
            loaded: isLoaded,
            is_ignored: isIgnored,
          });
        }
        return list;
      };

      const nodes = await populateNodes(res.entries);
      if (activeRootRef.current !== reqRoot) return;
      setTree(nodes);
      setCachedTree(reqRoot, nodes);
      void saveCachedDirectoryTree(reqRoot, nodes);
    } catch (err: any) {
      if (activeRootRef.current === reqRoot) {
        showToast(err?.message || tRef.current('explorer.loadTreeFailed'), 'error');
      }
    } finally {
      if (activeRootRef.current === reqRoot) {
        setLoading(false);
      }
    }
  }, [rootPath]);

  useEffect(() => {
    void loadRoot();
  }, [loadRoot]);

  // 节点展开 / 折叠
  const toggleNode = async (node: TreeNode) => {
    if (!node.is_dir) {
      onOpenFile?.(node.path);
      return;
    }

    if (node.expanded) {
      node.expanded = false;
      setExpandedPaths((prev) => {
        const next = new Set(prev);
        next.delete(node.path);
        saveExpandedPaths(rootPath, next);
        return next;
      });
      const nextTree = [...tree];
      setTree(nextTree);
      setCachedTree(rootPath, nextTree);
      return;
    }

    node.expanded = true;
    setExpandedPaths((prev) => {
      const next = new Set(prev);
      next.add(node.path);
      saveExpandedPaths(rootPath, next);
      return next;
    });
    if (!node.loaded) {
      node.loading = true;
      setTree([...tree]);
      try {
        const res = await fetchFsTree(node.path);
        const parentIgnored = Boolean(node.is_ignored);
        node.children = res.entries.map((e) => {
          const isBuiltinIgnored = e.name === 'target' || e.name === 'node_modules' || e.name === 'dist' || e.name.startsWith('.');
          const isIgnored = e.is_ignored === true
            ? true
            : e.is_ignored === false
              ? false
              : (parentIgnored || isBuiltinIgnored);
          return {
            name: e.name,
            path: e.path,
            is_dir: e.is_dir,
            expanded: false,
            loading: false,
            children: e.is_dir ? [] : undefined,
            loaded: false,
            is_ignored: isIgnored,
          };
        });
        node.loaded = true;
      } catch (err: any) {
        showToast(err?.message || tRef.current('explorer.readDirFailed'), 'error');
      } finally {
        node.loading = false;
      }
    }
    const nextTree = [...tree];
    setTree(nextTree);
    setCachedTree(rootPath, nextTree);
    void saveCachedDirectoryTree(rootPath, nextTree);
  };

  const revealAndExpandToPath = useCallback(
    async (targetPath: string, isDir: boolean) => {
      const normTarget = targetPath.replace(/\\/g, '/');
      const normRoot = rootPath.replace(/\\/g, '/').replace(/\/+$/, '');

      // 选中目标
      setSelectedPaths(new Set([normTarget]));
      setLastSelectedPath(normTarget);

      if (normTarget.startsWith(normRoot)) {
        const rel = normTarget.slice(normRoot.length).replace(/^\/+/, '');
        const segments = rel.split('/');
        const dirSegments = isDir ? segments : segments.slice(0, -1);

        const pathsToExpand = new Set(expandedPaths);
        let curr = normRoot;
        for (const seg of dirSegments) {
          curr = `${curr}/${seg}`;
          pathsToExpand.add(curr);
        }
        setExpandedPaths(pathsToExpand);
        saveExpandedPaths(rootPath, pathsToExpand);
        void refreshTree(true);
      }

      setTimeout(() => {
        const el = containerRef.current?.querySelector(`[title*="${normTarget}"]`) as HTMLElement | null;
        if (el) {
          el.scrollIntoView({ behavior: 'smooth', block: 'nearest' });
        }
      }, 180);
    },
    [rootPath, expandedPaths, refreshTree],
  );

  const handleSelectLocateItem = useCallback(
    async (item: FsLocateItem) => {
      if (!item.is_dir) {
        onOpenFile?.(item.path);
      }
      await revealAndExpandToPath(item.path, item.is_dir);
    },
    [onOpenFile, revealAndExpandToPath],
  );

  // 快捷键监听：仅当焦点在资源管理器范围内时，按 Ctrl+F 调出全局内容搜索
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      const root = containerRef.current;
      if (!root) return;
      const target = e.target as HTMLElement | null;
      const isInside = Boolean(
        (target && root.contains(target)) ||
        (document.activeElement && root.contains(document.activeElement))
      );
      if (!isInside) return;

      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'f' && !e.shiftKey) {
        e.preventDefault();
        e.stopPropagation();
        setSearchMode('search');
        setTimeout(() => searchInputRef.current?.focus(), 50);
      }
    };
    window.addEventListener('keydown', handleKeyDown);
    return () => window.removeEventListener('keydown', handleKeyDown);
  }, []);

  // 执行全局搜索
  const executeGlobalSearch = useCallback(async () => {
    if (!searchQuery.trim()) return;
    setSearchLoading(true);
    try {
      const res = await fetchFsSearch(searchQuery.trim(), rootPath);
      const results = res.results || [];
      setSearchResults(results);
      // 默认全部展开各个文件
      const allFilePaths = new Set(results.map((r) => r.file));
      setExpandedFiles(allFilePaths);
    } catch (err: any) {
      showToast(err?.message || t('explorer.searchFailed'), 'error');
    } finally {
      setSearchLoading(false);
    }
  }, [searchQuery, rootPath, t]);

  // 按文件对搜索结果进行分组
  const groupedResults = useMemo(() => {
    const map = new Map<string, FsSearchResult[]>();
    for (const r of searchResults) {
      const list = map.get(r.file) || [];
      list.push(r);
      map.set(r.file, list);
    }
    const groups: GroupedSearchResults[] = [];
    for (const [file, items] of map.entries()) {
      groups.push({
        file,
        fileName: file.split(/[\\/]/).pop() || file,
        items,
        expanded: expandedFiles.has(file),
      });
    }
    return groups;
  }, [searchResults, expandedFiles]);

  const toggleGroupExpand = (file: string) => {
    setExpandedFiles((prev) => {
      const next = new Set(prev);
      if (next.has(file)) next.delete(file);
      else next.add(file);
      return next;
    });
  };

  // 收集当前树中所有展开可见的节点（用于 Shift 连续范围选择）
  const collectVisibleNodes = useCallback((nodes: TreeNode[], acc: TreeNode[] = []): TreeNode[] => {
    for (const node of nodes) {
      acc.push(node);
      if (node.is_dir && node.expanded && node.children) {
        collectVisibleNodes(node.children, acc);
      }
    }
    return acc;
  }, []);

  const handleRowClick = (node: TreeNode, e: MouseEvent) => {
    if (e.ctrlKey || e.metaKey) {
      // Ctrl + 左键：多选切换，绝不在画布打开文件或折叠文件夹
      e.preventDefault();
      e.stopPropagation();
      setSelectedPaths((prev) => {
        const next = new Set(prev);
        if (next.has(node.path)) {
          next.delete(node.path);
        } else {
          next.add(node.path);
        }
        return next;
      });
      setLastSelectedPath(node.path);
      return;
    }

    if (e.shiftKey && lastSelectedPath) {
      // Shift + 左键：连续范围选择
      e.preventDefault();
      e.stopPropagation();
      const visible = collectVisibleNodes(tree);
      const startIdx = visible.findIndex((n) => n.path === lastSelectedPath);
      const endIdx = visible.findIndex((n) => n.path === node.path);
      if (startIdx !== -1 && endIdx !== -1) {
        const from = Math.min(startIdx, endIdx);
        const to = Math.max(startIdx, endIdx);
        const rangePaths = visible.slice(from, to + 1).map((n) => n.path);
        setSelectedPaths((prev) => {
          const next = new Set(prev);
          for (const p of rangePaths) next.add(p);
          return next;
        });
        return;
      }
    }

    // 普通左键单击：选中单项，文件则打开预览，目录则展开/折叠
    setSelectedPaths(new Set([node.path]));
    setLastSelectedPath(node.path);
    void toggleNode(node);
  };

  const handleContextMenu = (node: TreeNode, e: MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    let currentSelected = selectedPaths;
    if (!selectedPaths.has(node.path)) {
      currentSelected = new Set([node.path]);
      setSelectedPaths(currentSelected);
      setLastSelectedPath(node.path);
    }
    const { left: x, top: y } = menuPointFromEvent(e.clientX, e.clientY, 190, 220);

    setContextMenu({
      x,
      y,
      node,
      targets: Array.from(currentSelected),
    });
  };

  const triggerRename = (node: TreeNode) => {
    setContextMenu(null);
    setRenameModal({
      open: true,
      node,
      currentName: node.name,
    });
  };

  const executeRename = async (node: TreeNode, newName: string) => {
    const trimmed = (newName || '').trim();
    if (!trimmed || trimmed === node.name) {
      setRenameModal({ open: false, node: null, currentName: '' });
      return;
    }
    if (/[\\/:*?"<>|]/.test(trimmed)) {
      showToast(t('explorer.invalidName'), 'error');
      return;
    }
    try {
      await fetchFsAction('rename', {
        path: node.path,
        new_name: trimmed,
      });
      setRenameModal({ open: false, node: null, currentName: '' });
      showToast(t('explorer.renameSuccess', { name: trimmed }), 'success');
      void refreshTree(true);
      window.dispatchEvent(new CustomEvent('jeikcode:worktree_touch'));
    } catch (err: any) {
      showToast(t('explorer.renameFailed', { error: err?.message || err }), 'error');
    }
  };

  const triggerDeleteConfirm = (paths: string[]) => {
    if (paths.length === 0) return;
    setContextMenu(null);
    setDeleteModal({
      open: true,
      paths,
    });
  };

  const executeDelete = async (paths: string[]) => {
    setDeleteModal({ open: false, paths: [] });
    let successCount = 0;
    let failCount = 0;
    for (const p of paths) {
      try {
        await fetchFsAction('delete', { path: p });
        successCount += 1;
      } catch {
        failCount += 1;
      }
    }
    setSelectedPaths(new Set());
    setLastSelectedPath(null);
    if (successCount > 0) {
      showToast(t('explorer.deleteSuccess', { n: successCount }), 'success');
      void refreshTree(true);
      window.dispatchEvent(new CustomEvent('jeikcode:worktree_touch'));
    }
    if (failCount > 0) {
      showToast(t('explorer.deleteFailed', { n: failCount }), 'error');
    }
  };

  // 快捷键监听：仅当焦点落在资源管理器文件树内且无模态弹窗时，按 Delete / Ctrl+D 删除、按 F2 重命名
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      if (searchMode !== 'tree') return;
      if (document.querySelector('[role="dialog"][aria-modal="true"]')) return;
      const root = containerRef.current;
      if (!root) return;
      const target = e.target as HTMLElement | null;
      const isInside = Boolean(
        (target && root.contains(target)) ||
        (document.activeElement && root.contains(document.activeElement))
      );
      if (!isInside) return;
      const activeTag = (document.activeElement?.tagName || '').toLowerCase();
      if (activeTag === 'input' || activeTag === 'textarea' || (document.activeElement as HTMLElement | null)?.isContentEditable) {
        return;
      }

      if (e.key === 'F2') {
        const targetPath = lastSelectedPath || (selectedPaths.size === 1 ? Array.from(selectedPaths)[0] : null);
        if (targetPath) {
          const visible = collectVisibleNodes(tree);
          const found = visible.find((n) => n.path === targetPath);
          if (found) {
            e.preventDefault();
            e.stopPropagation();
            triggerRename(found);
            return;
          }
        }
      }

      const isDeleteKey = e.key === 'Delete';
      const isCtrlD = (e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'd' && !e.shiftKey && !e.altKey;
      if (isDeleteKey || isCtrlD) {
        if (selectedPaths.size > 0) {
          e.preventDefault();
          e.stopPropagation();
          triggerDeleteConfirm(Array.from(selectedPaths));
        }
      }
    };
    window.addEventListener('keydown', handleKeyDown);
    return () => window.removeEventListener('keydown', handleKeyDown);
  }, [selectedPaths, lastSelectedPath, tree, collectVisibleNodes, searchMode]);

  // 关闭右键菜单
  useEffect(() => {
    const handleClose = () => setContextMenu(null);
    window.addEventListener('click', handleClose);
    return () => window.removeEventListener('click', handleClose);
  }, []);

  // 递归渲染树节点
  const renderNode = (node: TreeNode, depth = 0) => {
    const isSelected = selectedPaths.has(node.path);
    const isActive = activeFilePath === node.path;
    const isIgnored = Boolean(node.is_ignored);
    return (
      <div key={node.path} class="file-tree-item-wrap">
        <div
          class={
            'file-tree-row' +
            (isSelected ? ' selected' : '') +
            (isActive ? ' active' : '') +
            (isIgnored ? ' is-ignored' : '')
          }
          style={{ paddingLeft: `${8 + depth * 14}px` }}
          draggable={!node.is_dir}
          onDragStart={(e) => {
            if (node.is_dir) return;
            const ext = node.name.split('.').pop() || 'FILE';
            const data = {
              id: `editor_${node.path}`,
              filePath: node.path,
              fileName: node.name,
              fileStatus: ext.slice(0, 3).toUpperCase(),
            };
            (window as any).__jeikcode_dragged_tab = data;
            e.dataTransfer?.setData('application/jeikcode-tab', JSON.stringify(data));
            e.dataTransfer?.setData('text/plain', node.path);
            if (e.dataTransfer) {
              e.dataTransfer.effectAllowed = 'all';
            }
          }}
          onDragEnd={() => {
            (window as any).__jeikcode_dragged_tab = null;
          }}
          onClick={(e) => handleRowClick(node, e)}
          onContextMenu={(e) => handleContextMenu(node, e)}
          title={isIgnored ? `${node.path} (git-ignored)` : node.path}
        >
          {node.is_dir && (
            <span class="file-tree-caret">
              {node.loading ? '⏳' : node.expanded ? '▼' : '▶'}
            </span>
          )}
          {!node.is_dir && <span class="file-tree-caret-empty" />}
          <span class="file-tree-icon">{getFileIcon(node.name, node.is_dir, node.expanded)}</span>
          <span class="file-tree-name">{node.name}</span>
        </div>
        {node.is_dir && node.expanded && node.children && (
          <div class="file-tree-children">
            {node.children.map((c) => renderNode(c, depth + 1))}
          </div>
        )}
      </div>
    );
  };

  return (
    <div class="file-explorer-root" ref={containerRef} tabIndex={0}>
      {/* 资源管理器内部顶部栏 */}
      <div class="file-explorer-header">
        <div class="file-explorer-title-row">
          <span class="file-explorer-title">
            {searchMode === 'search' ? t('explorer.search') : t('explorer.title')}
          </span>
          <div class="file-explorer-header-actions">
            <button
              type="button"
              class={'explorer-mode-btn' + (searchMode === 'tree' ? ' active' : '')}
              onClick={() => setSearchMode('tree')}
              title={t('explorer.files')}
            >
              📁
            </button>
            <button
              type="button"
              class={'explorer-mode-btn' + (searchMode === 'search' ? ' active' : '')}
              onClick={() => {
                setSearchMode('search');
                setTimeout(() => searchInputRef.current?.focus(), 50);
              }}
              title={t('explorer.searchReplaceTitle')}
            >
              🔍
            </button>
            <button
              type="button"
              class="explorer-mode-btn"
              onClick={() => {
                setCachedTree(rootPath, []);
                void saveCachedDirectoryTree(rootPath, []);
                void loadRoot(true);
              }}
              title={t('explorer.refreshTree')}
            >
              🔄
            </button>
          </div>
        </div>

        {/* 树视图下的文件名快速过滤与相对路径定位 */}
        {searchMode === 'tree' && (
          <div class="file-filter-box">
            <div class="search-input-wrapper">
              <input
                type="text"
                class="vscode-search-field"
                placeholder={t('explorer.filterPlaceholder')}
                value={filterQuery}
                onInput={(e) => setFilterQuery((e.target as HTMLInputElement).value)}
                onKeyDown={(e) => {
                  if (e.key === 'ArrowDown') {
                    e.preventDefault();
                    setSelectedLocateIdx((i) => Math.min(locateResults.length - 1, i + 1));
                  } else if (e.key === 'ArrowUp') {
                    e.preventDefault();
                    setSelectedLocateIdx((i) => Math.max(0, i - 1));
                  } else if (e.key === 'Enter') {
                    e.preventDefault();
                    if (locateResults[selectedLocateIdx]) {
                      void handleSelectLocateItem(locateResults[selectedLocateIdx]);
                    }
                  } else if (e.key === 'Escape') {
                    setFilterQuery('');
                  }
                }}
              />
              {filterQuery && (
                <div class="search-field-actions">
                  <button
                    type="button"
                    class="search-clear-btn"
                    onClick={() => setFilterQuery('')}
                    title={t('explorer.clearInput')}
                  >
                    ×
                  </button>
                </div>
              )}
            </div>
          </div>
        )}

        {/* VSCode 经典高保真全局搜索与替换面板 (对齐用户截图) */}
        {searchMode === 'search' && (
          <div class="vscode-search-panel-box">
            {/* 搜索行 */}
            <div class="search-input-wrapper">
              <input
                ref={searchInputRef}
                type="text"
                class="vscode-search-field"
                placeholder={t('explorer.searchContent')}
                value={searchQuery}
                onInput={(e) => setSearchQuery((e.target as HTMLInputElement).value)}
                onKeyDown={(e) => e.key === 'Enter' && void executeGlobalSearch()}
              />
              <div class="search-field-actions">
                {searchQuery && (
                  <button
                    type="button"
                    class="search-clear-btn"
                    onClick={() => {
                      setSearchQuery('');
                      setSearchResults([]);
                      searchInputRef.current?.focus();
                    }}
                    title={t('explorer.clearInput')}
                  >
                    ×
                  </button>
                )}
              </div>
            </div>
          </div>
        )}
      </div>

      {/* 核心内容区 */}
      <div class="file-explorer-body">
        {searchMode === 'tree' ? (
          filterQuery.trim() ? (
            locateLoading ? (
              <div class="file-tree-loading">
                <span class="git-spinner" />
                <span>{t('explorer.locating')}</span>
              </div>
            ) : locateResults.length > 0 ? (
              <div class="file-locate-list">
                <div class="file-locate-summary">
                  <span>{t('explorer.locateCount', { n: locateResults.length })}</span>
                </div>
                {locateResults.map((item, idx) => {
                  const isSelected = idx === selectedLocateIdx;
                  return (
                    <div
                      key={item.path}
                      class={'file-locate-row' + (isSelected ? ' selected' : '')}
                      onClick={() => void handleSelectLocateItem(item)}
                      title={t('explorer.locateItemTitle', { path: item.path })}
                    >
                      <span class="file-tree-icon">{getFileIcon(item.name, item.is_dir, false)}</span>
                      <span class="file-locate-name">{item.name}</span>
                      <span class="file-locate-relpath" title={item.rel_path}>{item.rel_path}</span>
                      <span class="file-locate-badge">{item.is_dir ? 'DIR' : 'FILE'}</span>
                    </div>
                  );
                })}
              </div>
            ) : (
              <div class="file-tree-empty">{t('explorer.noLocateMatches')}</div>
            )
          ) : loading ? (
            <div class="file-tree-loading">
              <span class="git-spinner" />
              <span>{t('explorer.loadingTree')}</span>
            </div>
          ) : (
            <div class="file-tree-list">
              {tree.map((n) => renderNode(n, 0))}
              {tree.length === 0 && <div class="file-tree-empty">{t('explorer.noFiles')}</div>}
            </div>
          )
        ) : (
          /* VSCode 经典树状搜索结果展示 (按文件折叠，带匹配总数徽标与行高亮) */
          <div class="vscode-search-results-tree">
            {searchLoading ? (
              <div class="file-tree-loading">
                <span class="git-spinner" />
                <span>{t('explorer.searchInProgress')}</span>
              </div>
            ) : searchResults.length > 0 ? (
              <>
                <div class="search-summary-bar">
                  <span>{t('explorer.searchResultCount', { files: groupedResults.length, matches: searchResults.length })}</span>
                  <a
                    class="search-summary-action"
                    onClick={() => {
                      if (searchResults[0]) onOpenFile?.(searchResults[0].file, searchResults[0].line);
                    }}
                  >
                    {t('explorer.openInEditor')}
                  </a>
                </div>

                <div class="search-groups-container">
                  {groupedResults.map((group) => (
                    <div key={group.file} class="search-file-group">
                      <div
                        class="search-file-group-header"
                        draggable={true}
                        onDragStart={(e) => {
                          const ext = group.fileName.split('.').pop() || 'FILE';
                          const data = {
                            id: `editor_${group.file}`,
                            filePath: group.file,
                            fileName: group.fileName,
                            fileStatus: ext.slice(0, 3).toUpperCase(),
                          };
                          (window as any).__jeikcode_dragged_tab = data;
                          e.dataTransfer?.setData('application/jeikcode-tab', JSON.stringify(data));
                          e.dataTransfer?.setData('text/plain', group.file);
                          if (e.dataTransfer) {
                            e.dataTransfer.effectAllowed = 'all';
                          }
                        }}
                        onDragEnd={() => {
                          (window as any).__jeikcode_dragged_tab = null;
                        }}
                        onClick={() => toggleGroupExpand(group.file)}
                      >
                        <span class="group-caret">{group.expanded ? '▼' : '▶'}</span>
                        <span class="group-file-icon">📄</span>
                        <span class="group-file-name" title={group.file}>{group.fileName}</span>
                        <span class="group-match-count">{group.items.length}</span>
                      </div>

                      {group.expanded && (
                        <div class="search-file-items">
                          {group.items.map((item, idx) => (
                            <div
                              key={idx}
                              class="search-match-line-row"
                              onClick={() => onOpenFile?.(item.file, item.line)}
                              title={`${item.file}:${item.line}`}
                            >
                              <span class="match-line-num">{item.line}</span>
                              <span class="match-line-text">{item.content}</span>
                            </div>
                          ))}
                        </div>
                      )}
                    </div>
                  ))}
                </div>
              </>
            ) : (
              <div class="file-tree-empty">
                {searchQuery ? t('explorer.noMatches') : t('explorer.searchPrompt')}
              </div>
            )}
          </div>
        )}
      </div>

      {/* 右键上下文菜单 (createPortal 挂在 document.body，防溢出且防缩放坐标漂移) */}
      {contextMenu && typeof document !== 'undefined' && createPortal(
        <div
          class="file-context-menu"
          style={{ top: `${contextMenu.y}px`, left: `${contextMenu.x}px` }}
          onClick={(e) => e.stopPropagation()}
        >
          <button
            type="button"
            class="context-menu-btn"
            onClick={async () => {
              const targetPath = contextMenu.node.path;
              setContextMenu(null);
              try {
                await revealInFileExplorer(targetPath);
                showToast(t('explorer.revealInOs'), 'success');
              } catch {
                // 系统外壳调用失败时静默忽略
              }
            }}
          >
            {t('explorer.revealInOs')}
          </button>
          <button
            type="button"
            class="context-menu-btn"
            onClick={() => {
              const normFull = contextMenu.node.path.replace(/\\/g, '/');
              const normBase = rootPath.replace(/\\/g, '/');
              let rel = normFull;
              if (normFull === normBase) {
                rel = '.';
              } else if (normFull.startsWith(normBase + '/')) {
                rel = normFull.slice(normBase.length + 1);
              } else if (normFull.startsWith(normBase)) {
                rel = normFull.slice(normBase.length).replace(/^\//, '');
              }
              navigator.clipboard?.writeText(rel);
              showToast(t('explorer.copyRelativePath'), 'success');
              setContextMenu(null);
            }}
          >
            {t('explorer.copyRelativePath')}
          </button>
          <button
            type="button"
            class="context-menu-btn"
            onClick={() => {
              navigator.clipboard?.writeText(contextMenu.node.path);
              showToast(t('explorer.copyFullPath'), 'success');
              setContextMenu(null);
            }}
          >
            {t('explorer.copyFullPath')}
          </button>
          <button
            type="button"
            class="context-menu-btn"
            onClick={() => {
              navigator.clipboard?.writeText(contextMenu.node.name);
              showToast(contextMenu.node.is_dir ? t('explorer.copyFolderName') : t('explorer.copyFileName'), 'success');
              setContextMenu(null);
            }}
          >
            {contextMenu.node.is_dir ? t('explorer.copyFolderName') : t('explorer.copyFileName')}
          </button>
          <button
            type="button"
            class="context-menu-btn"
            onClick={() => triggerRename(contextMenu.node)}
          >
            <span>{t('explorer.rename')}</span>
          </button>
          <div class="context-menu-divider" />
          <button
            type="button"
            class="context-menu-btn context-menu-danger"
            onClick={() => triggerDeleteConfirm(contextMenu.targets || [contextMenu.node.path])}
          >
            <span>{contextMenu.targets && contextMenu.targets.length > 1 ? t('explorer.deleteCount', { n: String(contextMenu.targets.length) }) : t('explorer.delete')}</span>
          </button>
        </div>,
        document.body,
      )}

      {/* 重命名弹窗 */}
      {renameModal.open && renameModal.node && (
        <ConfirmDialog
          title={renameModal.node.is_dir ? t('explorer.renameFolderTitle') : t('explorer.renameFileTitle')}
          body={t('explorer.renamePrompt')}
          icon="✏️"
          confirmLabel={t('common.confirm')}
          cancelLabel={t('common.cancel')}
          danger={false}
          input={{
            defaultValue: renameModal.currentName,
            placeholder: t('explorer.renamePlaceholder'),
          }}
          onConfirm={(val) => {
            if (renameModal.node) {
              void executeRename(renameModal.node, val || '');
            }
          }}
          onClose={() => setRenameModal({ open: false, node: null, currentName: '' })}
        />
      )}

      {/* 删除确认弹窗 */}
      {deleteModal.open && (
        <ConfirmDialog
          title={t('explorer.deleteConfirmTitle')}
          body={
            deleteModal.paths.length > 1
              ? t('explorer.deleteMultiConfirm', { n: String(deleteModal.paths.length) })
              : t('explorer.deleteSingleConfirm', { name: deleteModal.paths[0]?.split(/[\\/]/).pop() || deleteModal.paths[0] })
          }
          confirmLabel={t('explorer.delete')}
          cancelLabel={t('common.cancel')}
          danger={true}
          onConfirm={() => executeDelete(deleteModal.paths)}
          onClose={() => setDeleteModal({ open: false, paths: [] })}
        />
      )}
    </div>
  );
}
