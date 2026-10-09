// Example Ferro admin extension. Place the folder in FERRO_EXTENSIONS_DIR.
// `ferro` gives access to the API client, the DOM helpers, the schema helpers and realtime.

export default function register(ferro) {
  const { h, button, toast } = ferro.dom;

  // 1. A page in the sidebar (#/ext/overview) with a live counter
  ferro.addPage({
    id: 'overview',
    title: 'Overview',
    async render(root) {
      const count = h('p', {}, '…');
      const refresh = async () => {
        const r = await ferro.api.get('/items/pages', { fields: 'id', limit: 1, meta: 'filter_count' });
        count.textContent = `${r.meta.filter_count} pages in this environment.`;
      };
      await refresh();
      root.append(h('div', { class: 'page-head' }, h('h1', {}, 'Overview')), h('div', { class: 'card' }, count));
      // realtime: the counter follows every change made by anyone; the subscription ends when the page is left
      ferro.onLeave(ferro.realtime.subscribe('pages', (ev) => (ev.event === 'error' ? null : refresh()), { fields: 'id' }));
    },
  });

  // 2. A field widget: set "interface": "stars" in the field metadata
  ferro.addInterface('stars', (field, value, ctx) => {
    let current = Number(value) || 0;
    const wrap = h('div', { class: 'row-actions' });
    const draw = () =>
      wrap.replaceChildren(
        ...[1, 2, 3, 4, 5].map((n) =>
          button(n <= current ? '★' : '☆', () => {
            if (!ctx.readonly) {
              current = n;
              draw();
            }
          }, 'icon'),
        ),
      );
    draw();
    return { el: wrap, get: () => current };
  });

  // 3. A button on the item page of the listed collections
  ferro.addItemAction({
    label: 'Copy slug',
    collections: ['pages'],
    run: async ({ item }) => {
      await navigator.clipboard.writeText(item.slug || '');
      toast('Slug copied', 'ok');
    },
  });
}
