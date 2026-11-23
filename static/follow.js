// Keeps a live log scrolled to its newest line, like a terminal. The log block is swapped by htmx every
// couple of seconds, which would reset the scroll to the top; this restores it. If the reader scrolled up
// to read something, their position is kept instead of being pulled down.
(() => {
  const SELECTOR = '[data-follow] pre';
  const NEAR = 24; // px from the bottom that still counts as "at the bottom"
  const state = new Map();
  document.addEventListener('htmx:beforeSwap', (e) => {
    const host = e.target && e.target.matches && e.target.matches('[data-follow]') ? e.target : null;
    const pre = host && host.querySelector('pre');
    if (host && host.id) {
      state.set(host.id, pre ? { top: pre.scrollTop, stick: pre.scrollHeight - pre.scrollTop - pre.clientHeight <= NEAR } : { top: 0, stick: true });
    }
  });
  document.addEventListener('htmx:afterSwap', () => {
    document.querySelectorAll('[data-follow]').forEach((host) => {
      const pre = host.querySelector('pre');
      if (!pre) return;
      const s = state.get(host.id);
      pre.scrollTop = !s || s.stick ? pre.scrollHeight : s.top;
    });
  });
})();
