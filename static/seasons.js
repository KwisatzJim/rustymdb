document.querySelectorAll('.season-section').forEach(section => {
  const body = section.querySelector('.season-episodes');
  let loading = false;
  async function load(page, moveFocus = false) {
    if (loading) return;
    loading = true;
    body.setAttribute('aria-busy', 'true');
    body.querySelector('.season-load-status')?.remove();
    const status = document.createElement('p');
    status.className = 'season-load-status';
    status.setAttribute('role', 'status');
    status.textContent = 'Loading episodes…';
    body.prepend(status);
    try {
      const url = new URL(`/title/${section.dataset.titleId}/episodes`, window.location.origin);
      url.searchParams.set('season', section.dataset.season);
      url.searchParams.set('page', page);
      const response = await fetch(url, { redirect: 'error', headers: { Accept: 'text/html' } });
      if (!response.ok) throw new Error('Episode request failed');
      body.innerHTML = await response.text();
      body.dataset.loaded = 'true';
      if (moveFocus) body.focus({ preventScroll: true });
    } catch {
      body.dataset.loaded = 'false';
      status.textContent = 'Episodes could not be loaded. Use the link below, or close and reopen this season to retry.';
    } finally {
      loading = false;
      body.removeAttribute('aria-busy');
    }
  }
  section.addEventListener('toggle', () => {
    if (section.open && body.dataset.loaded !== 'true') load('1');
  });
  body.addEventListener('click', event => {
    if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
    const link = event.target instanceof Element ? event.target.closest('.pagination a') : null;
    if (!link) return;
    event.preventDefault();
    const url = new URL(link.href);
    load(url.searchParams.get('page') || '1', true);
  });
});
