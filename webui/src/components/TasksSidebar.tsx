import { useCallback, useEffect, useRef, useState } from 'preact/hooks';
import {
  getSchedules,
  ScheduleTaskItem,
  syncSchedules,
  toggleSchedule,
} from '../api';
import { onGlobalEvent } from '../lib/globalEvents';
import { useT } from '../settings';
import { formatScheduleLabel } from './TasksPanel';
import { showToast } from './Toast';

interface TasksSidebarProps {
  selectedTaskId?: string | null;
  onSelectTask: (taskId: string) => void;
  onNewTask: () => void;
}

export function TasksSidebar({
  selectedTaskId,
  onSelectTask,
  onNewTask,
}: TasksSidebarProps) {
  const t = useT();
  const [tasks, setTasks] = useState<ScheduleTaskItem[]>([]);
  const [loading, setLoading] = useState(true);
  const [syncing, setSyncing] = useState(false);
  const reqSeqRef = useRef(0);

  const load = useCallback(async (silent = false) => {
    const seq = ++reqSeqRef.current;
    if (!silent) setLoading(true);
    try {
      const resp = await getSchedules();
      if (seq !== reqSeqRef.current) return;
      setTasks(resp.tasks);
    } catch {
      /* errors surface in main TasksPanel */
    } finally {
      if (seq === reqSeqRef.current) setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
    return onGlobalEvent((event) => {
      if (event.type === 'schedules_updated' || event.type === 'sync') {
        void load(true);
      }
    });
  }, [load]);

  const handleSync = async () => {
    setSyncing(true);
    try {
      const res = await syncSchedules();
      showToast(
        t('tasks.syncSuccess', { installed: res.installed, uninstalled: res.uninstalled }),
        res.error_count > 0 ? 'info' : 'success',
      );
      await load(true);
    } catch (err: any) {
      showToast(t('tasks.syncFailed', { error: err?.message || err }), 'error');
    } finally {
      setSyncing(false);
    }
  };

  const handleToggle = async (e: MouseEvent, task: ScheduleTaskItem) => {
    e.stopPropagation();
    try {
      await toggleSchedule(task.id, !task.enabled);
      await load(true);
    } catch (err: any) {
      showToast(t('tasks.toggleFailed', { error: err?.message || err }), 'error');
    }
  };

  return (
    <div class="tasks-sidebar-root">
      <div class="sidebar-actions">
        <button type="button" class="sidebar-action" onClick={onNewTask}>
          <span class="sidebar-action-icon">+</span>
          <span class="sidebar-action-label">{t('tasks.newTask')}</span>
        </button>
        <button
          type="button"
          class="sidebar-action"
          onClick={() => void handleSync()}
          disabled={syncing}
        >
          <span class="sidebar-action-icon">↻</span>
          <span class="sidebar-action-label">{syncing ? '…' : t('tasks.syncOs')}</span>
        </button>
      </div>

      <div class="tasks-sidebar-list">
        {loading ? (
          <div class="tasks-sidebar-empty">{t('sidebar.skillsLoading')}</div>
        ) : tasks.length === 0 ? (
          <div class="tasks-sidebar-empty">{t('tasks.emptyTitle')}</div>
        ) : (
          tasks.map((task) => {
            const active = selectedTaskId === task.id;
            return (
              <div
                key={task.id}
                class={
                  'tasks-sidebar-item' +
                  (active ? ' active' : '') +
                  (!task.enabled ? ' is-disabled' : '')
                }
                onClick={() => onSelectTask(task.id)}
                role="button"
                tabIndex={0}
                onKeyDown={(e) => {
                  if (e.key === 'Enter' || e.key === ' ') {
                    e.preventDefault();
                    onSelectTask(task.id);
                  }
                }}
              >
                <div class="tasks-sidebar-item-top">
                  <span
                    class={
                      'tasks-sidebar-dot ' +
                      (task.is_running
                        ? 'running'
                        : !task.enabled
                          ? 'paused'
                          : task.last_status === 'error' || task.os_status === 'missing'
                            ? 'warn'
                            : 'ok')
                    }
                  />
                  <span class="tasks-sidebar-item-title">{task.title}</span>
                  <button
                    type="button"
                    class={'tasks-sidebar-toggle' + (task.enabled ? ' on' : '')}
                    onClick={(e) => void handleToggle(e as unknown as MouseEvent, task)}
                    title={task.enabled ? t('tasks.disabled') : t('tasks.enabled')}
                    aria-label={task.enabled ? t('tasks.disabled') : t('tasks.enabled')}
                  >
                    {task.enabled ? t('tasks.toggleOn') : t('tasks.toggleOff')}
                  </button>
                </div>
                <div class="tasks-sidebar-item-meta">
                  <span>{formatScheduleLabel(task.schedule, t)}</span>
                </div>
              </div>
            );
          })
        )}
      </div>
    </div>
  );
}
