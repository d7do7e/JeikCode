import assert from 'node:assert/strict';
import test from 'node:test';
import { isDesktopShell, triggerDesktopAction } from './desktop.ts';
import { triggerBlobDownload } from './download.ts';

test('isDesktopShell detects ?desktop= query, sessionStorage, and __jeikcodeHostAway', () => {
  const origWin = (globalThis as any).window;
  try {
    const store = new Map<string, string>();
    (globalThis as any).window = {
      location: { search: '?token=abc&desktop=1' },
      sessionStorage: {
        getItem: (k: string) => store.get(k) ?? null,
        setItem: (k: string, v: string) => store.set(k, String(v)),
      },
    };
    assert.equal(isDesktopShell(), true);
    assert.equal(store.get('jeikcode_desktop'), '1');

    // Subsequent page with ?desktop= stripped still resolves true via sessionStorage.
    (globalThis as any).window.location.search = '';
    assert.equal(isDesktopShell(), true);

    // Cleared storage still falls back to __jeikcodeHostAway boolean injected by Tauri.
    store.clear();
    assert.equal(isDesktopShell(), false);
    (globalThis as any).window.__jeikcodeHostAway = false;
    assert.equal(isDesktopShell(), true);
  } finally {
    if (origWin === undefined) delete (globalThis as any).window;
    else (globalThis as any).window = origWin;
  }
});

test('triggerDesktopAction navigates to encoded tauri.localhost shell action', () => {
  const origWin = (globalThis as any).window;
  try {
    const loc = { href: 'http://127.0.0.1:13457/' };
    (globalThis as any).window = { location: loc };
    const ok = triggerDesktopAction('set_language', { lang: 'zh-TW' });
    assert.equal(ok, true);
    assert.equal(
      loc.href,
      'http://tauri.localhost/__desktop_action/set_language?lang=zh-TW',
    );
  } finally {
    if (origWin === undefined) delete (globalThis as any).window;
    else (globalThis as any).window = origWin;
  }
});

test('triggerBlobDownload defers URL.revokeObjectURL until after click returns', async () => {
  const origDoc = (globalThis as any).document;
  const origCreate = URL.createObjectURL;
  const origRevoke = URL.revokeObjectURL;
  const events: string[] = [];

  try {
    URL.createObjectURL = () => 'blob:http://127.0.0.1:13457/test-blob';
    URL.revokeObjectURL = (u: string) => {
      events.push(`revoke:${u}`);
    };
    const fakeAnchor: any = {
      href: '',
      download: '',
      click: () => events.push(`click:${fakeAnchor.download}`),
    };
    (globalThis as any).document = {
      createElement: () => fakeAnchor,
      body: {
        appendChild: () => events.push('append'),
        removeChild: () => events.push('remove'),
      },
    };

    triggerBlobDownload(new Blob(['hello']), 'out.md', 20);
    // Synchronously right after triggerBlobDownload returns, URL must NOT be revoked yet.
    assert.deepEqual(events, ['append', 'click:out.md', 'remove']);

    await new Promise((r) => setTimeout(r, 40));
    assert.deepEqual(events, [
      'append',
      'click:out.md',
      'remove',
      'revoke:blob:http://127.0.0.1:13457/test-blob',
    ]);
  } finally {
    URL.createObjectURL = origCreate;
    URL.revokeObjectURL = origRevoke;
    if (origDoc === undefined) delete (globalThis as any).document;
    else (globalThis as any).document = origDoc;
  }
});
