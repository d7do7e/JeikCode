const status = document.getElementById('status');
const spinner = document.getElementById('spinner');
const sync = () => {
  if (!status || !spinner) return;
  spinner.hidden = (status.textContent || '').trim() !== 'Starting';
};
if (status) {
  try {
    const err = new URLSearchParams(window.location.search).get('error');
    if (err && err.trim()) {
      status.textContent = err;
    }
  } catch {}
  sync();
  new MutationObserver(sync).observe(status, { childList: true, characterData: true, subtree: true });
}
