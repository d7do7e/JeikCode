import { useState } from 'preact/hooks';
import { applyUpgradeDiffs, dismissUpgradeDiffs, ConfigDiffItem } from '../api';
import { useT } from '../settings';

interface ConfigSyncModalProps {
  diffs: ConfigDiffItem[];
  onDone: (appliedCount: number) => void;
  onSkip: () => void;
  onLoadFromDisk?: () => Promise<void>;
}

export function ConfigSyncModal({ diffs, onDone, onSkip, onLoadFromDisk }: ConfigSyncModalProps) {
  const t = useT();
  const [selectedMap, setSelectedMap] = useState<Record<string, boolean>>(() => {
    const initial: Record<string, boolean> = {};
    for (const d of diffs) {
      initial[d.relative_path] = d.selected;
    }
    return initial;
  });
  const [expandedDiffs, setExpandedDiffs] = useState<Record<string, boolean>>({});
  const [submitting, setSubmitting] = useState(false);
  const [errorMsg, setErrorMsg] = useState<string | null>(null);

  const selectedCount = Object.values(selectedMap).filter(Boolean).length;
  const allSelected = selectedCount === diffs.length;

  const handleToggleAll = () => {
    const nextVal = !allSelected;
    const next: Record<string, boolean> = {};
    for (const d of diffs) {
      next[d.relative_path] = nextVal;
    }
    setSelectedMap(next);
  };

  const handleToggleItem = (path: string) => {
    setSelectedMap((prev) => ({
      ...prev,
      [path]: !prev[path],
    }));
  };

  const handleTogglePreview = (path: string, e: Event) => {
    e.stopPropagation();
    setExpandedDiffs((prev) => ({
      ...prev,
      [path]: !prev[path],
    }));
  };

  const handleConfirm = async () => {
    const selectedPaths = Object.entries(selectedMap)
      .filter(([_, sel]) => sel)
      .map(([path]) => path);

    setSubmitting(true);
    setErrorMsg(null);
    try {
      const res = await applyUpgradeDiffs(selectedPaths);
      onDone(res.applied_count);
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      setErrorMsg(t('configSync.applyFailed', { error: msg }));
      setSubmitting(false);
    }
  };

  const handleLoadFromDisk = async () => {
    if (!onLoadFromDisk || submitting) return;
    setSubmitting(true);
    setErrorMsg(null);
    try {
      await onLoadFromDisk();
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      setErrorMsg(msg);
      setSubmitting(false);
    }
  };

  const handleSkip = async () => {
    setSubmitting(true);
    try {
      await dismissUpgradeDiffs();
      onSkip();
    } catch (e) {
      console.error('Failed to dismiss config diffs', e);
      onSkip();
    }
  };

  return (
    <div class="modal-overlay">
      <div
        class="modal-card config-sync-modal-card"
        role="dialog"
        aria-labelledby="config-sync-title"
      >
        <div class="modal-header">
          <span style="font-size: 16px;">⚙️</span>
          <h3 id="config-sync-title">{t('configSync.dialogTitle')}</h3>
        </div>

        <div class="modal-body config-sync-body">
          <p class="config-sync-desc-text">
            {t('configSync.dialogDesc')}
          </p>

          {errorMsg && (
            <div class="config-sync-error" role="alert">
              {errorMsg}
            </div>
          )}

          <div class="config-sync-toolbar">
            <button
              type="button"
              class="config-sync-toggle-all-btn"
              onClick={handleToggleAll}
            >
              {allSelected ? t('configSync.unselectAll') : t('configSync.selectAll')}
            </button>
            <span class="config-sync-count-tag">
              {t('configSync.selectedCount', { n: selectedCount, total: diffs.length })}
            </span>
          </div>

          <div class="config-sync-list">
            {diffs.map((item) => {
              const isChecked = !!selectedMap[item.relative_path];
              const isExpanded = !!expandedDiffs[item.relative_path];
              const kindBadgeClass =
                item.kind === 'new'
                  ? 'badge-new'
                  : item.kind === 'modified'
                    ? 'badge-modified'
                    : 'badge-obsolete';
              const kindBadgeText =
                item.kind === 'new'
                  ? t('configSync.kindNew')
                  : item.kind === 'modified'
                    ? t('configSync.kindModified')
                    : t('configSync.kindObsolete');

              return (
                <div
                  key={item.relative_path}
                  class={`config-sync-item ${isChecked ? 'selected' : ''}`}
                  onClick={() => handleToggleItem(item.relative_path)}
                >
                  <input
                    type="checkbox"
                    class="config-sync-checkbox"
                    checked={isChecked}
                    onChange={() => handleToggleItem(item.relative_path)}
                    onClick={(e) => e.stopPropagation()}
                  />
                  <div class="config-sync-item-info">
                    <div class="config-sync-item-top">
                      <span class="config-sync-item-path">
                        {item.relative_path}
                      </span>
                      <div style="display: flex; align-items: center; gap: 6px; flex-shrink: 0;">
                        <button
                          type="button"
                          class="config-sync-preview-btn"
                          onClick={(e) => handleTogglePreview(item.relative_path, e)}
                        >
                          {isExpanded ? t('configSync.hideDiff') : t('configSync.previewDiff')}
                        </button>
                        <span class={`config-sync-kind-badge ${kindBadgeClass}`}>
                          {kindBadgeText}
                        </span>
                      </div>
                    </div>
                    <div class="config-sync-item-desc">
                      {item.description}
                    </div>
                    {isExpanded && (
                      <pre
                        class="config-sync-diff-preview"
                        onClick={(e) => e.stopPropagation()}
                      >
                        {item.diff_preview || t('configSync.noDiffPreview')}
                      </pre>
                    )}
                  </div>
                </div>
              );
            })}
          </div>
        </div>

        <div class="modal-footer config-sync-footer">
          {onLoadFromDisk && (
            <button
              type="button"
              class="btn btn-secondary"
              onClick={handleLoadFromDisk}
              disabled={submitting}
            >
              {t('configSync.btnLoadFromDisk')}
            </button>
          )}
          <div class="config-sync-footer-actions">
            <button
              type="button"
              class="btn btn-secondary"
              onClick={handleSkip}
              disabled={submitting}
            >
              {t('configSync.btnSkip')}
            </button>
            <button
              type="button"
              class="btn btn-primary config-sync-confirm-btn"
              onClick={handleConfirm}
              disabled={submitting || selectedCount === 0}
            >
              {submitting ? t('configSync.applying') : t('configSync.btnApply')}
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
