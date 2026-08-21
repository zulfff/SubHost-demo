(() => {
  const views = [...document.querySelectorAll('.doc-view')];
  const navItems = [...document.querySelectorAll('.nav-item')];
  const search = document.querySelector('#docSearch');
  const sidebar = document.querySelector('#sidebar');
  const menuToggle = document.querySelector('#menuToggle');
  const notFound = document.querySelector('#notFound');
  const viewIds = new Set(views.map((view) => view.id));

  function setView(id, updateHash = true) {
    const target = viewIds.has(id) ? id : 'overview';
    views.forEach((view) => view.classList.toggle('active', view.id === target));
    navItems.forEach((item) => item.classList.toggle('active', item.dataset.target === target));
    if (updateHash) history.replaceState(null, '', `#${target}`);
    sidebar?.classList.remove('open');
    if (menuToggle) menuToggle.setAttribute('aria-expanded', 'false');
    window.scrollTo({ top: 0, behavior: 'smooth' });
  }

  navItems.forEach((item) => item.addEventListener('click', () => setView(item.dataset.target)));
  window.addEventListener('hashchange', () => {
    const id = location.hash.slice(1);
    if (viewIds.has(id)) setView(id, false);
  });
  document.querySelectorAll('[data-target-link]').forEach((link) => {
    link.addEventListener('click', (event) => {
      event.preventDefault();
      setView(link.dataset.targetLink);
    });
  });

  function filterDocs(query) {
    const normalized = query.trim().toLowerCase();
    if (!normalized) {
      notFound.hidden = true;
      navItems.forEach((item) => { item.hidden = false; });
      setView(location.hash.slice(1) || 'overview', false);
      return;
    }
    let firstMatch = null;
    navItems.forEach((item) => {
      const view = document.getElementById(item.dataset.target);
      const match = `${item.textContent} ${view?.textContent || ''}`.toLowerCase().includes(normalized);
      item.hidden = !match;
      if (match && !firstMatch) firstMatch = item.dataset.target;
    });
    notFound.hidden = Boolean(firstMatch);
    if (firstMatch) setView(firstMatch, false);
    else views.forEach((view) => view.classList.remove('active'));
  }

  search?.addEventListener('input', (event) => filterDocs(event.target.value));
  document.addEventListener('keydown', (event) => {
    if (event.key === '/' && document.activeElement !== search) {
      event.preventDefault();
      search?.focus();
    }
    if (event.key === 'Escape' && search) {
      search.value = '';
      filterDocs('');
      search.blur();
    }
  });
  menuToggle?.addEventListener('click', () => {
    const open = sidebar.classList.toggle('open');
    menuToggle.setAttribute('aria-expanded', String(open));
  });

  document.querySelectorAll('.copy-button').forEach((button) => {
    button.addEventListener('click', async () => {
      try {
        await navigator.clipboard.writeText(button.dataset.copy);
        const original = button.textContent;
        button.textContent = 'Copied';
        setTimeout(() => { button.textContent = original; }, 1200);
      } catch (_) {
        button.textContent = 'Select manually';
      }
    });
  });

  const initial = location.hash.slice(1);
  setView(initial || 'overview', false);
})();
