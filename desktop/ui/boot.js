const status = document.getElementById('status');
const spinner = document.getElementById('spinner');
const sync = () => {
  if (!status || !spinner) return;
  spinner.hidden = (status.textContent || '').trim() !== 'Starting';
};
if (status) {
  new MutationObserver(sync).observe(status, { childList: true, characterData: true, subtree: true });
}
