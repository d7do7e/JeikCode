/**
 * Trigger a browser/WebView file download from a `Blob` and defer `URL.revokeObjectURL`
 * so WebView2 / Chromium can start reading the blob stream before the URL is invalidated.
 */
export function triggerBlobDownload(blob: Blob, filename: string, revokeDelayMs = 1000): void {
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = filename;
  document.body.appendChild(a);
  a.click();
  document.body.removeChild(a);
  const schedule = typeof window !== 'undefined' && typeof window.setTimeout === 'function'
    ? window.setTimeout.bind(window)
    : setTimeout;
  schedule(() => {
    URL.revokeObjectURL(url);
  }, revokeDelayMs);
}
