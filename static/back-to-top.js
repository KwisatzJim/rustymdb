(() => {
  const link = document.querySelector('.back-to-top');
  const top = document.getElementById('page-top');
  if (!link || !top) return;
  const update = () => { link.hidden = window.scrollY < 400; };
  window.addEventListener('scroll', update, { passive: true });
  window.addEventListener('pageshow', update);
  link.addEventListener('click', event => {
    event.preventDefault();
    top.focus({ preventScroll: true });
    window.scrollTo({ top: 0, behavior: window.matchMedia('(prefers-reduced-motion: reduce)').matches ? 'instant' : 'smooth' });
  });
  update();
})();
