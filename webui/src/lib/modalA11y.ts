import type { RefObject } from 'preact';
import { useEffect, useRef } from 'preact/hooks';

const FOCUSABLE_SELECTOR = [
  'a[href]',
  'button:not([disabled])',
  'input:not([disabled])',
  'select:not([disabled])',
  'textarea:not([disabled])',
  '[tabindex]:not([tabindex="-1"])',
].join(', ');

const INTERACTIVE_DESCENDANT_SELECTOR =
  'button, a, input, select, textarea, [role="button"]';

/** Whether `container` is the topmost open `[role="dialog"][aria-modal="true"]` in document order. */
export function isTopmostDialog(
  container: unknown,
  dialogs: ReadonlyArray<unknown>,
): boolean {
  if (!container || dialogs.length === 0) return false;
  return dialogs[dialogs.length - 1] === container;
}

/** Next focusable index for `Tab` / `Shift+Tab` inside a modal focus trap. */
export function nextFocusTrapIndex(
  currentIndex: number,
  total: number,
  shiftKey: boolean,
): number {
  if (total <= 0) return -1;
  if (currentIndex < 0 || currentIndex >= total) {
    return shiftKey ? total - 1 : 0;
  }
  return shiftKey ? (currentIndex - 1 + total) % total : (currentIndex + 1) % total;
}

/** Ignore keyboard row activation when focus is on a nested interactive control. */
export function shouldIgnoreKeyboardActivateTarget(
  target: { closest?: (selector: string) => unknown } | null | undefined,
  currentTarget: unknown,
): boolean {
  if (!target || typeof target.closest !== 'function') return false;
  const interactive = target.closest(INTERACTIVE_DESCENDANT_SELECTOR);
  return Boolean(interactive && interactive !== currentTarget);
}

/** Accessible keyboard activation props for clickable `<div role="button">` rows/cards. */
export function keyboardActivateProps(onActivate: () => void) {
  return {
    role: 'button' as const,
    tabIndex: 0,
    onKeyDown: (e: KeyboardEvent) => {
      if (e.isComposing) return;
      if (e.key !== 'Enter' && e.key !== ' ') return;
      if (shouldIgnoreKeyboardActivateTarget(e.target as Element | null, e.currentTarget)) {
        return;
      }
      e.preventDefault();
      onActivate();
    },
  };
}

function getFocusableElements(container: HTMLElement): HTMLElement[] {
  return Array.from(container.querySelectorAll<HTMLElement>(FOCUSABLE_SELECTOR)).filter(
    (el) => el.offsetParent !== null || el === document.activeElement,
  );
}

export interface ModalA11yOptions {
  open?: boolean;
  disabled?: boolean;
  onClose: () => void;
  initialFocusRef?: RefObject<HTMLElement | null>;
}

/**
 * Shared modal keyboard & focus-management hook:
 * - Captures trigger focus on open and restores it once on close/unmount.
 * - Moves initial focus into the dialog (`initialFocusRef` -> first focusable -> container).
 * - Traps `Tab` / `Shift+Tab` and dismisses on `Escape` only for the topmost open dialog.
 */
export function useModalA11y(
  containerRef: RefObject<HTMLElement | null>,
  { open = true, disabled = false, onClose, initialFocusRef }: ModalA11yOptions,
) {
  const onCloseRef = useRef(onClose);
  onCloseRef.current = onClose;
  const disabledRef = useRef(disabled);
  disabledRef.current = disabled;
  const previousFocusRef = useRef<HTMLElement | null>(null);

  useEffect(() => {
    if (!open || typeof document === 'undefined' || typeof window === 'undefined') {
      return;
    }
    const container = containerRef.current;
    if (!container) return;

    const active = document.activeElement;
    previousFocusRef.current =
      active instanceof HTMLElement && !container.contains(active) ? active : null;

    if (!container.hasAttribute('tabindex')) {
      container.setAttribute('tabindex', '-1');
    }

    const focusInitial = () => {
      const target = initialFocusRef?.current;
      if (target && container.contains(target)) {
        target.focus();
        return;
      }
      if (container.contains(document.activeElement)) return;
      const focusables = getFocusableElements(container);
      if (focusables.length > 0) {
        focusables[0]!.focus();
      } else {
        container.focus();
      }
    };

    const raf = window.requestAnimationFrame(focusInitial);

    const onKeyDown = (e: KeyboardEvent) => {
      if (e.isComposing) return;
      const dialogs = Array.from(
        document.querySelectorAll('[role="dialog"][aria-modal="true"]'),
      );
      if (!isTopmostDialog(container, dialogs)) return;

      if (e.key === 'Escape') {
        if (disabledRef.current) return;
        e.preventDefault();
        e.stopImmediatePropagation();
        onCloseRef.current();
        return;
      }

      if (e.key === 'Tab') {
        const focusables = getFocusableElements(container);
        if (focusables.length === 0) {
          e.preventDefault();
          container.focus();
          return;
        }
        const currentIdx = focusables.indexOf(document.activeElement as HTMLElement);
        const nextIdx = nextFocusTrapIndex(currentIdx, focusables.length, e.shiftKey);
        if (nextIdx >= 0 && focusables[nextIdx]) {
          e.preventDefault();
          focusables[nextIdx]!.focus();
        }
      }
    };

    window.addEventListener('keydown', onKeyDown);
    return () => {
      window.cancelAnimationFrame(raf);
      window.removeEventListener('keydown', onKeyDown);
      const prev = previousFocusRef.current;
      previousFocusRef.current = null;
      if (prev && prev.isConnected) {
        prev.focus();
      }
    };
  }, [open, containerRef, initialFocusRef]);
}
