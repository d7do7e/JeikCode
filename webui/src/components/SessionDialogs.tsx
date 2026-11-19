// Rename + delete dialogs for a session (triggered from the sidebar item menu).

import { useEffect, useRef, useState } from 'preact/hooks';
import { renameSession, deleteSession, DeleteSessionError, SessionMetaWithProject } from '../api';
import { deleteSessionsStreaming, SESSION_DELETE_CONCURRENCY } from '../lib/sessionDelete';
import { useBackdropDismiss } from '../lib/backdropDismiss';
import { useT } from '../settings';

interface RenameDialogProps {
  session: SessionMetaWithProject;
  onClose: () => void;
  /** Called with the new name after a successful rename. */
  onDone: (name: string) => void;
}

export function RenameDialog({ session, onClose, onDone }: RenameDialogProps) {
  const t = useT();
  const [name, setName] = useState(session.name || '');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  // 遮罩空白处点击关闭：避免弹窗内拖拽划词（左滑选中）时被误判为外部点击而秒关。
  const backdropDismiss = useBackdropDismiss(() => {
    if (!busy) onClose();
  });

  useEffect(() => {
    inputRef.current?.focus();
    inputRef.current?.select();
  }, []);

  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      if (busy || e.isComposing) return;
      if (e.key === 'Escape') {
        e.preventDefault();
        onClose();
      }
    }
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [busy, onClose]);

  async function submit() {
    const n = name.trim();
    if (!n || busy) return;
    setBusy(true);
    setError(null);
    try {
      await renameSession(session.project_hash, session.id, n);
      onDone(n);
      onClose();
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  }

  return (
    <div
      class="modal-overlay"
      {...backdropDismiss}
    >
      <div class="modal-card modal-card-sm" role="dialog" aria-modal="true">
        <div class="modal-header">
          <span>✎</span>
          <h3>{t('rename.title')}</h3>
          <button
            type="button"
            class="ghost-btn modal-close"
            onClick={() => { if (!busy) onClose(); }}
            disabled={busy}
            aria-label={t('common.cancel')}
          >
            ×
          </button>
        </div>
        <div class="modal-body">
          <input
            ref={inputRef}
            class="menu-input"
            type="text"
            value={name}
            placeholder={t('rename.placeholder')}
            disabled={busy}
            onInput={(e) => setName((e.target as HTMLInputElement).value)}
            onKeyDown={(e) => {
              // 忽略输入法组字阶段的回车（选词确认），避免误触发保存。
              if (e.isComposing) return;
              if (e.key === 'Enter') submit();
            }}
          />
          {error && <div class="modal-error" role="alert">{error}</div>}
        </div>
        <div class="modal-footer">
          <button type="button" class="btn" onClick={() => { if (!busy) onClose(); }} disabled={busy}>
            {t('common.cancel')}
          </button>
          <button type="button" class="btn btn-primary" onClick={submit} disabled={busy || !name.trim()}>
            {t('rename.confirm')}
          </button>
        </div>
      </div>
    </div>
  );
}

interface DeleteDialogProps {
  /** One or more sessions. A single item keeps the original confirm copy. */
  sessions: SessionMetaWithProject[];
  onClose: () => void;
  /** Called with ids that were actually removed. May fire repeatedly during a bulk delete. */
  onDone: (deletedIds: string[]) => void;
  /** Bulk delete: fired once the confirm overlay is dismissed and work starts. */
  onBatchStart?: (count: number) => void;
  /** Bulk delete: fired after each item settles so the sidebar can count down. */
  onBatchProgress?: () => void;
  /** Bulk delete: fired when every item has succeeded or failed. */
  onBatchFinished?: (error: string | null) => void;
}

function localizeDeleteError(cause: unknown, t: ReturnType<typeof useT>): string {
  if (cause instanceof DeleteSessionError) {
    const localized = (
      {
        SESSION_IN_USE: t('delete.inUse'),
        SESSION_NOT_FOUND: t('delete.notFound'),
        INVALID_SESSION: t('delete.invalid'),
        DELETE_FAILED: t('delete.failed'),
      } as Record<string, string>
    )[cause.code ?? ''];
    if (localized) return localized;
  }
  return cause instanceof Error ? cause.message : String(cause);
}

export function DeleteDialog({
  sessions,
  onClose,
  onDone,
  onBatchStart,
  onBatchFinished,
  onBatchProgress,
}: DeleteDialogProps) {
  const t = useT();
  // 遮罩空白处点击关闭：避免弹窗内拖拽划词（左滑选中）时被误判为外部点击而秒关。
  const backdropDismiss = useBackdropDismiss(() => {
    if (!busy) onClose();
  });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const remaining = sessions;
  const name = remaining[0]?.name || remaining[0]?.id.slice(0, 8) || '';
  const many = remaining.length > 1;
  const body = many
    ? t('delete.bodyMany', { n: remaining.length })
    : t('delete.body', { name });

  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      if (busy || e.isComposing) return;
      if (e.key === 'Escape') {
        e.preventDefault();
        onClose();
      }
    }
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [busy, onClose]);

  async function confirm() {
    if (busy || remaining.length === 0) return;
    const batch = remaining.slice();
    const bulk = batch.length > 1;
    setBusy(true);
    setError(null);
    const notifyDone = onDone;
    const notifyProgress = onBatchProgress;
    const notifyFinished = onBatchFinished;
    const close = onClose;
    if (bulk) {
      onBatchStart?.(batch.length);
      close();
    }
    const { failed } = await deleteSessionsStreaming(
      batch,
      deleteSession,
      (id) => notifyDone([id]),
      SESSION_DELETE_CONCURRENCY,
      notifyProgress,
    );
    if (failed.length === 0) {
      if (!bulk) close();
      if (bulk) notifyFinished?.(null);
      return;
    }
    const names = failed.map((item) => item.name).join('、');
    const message =
      failed.length === 1
        ? localizeDeleteError(failed[0].cause, t)
        : t('delete.partialFailed', { n: failed.length, names });
    if (bulk) {
      notifyFinished?.(message);
      return;
    }
    setError(message);
    setBusy(false);
  }

  return (
    <div
      class="modal-overlay"
      {...backdropDismiss}
    >
      <div class="modal-card modal-card-sm" role="dialog" aria-modal="true">
        <div class="modal-header">
          <span>🗑</span>
          <h3>{t('delete.title')}</h3>
          <button
            type="button"
            class="ghost-btn modal-close"
            onClick={() => { if (!busy) onClose(); }}
            disabled={busy}
            aria-label={t('common.cancel')}
          >
            ×
          </button>
        </div>
        <div class="modal-body">
          <p class="field-hint">{body}</p>
          {error && <div class="modal-error" role="alert">{error}</div>}
        </div>
        <div class="modal-footer">
          <button type="button" class="btn" onClick={() => { if (!busy) onClose(); }} disabled={busy}>
            {t('common.cancel')}
          </button>
          <button type="button" class="btn btn-danger" onClick={confirm} disabled={busy || remaining.length === 0}>
            {t('delete.confirm')}
          </button>
        </div>
      </div>
    </div>
  );
}
