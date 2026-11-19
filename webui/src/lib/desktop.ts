const DESKTOP_FLAG_KEY = 'jeikcode_desktop';

/**
 * Returns `true` when running inside the JeikCode Tauri desktop WebView.
 * Persists `jeikcode_desktop=1` in `sessionStorage` once `?desktop=` is seen
 * and also recognizes the native `window.__jeikcodeHostAway` property injected
 * by the Tauri shell.
 */
export function isDesktopShell(): boolean {
  if (typeof window === 'undefined') return false;
  try {
    const search = window.location?.search ?? '';
    if (search.includes('desktop=')) {
      window.sessionStorage?.setItem(DESKTOP_FLAG_KEY, '1');
      return true;
    }
    if (window.sessionStorage?.getItem(DESKTOP_FLAG_KEY) === '1') {
      return true;
    }
  } catch {
    /* ignore storage errors in restricted contexts */
  }
  return typeof window.__jeikcodeHostAway === 'boolean';
}

/**
 * Trigger a Tauri desktop shell action via top-level navigation interception
 * (`on_navigation` in `desktop/src-tauri/src/main.rs` intercepts `/__desktop_action/*`
 * and returns `false` without navigating the WebView away or opening popups).
 */
export function triggerDesktopAction(
  action: 'hide_to_tray' | 'quit_app' | 'set_language',
  params?: Record<string, string>,
): boolean {
  if (typeof window === 'undefined' || !window.location) return false;
  const url = new URL(`http://tauri.localhost/__desktop_action/${action}`);
  if (params) {
    for (const [k, v] of Object.entries(params)) {
      url.searchParams.set(k, v);
    }
  }
  window.location.href = url.toString();
  return true;
}
