import { useCallback, useEffect, useRef, useState } from 'preact/hooks';
import {
  clearScheduleRuns,
  createSchedule,
  deleteSchedule,
  deleteSession,
  DeleteSessionError,
  getScheduleRuns,
  getSchedules,
  runScheduleNow,
  ScheduleNotifyLevel,
  SchedulePermissionMode,
  SchedulesListResponse,
  ScheduleSpec,
  ScheduleTaskItem,
  SessionMetaWithProject,
  syncSchedules,
  toggleSchedule,
  updateSchedule,
} from '../api';
import { onGlobalEvent } from '../lib/globalEvents';
import { keyboardActivateProps } from '../lib/modalA11y';
import { deleteSessionCache } from '../lib/sessionCache';
import { useT } from '../settings';
import { ConfirmDialog } from './ConfirmDialog';
import { CwdPicker } from './CwdPicker';
import { showToast } from './Toast';

interface TasksPanelProps {
  cwd: string;
  selectedTaskId: string | null;
  onSelectTaskId: (id: string | null) => void;
  creatingNew: boolean;
  onCreatingChange: (creating: boolean) => void;
  onSelectSession: (session: SessionMetaWithProject) => void;
  onSessionDeleted: (id: string) => void;
  onClose: () => void;
}

type FreqMode = 'interval' | 'hourly' | 'daily' | 'weekly' | 'cron';
type IntervalUnit = 'minutes' | 'hours';

export function formatScheduleLabel(
  sched: ScheduleSpec,
  t: (k: any, vars?: Record<string, string | number>) => string,
): string {
  switch (sched.kind) {
    case 'interval': {
      if (sched.every_minutes >= 60 && sched.every_minutes % 60 === 0) {
        return t('tasks.everyNHoursLabel', { n: sched.every_minutes / 60 });
      }
      return t('tasks.everyNMinutesLabel', { n: sched.every_minutes });
    }
    case 'hourly':
      return t('tasks.hourlyLabel');
    case 'daily':
      return t('tasks.dailyLabel', { time: sched.time });
    case 'weekly': {
      const dayKey = `tasks.weekday${Math.min(7, Math.max(1, sched.weekday))}`;
      return t('tasks.weeklyLabel', { day: t(dayKey), time: sched.time });
    }
    case 'cron':
      return sched.expr;
  }
}

function formatTimestamp(epochSecs?: number | null, fallback = '-'): string {
  if (!epochSecs || !Number.isFinite(epochSecs)) return fallback;
  try {
    return new Date(epochSecs * 1000).toLocaleString();
  } catch {
    return fallback;
  }
}

export function TasksPanel({
  cwd,
  selectedTaskId,
  onSelectTaskId,
  creatingNew,
  onCreatingChange,
  onSelectSession,
  onSessionDeleted,
  onClose,
}: TasksPanelProps) {
  const t = useT();
  const [data, setData] = useState<SchedulesListResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [syncing, setSyncing] = useState(false);
  const [runningId, setRunningId] = useState<string | null>(null);
  const [osWarning, setOsWarning] = useState<string | null>(null);
  const [formError, setFormError] = useState<string | null>(null);
  const [showCwdPicker, setShowCwdPicker] = useState(false);
  const [deleteTarget, setDeleteTarget] = useState<ScheduleTaskItem | null>(null);
  const [deleteRunTarget, setDeleteRunTarget] = useState<SessionMetaWithProject | null>(null);
  const [clearRunsOpen, setClearRunsOpen] = useState(false);
  const [clearingRuns, setClearingRuns] = useState(false);

  // Monotonic request guards: prevent slow loads from overwriting newer SSE updates,
  // and prevent in-flight save/toggle from writing errors/selection into a switched task.
  const schedulesReqSeqRef = useRef(0);
  const formContextSeqRef = useRef(0);

  // Run history for selected task
  const [runs, setRuns] = useState<SessionMetaWithProject[]>([]);
  const [runsLoading, setRunsLoading] = useState(false);

  // Form state
  const [title, setTitle] = useState('');
  const [prompt, setPrompt] = useState('');
  const [taskCwd, setTaskCwd] = useState(cwd || '');
  const [freqMode, setFreqMode] = useState<FreqMode>('interval');
  const [intervalValue, setIntervalValue] = useState(4);
  const [intervalUnit, setIntervalUnit] = useState<IntervalUnit>('hours');
  const [timeOfDay, setTimeOfDay] = useState('09:00');
  const [weekday, setWeekday] = useState(1);
  const [cronExpr, setCronExpr] = useState('');
  const [permissionMode, setPermissionMode] = useState<SchedulePermissionMode>('plan');
  const [notify, setNotify] = useState<ScheduleNotifyLevel>('important');
  const [enabled, setEnabled] = useState(true);

  const loadSchedules = useCallback(
    async (silent = false) => {
      const seq = ++schedulesReqSeqRef.current;
      if (!silent) setLoading(true);
      try {
        const resp = await getSchedules();
        if (seq !== schedulesReqSeqRef.current) return;
        setData(resp);
      } catch (err: any) {
        if (seq !== schedulesReqSeqRef.current) return;
        if (!silent) {
          showToast(
            t('tasks.loadFailed', { error: err?.message || t('tasks.unknownError') }),
            'error',
          );
        }
      } finally {
        if (seq === schedulesReqSeqRef.current) setLoading(false);
      }
    },
    [t],
  );

  useEffect(() => {
    void loadSchedules();
    return onGlobalEvent((event) => {
      if (event.type === 'schedules_updated' || event.type === 'sync') {
        void loadSchedules(true);
      }
    });
  }, [loadSchedules]);

  const tasks = data?.tasks ?? [];
  const selectedTask = !creatingNew
    ? tasks.find((item) => item.id === selectedTaskId) ?? null
    : null;

  // Populate form when switching between creatingNew and selectedTask
  useEffect(() => {
    formContextSeqRef.current += 1;
    setFormError(null);
    if (creatingNew) {
      setOsWarning(null);
      setTitle('');
      setPrompt('');
      setTaskCwd(cwd || '');
      setFreqMode('interval');
      setIntervalValue(4);
      setIntervalUnit('hours');
      setTimeOfDay('09:00');
      setWeekday(1);
      setCronExpr('');
      setPermissionMode('plan');
      setNotify('important');
      setEnabled(true);
      return;
    }
    if (!selectedTask) return;
    setTitle(selectedTask.title);
    setPrompt(selectedTask.prompt);
    setTaskCwd(selectedTask.cwd);
    setPermissionMode(selectedTask.permission_mode || 'plan');
    setNotify(selectedTask.notify || 'important');
    setEnabled(selectedTask.enabled);
    const s = selectedTask.schedule;
    if (s.kind === 'interval') {
      setFreqMode('interval');
      if (s.every_minutes >= 60 && s.every_minutes % 60 === 0) {
        setIntervalUnit('hours');
        setIntervalValue(s.every_minutes / 60);
      } else {
        setIntervalUnit('minutes');
        setIntervalValue(s.every_minutes);
      }
    } else if (s.kind === 'hourly') {
      setFreqMode('hourly');
    } else if (s.kind === 'daily') {
      setFreqMode('daily');
      setTimeOfDay(s.time);
    } else if (s.kind === 'weekly') {
      setFreqMode('weekly');
      setWeekday(s.weekday);
      setTimeOfDay(s.time);
    } else if (s.kind === 'cron') {
      setFreqMode('cron');
      setCronExpr(s.expr);
    }
  }, [creatingNew, selectedTask?.id]);

  // Keep enabled checkbox synchronized when toggled externally (sidebar/header/SSE)
  // without resetting other unsaved form fields.
  useEffect(() => {
    if (!creatingNew && selectedTask) {
      setEnabled(selectedTask.enabled);
    }
  }, [creatingNew, selectedTask?.id, selectedTask?.enabled]);

  // Load run history for selected task (guarded by monotonic seq to discard stale replies)
  const runsReqSeqRef = useRef(0);
  const loadRuns = useCallback(async (id: string, silent = false) => {
    const reqSeq = ++runsReqSeqRef.current;
    if (!silent) setRunsLoading(true);
    try {
      const list = await getScheduleRuns(id);
      if (runsReqSeqRef.current !== reqSeq) return;
      setRuns(list);
    } catch {
      if (runsReqSeqRef.current !== reqSeq) return;
      if (!silent) setRuns([]);
    } finally {
      if (runsReqSeqRef.current === reqSeq) setRunsLoading(false);
    }
  }, []);

  useEffect(() => {
    if (!selectedTask || creatingNew) {
      runsReqSeqRef.current += 1;
      setRuns([]);
      setRunsLoading(false);
      return;
    }
    const id = selectedTask.id;
    void loadRuns(id);
    const refreshSilent = () => {
      void loadSchedules(true);
      void loadRuns(id, true);
    };
    const onFocus = () => refreshSilent();
    const onVisibility = () => {
      if (!document.hidden) refreshSilent();
    };
    window.addEventListener('focus', onFocus);
    document.addEventListener('visibilitychange', onVisibility);
    const timer = window.setInterval(refreshSilent, 15000);
    const unsubscribe = onGlobalEvent((event) => {
      if (event.type === 'schedules_updated') {
        void loadRuns(id, true);
      } else if (event.type === 'session_deleted') {
        void deleteSessionCache(event.project_hash, event.session_id);
        setRuns((prev) => prev.filter((r) => r.id !== event.session_id));
      }
    });
    return () => {
      runsReqSeqRef.current += 1;
      window.removeEventListener('focus', onFocus);
      document.removeEventListener('visibilitychange', onVisibility);
      window.clearInterval(timer);
      unsubscribe();
    };
  }, [selectedTask?.id, creatingNew, loadRuns, loadSchedules]);

  const buildScheduleSpec = (): ScheduleSpec => {
    if (freqMode === 'interval') {
      const n = Math.max(1, Math.floor(Number(intervalValue) || 1));
      const every_minutes = intervalUnit === 'hours' ? n * 60 : n;
      return { kind: 'interval', every_minutes };
    }
    if (freqMode === 'hourly') {
      return { kind: 'hourly' };
    }
    if (freqMode === 'daily') {
      return { kind: 'daily', time: timeOfDay || '09:00' };
    }
    if (freqMode === 'weekly') {
      return { kind: 'weekly', weekday, time: timeOfDay || '09:00' };
    }
    return { kind: 'cron', expr: cronExpr };
  };

  const handleSave = async (e: Event) => {
    e.preventDefault();
    const formSeq = formContextSeqRef.current;
    setFormError(null);
    setOsWarning(null);
    const schedule = buildScheduleSpec();
    setSaving(true);
    try {
      if (creatingNew) {
        const res = await createSchedule({
          title,
          prompt,
          cwd: taskCwd,
          schedule,
          permission_mode: permissionMode,
          notify,
          enabled,
        });
        showToast(t('tasks.createdToast'), 'success');
        if (formSeq === formContextSeqRef.current) {
          if (res.os_warning) setOsWarning(res.os_warning);
          onCreatingChange(false);
          if (res.task?.id) onSelectTaskId(res.task.id);
        }
        await loadSchedules(true);
      } else if (selectedTask) {
        const res = await updateSchedule(selectedTask.id, {
          title,
          prompt,
          cwd: taskCwd,
          schedule,
          permission_mode: permissionMode,
          notify,
          enabled,
        });
        showToast(t('tasks.savedToast'), 'success');
        if (formSeq === formContextSeqRef.current && res.os_warning) {
          setOsWarning(res.os_warning);
        }
        await loadSchedules(true);
      }
    } catch (err: any) {
      const msg = err?.message || t('tasks.unknownError');
      if (formSeq === formContextSeqRef.current) {
        setFormError(msg);
      }
      showToast(t('tasks.saveFailed', { error: msg }), 'error');
    } finally {
      setSaving(false);
    }
  };

  const handleSyncOs = async () => {
    setSyncing(true);
    try {
      const res = await syncSchedules();
      showToast(
        t('tasks.syncSuccess', { installed: res.installed, uninstalled: res.uninstalled }),
        res.error_count > 0 ? 'info' : 'success',
      );
      if (res.error_count > 0 && res.errors.length > 0) {
        setOsWarning(res.errors.join('\n'));
      } else {
        setOsWarning(null);
      }
      await loadSchedules(true);
    } catch (err: any) {
      showToast(t('tasks.syncFailed', { error: err?.message || err }), 'error');
    } finally {
      setSyncing(false);
    }
  };

  const handleRunNow = async (task: ScheduleTaskItem) => {
    setRunningId(task.id);
    try {
      await runScheduleNow(task.id);
      showToast(t('tasks.startedToast'), 'success');
      await loadSchedules(true);
    } catch (err: any) {
      showToast(t('tasks.runFailed', { error: err?.message || err }), 'error');
    } finally {
      setRunningId(null);
    }
  };

  const handleToggle = async (task: ScheduleTaskItem) => {
    const formSeq = formContextSeqRef.current;
    try {
      const res = await toggleSchedule(task.id, !task.enabled);
      if (formSeq === formContextSeqRef.current && selectedTask?.id === task.id && res.os_warning) {
        setOsWarning(res.os_warning);
      }
      await loadSchedules(true);
    } catch (err: any) {
      showToast(t('tasks.toggleFailed', { error: err?.message || err }), 'error');
    }
  };

  const platformLabel = (() => {
    switch (data?.os_platform) {
      case 'windows':
        return t('tasks.osPlatformWindows');
      case 'linux':
        return t('tasks.osPlatformLinux');
      case 'macos':
        return t('tasks.osPlatformMacos');
      default:
        return t('tasks.osPlatformOther');
    }
  })();

  const everyMinutesPreview =
    freqMode === 'interval'
      ? (Math.max(1, Math.floor(Number(intervalValue) || 1)) * (intervalUnit === 'hours' ? 60 : 1))
      : 0;

  return (
    <div class="tasks-panel-root">
      <div class="tasks-panel-header">
        <div class="tasks-panel-header-left">
          <h2 class="tasks-panel-title">{t('tasks.title')}</h2>
          <p class="tasks-panel-subtitle">{t('tasks.subtitle')}</p>
        </div>
        <div class="tasks-panel-header-actions">
          {data && <span class="tasks-platform-badge">{platformLabel}</span>}
          <button
            type="button"
            class="tasks-btn tasks-btn-secondary"
            onClick={handleSyncOs}
            disabled={syncing}
          >
            {syncing ? '…' : t('tasks.syncOs')}
          </button>
          <button
            type="button"
            class="tasks-btn tasks-btn-primary"
            onClick={() => onCreatingChange(true)}
          >
            + {t('tasks.newTask')}
          </button>
          <button
            type="button"
            class="tasks-btn tasks-btn-ghost"
            onClick={onClose}
            title={t('tasks.backToChat')}
          >
            {t('tasks.backToChat')}
          </button>
        </div>
      </div>

      <div class="tasks-panel-body">
        {loading ? (
          <div class="tasks-empty-state">{t('sidebar.skillsLoading')}</div>
        ) : !creatingNew && !selectedTask ? (
          tasks.length === 0 ? (
            <div class="tasks-empty-state">
              <div class="tasks-empty-icon">⏱</div>
              <h3>{t('tasks.emptyTitle')}</h3>
              <p>{t('tasks.emptyDesc')}</p>
              <button
                type="button"
                class="tasks-btn tasks-btn-primary"
                onClick={() => onCreatingChange(true)}
              >
                + {t('tasks.newTask')}
              </button>
            </div>
          ) : (
            <div class="tasks-overview-wrap">
              <p class="tasks-overview-hint">{t('tasks.selectOrCreate')}</p>
              <div class="tasks-overview-grid">
                {tasks.map((task) => (
                  <div
                    key={task.id}
                    class={'tasks-overview-card' + (!task.enabled ? ' is-disabled' : '')}
                    {...keyboardActivateProps(() => {
                      onCreatingChange(false);
                      onSelectTaskId(task.id);
                    })}
                    onClick={() => {
                      onCreatingChange(false);
                      onSelectTaskId(task.id);
                    }}
                  >
                    <div class="tasks-card-top">
                      <span class="tasks-card-title">{task.title}</span>
                      <span class={'tasks-status-pill ' + (task.is_running ? 'running' : task.enabled ? 'enabled' : 'paused')}>
                        {task.is_running ? t('tasks.running') : task.enabled ? t('tasks.enabled') : t('tasks.disabled')}
                      </span>
                    </div>
                    <div class="tasks-card-meta">
                      <span>{formatScheduleLabel(task.schedule, t)}</span>
                      {task.next_run_at && task.enabled && (
                        <span> · {t('tasks.nextRun')}: {formatTimestamp(task.next_run_at)}</span>
                      )}
                    </div>
                  </div>
                ))}
              </div>
            </div>
          )
        ) : (
          <div class="tasks-detail-layout">
            <form class="tasks-form-card" onSubmit={handleSave}>
              <div class="tasks-form-head">
                <h3>{creatingNew ? t('tasks.newTask') : t('tasks.editTask')}</h3>
                {!creatingNew && selectedTask && (
                  <div class="tasks-form-head-actions">
                    <span
                      class={
                        'tasks-os-pill ' +
                        (selectedTask.os_status === 'registered'
                          ? 'ok'
                          : selectedTask.os_status === 'missing'
                            ? 'warn'
                            : 'muted')
                      }
                    >
                      {selectedTask.os_status === 'registered'
                        ? t('tasks.osRegistered')
                        : selectedTask.os_status === 'missing'
                          ? t('tasks.osMissing')
                          : t('tasks.osUnknown')}
                    </span>
                    <button
                      type="button"
                      class="tasks-btn tasks-btn-secondary"
                      disabled={!selectedTask.enabled || selectedTask.is_running || runningId === selectedTask.id}
                      onClick={() => void handleRunNow(selectedTask)}
                    >
                      {selectedTask.is_running || runningId === selectedTask.id
                        ? t('tasks.running')
                        : t('tasks.runNow')}
                    </button>
                    <button
                      type="button"
                      class="tasks-btn tasks-btn-secondary"
                      onClick={() => void handleToggle(selectedTask)}
                    >
                      {selectedTask.enabled ? t('tasks.disabled') : t('tasks.enabled')}
                    </button>
                    <button
                      type="button"
                      class="tasks-btn tasks-btn-danger"
                      onClick={() => setDeleteTarget(selectedTask)}
                    >
                      {t('tasks.delete')}
                    </button>
                  </div>
                )}
              </div>

              {selectedTask && selectedTask.enabled && selectedTask.os_status === 'missing' && (
                <div class="tasks-banner tasks-banner-warn">{t('tasks.osMissingWarning')}</div>
              )}
              {osWarning && <div class="tasks-banner tasks-banner-warn">{osWarning}</div>}
              {formError && <div class="tasks-banner tasks-banner-error">{formError}</div>}

              <div class="tasks-field">
                <label>{t('tasks.fieldTitle')}</label>
                <input
                  type="text"
                  value={title}
                  onInput={(e) => setTitle((e.target as HTMLInputElement).value)}
                  placeholder={t('tasks.fieldTitlePlaceholder')}
                  required
                />
              </div>

              <div class="tasks-field">
                <label>{t('tasks.fieldPrompt')}</label>
                <textarea
                  rows={4}
                  value={prompt}
                  onInput={(e) => setPrompt((e.target as HTMLTextAreaElement).value)}
                  placeholder={t('tasks.fieldPromptPlaceholder')}
                  required
                />
              </div>

              <div class="tasks-field">
                <label>{t('tasks.fieldCwd')}</label>
                <div class="tasks-cwd-row">
                  <input
                    type="text"
                    value={taskCwd}
                    onInput={(e) => setTaskCwd((e.target as HTMLInputElement).value)}
                    required
                  />
                  <button
                    type="button"
                    class="tasks-btn tasks-btn-secondary"
                    onClick={() => setShowCwdPicker(true)}
                  >
                    {t('tasks.browseDir')}
                  </button>
                </div>
              </div>

              <div class="tasks-field-grid">
                <div class="tasks-field">
                  <label>{t('tasks.fieldFrequency')}</label>
                  <select
                    value={freqMode}
                    onChange={(e) => setFreqMode((e.target as HTMLSelectElement).value as FreqMode)}
                  >
                    <option value="interval">{t('tasks.freqEveryMinutes')}</option>
                    <option value="hourly">{t('tasks.freqHourly')}</option>
                    <option value="daily">{t('tasks.freqDaily')}</option>
                    <option value="weekly">{t('tasks.freqWeekly')}</option>
                    {freqMode === 'cron' && <option value="cron">{t('tasks.freqCron')}</option>}
                  </select>
                </div>

                {freqMode === 'interval' && (
                  <div class="tasks-field">
                    <label>{t('tasks.intervalValue')}</label>
                    <div class="tasks-inline-pair">
                      <input
                        type="number"
                        min={1}
                        max={
                          intervalUnit === 'hours'
                            ? Math.max(1, Math.floor((data?.max_interval_minutes ?? 10080) / 60))
                            : (data?.max_interval_minutes ?? 10080)
                        }
                        value={intervalValue}
                        onInput={(e) => setIntervalValue(Number((e.target as HTMLInputElement).value))}
                      />
                      <select
                        value={intervalUnit}
                        onChange={(e) => setIntervalUnit((e.target as HTMLSelectElement).value as IntervalUnit)}
                      >
                        <option value="minutes">{t('tasks.unitMinutes')}</option>
                        <option value="hours">{t('tasks.unitHours')}</option>
                      </select>
                    </div>
                  </div>
                )}

                {(freqMode === 'daily' || freqMode === 'weekly') && (
                  <div class="tasks-field">
                    <label>{t('tasks.timeOfDay')}</label>
                    <input
                      type="time"
                      value={timeOfDay}
                      onInput={(e) => setTimeOfDay((e.target as HTMLInputElement).value)}
                      required
                    />
                  </div>
                )}

                {freqMode === 'weekly' && (
                  <div class="tasks-field">
                    <label>{t('tasks.weekday')}</label>
                    <select
                      value={weekday}
                      onChange={(e) => setWeekday(Number((e.target as HTMLSelectElement).value))}
                    >
                      {[1, 2, 3, 4, 5, 6, 7].map((d) => (
                        <option key={d} value={d}>{t(`tasks.weekday${d}` as any)}</option>
                      ))}
                    </select>
                  </div>
                )}
              </div>

              {freqMode === 'cron' && (
                <div class="tasks-banner tasks-banner-warn">
                  <code>{cronExpr}</code> — {t('tasks.cronReadOnlyHint')}
                </div>
              )}

              {data?.max_interval_minutes && freqMode === 'interval' && everyMinutesPreview > data.max_interval_minutes && (
                <div class="tasks-banner tasks-banner-error">
                  {t('tasks.windowsMaxIntervalHint')}
                </div>
              )}

              <div class="tasks-field-grid">
                <div class="tasks-field">
                  <label>{t('tasks.fieldPermission')}</label>
                  <select
                    value={permissionMode}
                    onChange={(e) =>
                      setPermissionMode((e.target as HTMLSelectElement).value as SchedulePermissionMode)
                    }
                  >
                    <option value="plan">{t('tasks.permPlan')}</option>
                    <option value="accept_edits">{t('tasks.permAcceptEdits')}</option>
                    <option value="auto">{t('tasks.permAuto')}</option>
                  </select>
                  <span class="tasks-field-hint">{t('tasks.unattendedSafetyNote')}</span>
                </div>

                <div class="tasks-field">
                  <label>{t('tasks.fieldNotify')}</label>
                  <select
                    value={notify}
                    onChange={(e) => setNotify((e.target as HTMLSelectElement).value as ScheduleNotifyLevel)}
                  >
                    <option value="off">{t('tasks.notifyOff')}</option>
                    <option value="important">{t('tasks.notifyImportant')}</option>
                    <option value="all">{t('tasks.notifyAll')}</option>
                  </select>
                </div>
              </div>

              <div class="tasks-form-footer">
                <label class="tasks-checkbox-label">
                  <input
                    type="checkbox"
                    checked={enabled}
                    onChange={(e) => setEnabled((e.target as HTMLInputElement).checked)}
                  />
                  <span>{enabled ? t('tasks.enabled') : t('tasks.disabled')}</span>
                </label>
                <div class="tasks-form-footer-btns">
                  {creatingNew && (
                    <button
                      type="button"
                      class="tasks-btn tasks-btn-secondary"
                      onClick={() => onCreatingChange(false)}
                    >
                      {t('tasks.cancel')}
                    </button>
                  )}
                  <button type="submit" class="tasks-btn tasks-btn-primary" disabled={saving}>
                    {saving ? '…' : creatingNew ? t('tasks.create') : t('tasks.save')}
                  </button>
                </div>
              </div>
            </form>

            {!creatingNew && selectedTask && (
              <div class="tasks-runs-card">
                <div class="tasks-runs-head">
                  <h3>{t('tasks.runsTitle')}</h3>
                  <div class="tasks-runs-head-actions">
                    <div class="tasks-runs-stats">
                      <span>
                        {t('tasks.lastRun')}:{' '}
                        {selectedTask.last_run_at
                          ? formatTimestamp(selectedTask.last_run_at)
                          : t('tasks.neverRun')}
                      </span>
                      {selectedTask.last_status && (
                        <span class={'tasks-run-status ' + selectedTask.last_status}>
                          {selectedTask.last_status === 'ok'
                            ? t('tasks.statusOk')
                            : selectedTask.last_status === 'cancelled'
                              ? t('tasks.statusCancelled')
                              : t('tasks.statusError')}
                        </span>
                      )}
                      {selectedTask.next_run_at && selectedTask.enabled && (
                        <span>
                          · {t('tasks.nextRun')}: {formatTimestamp(selectedTask.next_run_at)}
                        </span>
                      )}
                    </div>
                    {runs.length > 0 && (
                      <button
                        type="button"
                        class="tasks-btn tasks-btn-danger-ghost tasks-btn-sm"
                        disabled={clearingRuns}
                        onClick={() => setClearRunsOpen(true)}
                      >
                        {t('tasks.clearRuns')}
                      </button>
                    )}
                  </div>
                </div>

                {runsLoading ? (
                  <div class="tasks-runs-empty">{t('tasks.runsLoading')}</div>
                ) : runs.length === 0 ? (
                  <div class="tasks-runs-empty">{t('tasks.runsEmpty')}</div>
                ) : (
                  <div class="tasks-runs-list">
                    {runs.map((run) => (
                      <div key={run.id} class="tasks-run-row">
                        <button
                          type="button"
                          class="tasks-run-row-open"
                          onClick={() => onSelectSession(run)}
                          title={t('tasks.openRunSession')}
                        >
                          <div class="tasks-run-row-main">
                            <span class="tasks-run-name">{run.name}</span>
                            <span class="tasks-run-time">{formatTimestamp(run.updated_at)}</span>
                          </div>
                          <div class="tasks-run-row-meta">
                            <span>{t('tasks.runsMessages', { n: run.message_count })}</span>
                            <span class="tasks-run-open">{t('tasks.openRunSession')} →</span>
                          </div>
                        </button>
                        <button
                          type="button"
                          class="tasks-run-delete-btn"
                          title={t('tasks.deleteRun')}
                          aria-label={t('tasks.deleteRun')}
                          onClick={() => setDeleteRunTarget(run)}
                        >
                          🗑
                        </button>
                      </div>
                    ))}
                  </div>
                )}
              </div>
            )}
          </div>
        )}
      </div>

      {showCwdPicker && (
        <CwdPicker
          current={taskCwd || cwd}
          onPick={(picked) => {
            setTaskCwd(picked);
            setShowCwdPicker(false);
          }}
          onClose={() => setShowCwdPicker(false)}
        />
      )}

      {deleteTarget && (
        <ConfirmDialog
          title={t('tasks.deleteTitle')}
          body={t('tasks.deleteConfirm', { title: deleteTarget.title })}
          confirmLabel={t('tasks.delete')}
          danger={true}
          onConfirm={async () => {
            try {
              await deleteSchedule(deleteTarget.id);
              showToast(t('tasks.deletedToast'), 'success');
              if (selectedTaskId === deleteTarget.id) {
                onSelectTaskId(null);
              }
              setDeleteTarget(null);
              await loadSchedules(true);
            } catch (err: any) {
              showToast(t('tasks.deleteFailed', { error: err?.message || err }), 'error');
              throw err;
            }
          }}
          onClose={() => setDeleteTarget(null)}
        />
      )}

      {deleteRunTarget && (
        <ConfirmDialog
          title={t('tasks.deleteRunTitle')}
          body={t('tasks.deleteRunConfirm', { name: deleteRunTarget.name })}
          confirmLabel={t('tasks.delete')}
          danger={true}
          onConfirm={async () => {
            const target = deleteRunTarget;
            try {
              try {
                await deleteSession(target.project_hash, target.id);
              } catch (err: any) {
                if (!(err instanceof DeleteSessionError && err.code === 'SESSION_NOT_FOUND')) {
                  throw err;
                }
              }
              void deleteSessionCache(target.project_hash, target.id);
              onSessionDeleted(target.id);
              setRuns((prev) => prev.filter((r) => r.id !== target.id));
              setDeleteRunTarget(null);
              showToast(t('tasks.runDeletedToast'), 'success');
            } catch (err: any) {
              const msg = t('tasks.deleteRunFailed', { error: err?.message || err });
              showToast(msg, 'error');
              throw new Error(msg);
            }
          }}
          onClose={() => setDeleteRunTarget(null)}
        />
      )}

      {clearRunsOpen && selectedTask && (
        <ConfirmDialog
          title={t('tasks.clearRunsTitle')}
          body={t('tasks.clearRunsConfirm', { title: selectedTask.title })}
          confirmLabel={t('tasks.clearRuns')}
          danger={true}
          onConfirm={async () => {
            const taskId = selectedTask.id;
            const priorRuns = runs.slice();
            setClearingRuns(true);
            try {
              const res = await clearScheduleRuns(taskId);
              if (res.deleted === 0 && res.failed > 0 && res.skipped_active === 0) {
                const msg = t('tasks.clearRunsFailed');
                showToast(msg, 'error');
                throw new Error(msg);
              }
              let remaining: SessionMetaWithProject[] | null = null;
              try {
                remaining = await getScheduleRuns(taskId);
              } catch {
                remaining = null;
              }
              if (remaining !== null) {
                const remainingIds = new Set(remaining.map((r) => r.id));
                for (const r of priorRuns) {
                  if (!remainingIds.has(r.id)) {
                    void deleteSessionCache(r.project_hash, r.id);
                    onSessionDeleted(r.id);
                  }
                }
                setRuns(remaining);
              } else if (res.skipped_active === 0 && res.failed === 0) {
                for (const r of priorRuns) {
                  void deleteSessionCache(r.project_hash, r.id);
                  onSessionDeleted(r.id);
                }
                setRuns([]);
              }
              setClearRunsOpen(false);
              if (res.skipped_active > 0 || res.failed > 0) {
                showToast(
                  t('tasks.runsClearedPartialToast', {
                    deleted: res.deleted,
                    skipped: res.skipped_active + res.failed,
                  }),
                  'info',
                );
              } else {
                showToast(t('tasks.runsClearedToast', { n: res.deleted }), 'success');
              }
            } catch (err: any) {
              const msg = err?.message || t('tasks.clearRunsFailed');
              throw new Error(msg);
            } finally {
              setClearingRuns(false);
            }
          }}
          onClose={() => setClearRunsOpen(false)}
        />
      )}
    </div>
  );
}
