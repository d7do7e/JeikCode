import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

test('desktop splash shows Jeikcode, a spinner, and Starting with external CSP assets', async () => {
  const html = await readFile(new URL('../index.html', import.meta.url), 'utf8');
  const css = await readFile(new URL('../boot.css', import.meta.url), 'utf8');
  const js = await readFile(new URL('../boot.js', import.meta.url), 'utf8');
  assert.match(html, /<h1>Jeikcode<\/h1>/);
  assert.match(html, /<p id="status">Starting<\/p>/);
  assert.match(html, /id="spinner"/);
  assert.match(html, /<link rel="stylesheet" href="boot\.css" \/>/);
  assert.match(html, /<script defer src="boot\.js"><\/script>/);
  assert.doesNotMatch(html, /<style[\s>]/i);
  assert.doesNotMatch(html, /<script(?![^>]*\bsrc=)[^>]*>/i);
  assert.match(css, /@keyframes boot-spin/);
  assert.match(js, /MutationObserver/);
  assert.doesNotMatch(html, /正在启动|Đang khởi động|Starting…/);
  assert.doesNotMatch(html, /startup-i18n/);
});
