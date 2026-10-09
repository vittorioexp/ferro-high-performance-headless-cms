def edit(p, old, new, count=1):
    s = open(p, encoding='utf-8').read()
    assert s.count(old) == count, (p, old[:70], s.count(old))
    open(p, 'w', encoding='utf-8').write(s.replace(old, new))


# api.js: expose the current access token
edit('js/api.js', "export const baseUrl = () => state.base;", "export const baseUrl = () => state.base;\nexport const currentToken = () => state.access;")

# extensions can use realtime and cleanup hooks
edit(
    'js/extensions.js',
    "import * as schema from './schema.js';",
    "import { onLeave } from './lifecycle.js';\nimport { realtime } from './realtime.js';\nimport * as schema from './schema.js';",
)
edit('js/extensions.js', "createExtensionApi(ext, { api, dom, schema })", "createExtensionApi(ext, { api, dom, schema, realtime, onLeave })")
edit('js/registry.js', "    schema: deps.schema,\n", "    schema: deps.schema,\n    realtime: deps.realtime,\n    onLeave: deps.onLeave,\n")

# main.js: leave hooks + close the socket when signing out
edit('js/main.js', "import { matchRoute } from './util.js';", "import { runLeave } from './lifecycle.js';\nimport { realtime } from './realtime.js';\nimport { matchRoute } from './util.js';")
edit('js/main.js', "async function route() {\n  const main = document.getElementById('main');\n  if (!main) return;", "async function route() {\n  const main = document.getElementById('main');\n  if (!main) return;\n  runLeave();")
edit('js/main.js', "function showLogin(message) {\n  routeToken++;", "function showLogin(message) {\n  routeToken++;\n  runLeave();\n  realtime.close();")

# content list: banner on changes
edit('js/views/content.js', "import { clearOptionCache, FormError, renderForm } from '../form.js';", "import { clearOptionCache, FormError, renderForm } from '../form.js';\nimport { onLeave } from '../lifecycle.js';\nimport { realtime } from '../realtime.js';")
edit('js/views/content.js', "import { buildFilter, FILTER_OPS, shortId, truncate } from '../util.js';", "import { buildFilter, FILTER_OPS, shortId, truncate } from '../util.js';\nimport { changeBanner } from './banner.js';")
edit(
    'js/views/content.js',
    "  add(\n    root,\n    h(\n      'div',\n      { class: 'page-head' },\n      h('h1', {}, S.collectionLabel(c)),",
    """  const banner = changeBanner('Refresh', () => reload(state.page));
  onLeave(
    realtime.subscribe(
      collection,
      (ev) => {
        if (ev.event === 'error') return;
        banner.show(ev.event === 'resync' ? 'The list may be out of date.' : `Someone ${ev.event === 'create' ? 'added' : ev.event === 'delete' ? 'deleted' : 'changed'} items in this collection.`);
      },
      { fields: 'id' },
    ),
  );

  add(
    root,
    banner.el,
    h(
      'div',
      { class: 'page-head' },
      h('h1', {}, S.collectionLabel(c)),""",
)

# item editor: banner when somebody else changes or deletes the item
edit(
    'js/views/content.js',
    "  async function save(thenClose) {\n    let payload;",
    """  let savedAt = 0; // our own writes echo back through the socket: ignore them
  const banner = changeBanner('Reload', () => rerender(id));
  if (!isNew) {
    onLeave(
      realtime.subscribe(
        collection,
        (ev) => {
          if (ev.event === 'error' || Date.now() - savedAt < 3000) return;
          if (ev.event === 'delete') {
            if (Array.isArray(ev.data) && ev.data.includes(id)) banner.show('This item was deleted.');
          } else if (ev.event === 'resync') banner.show('This item may have changed.');
          else banner.show('This item was changed by someone else. Reload to see the new version (your edits will be lost).');
        },
        { fields: 'id', filter: { id: { _eq: id } } },
      ),
    );
  }

  async function save(thenClose) {
    let payload;""",
)
edit(
    'js/views/content.js',
    "    const r = isNew ? await api.post(`/items/${collection}`, payload) : await api.patch(`/items/${collection}/${id}`, payload);\n    clearOptionCache();",
    "    savedAt = Date.now();\n    const r = isNew ? await api.post(`/items/${collection}`, payload) : await api.patch(`/items/${collection}/${id}`, payload);\n    savedAt = Date.now();\n    clearOptionCache();",
)
edit(
    'js/views/content.js',
    "    h('div', { class: 'card' }, form.el),\n    item?.date_updated",
    "    banner.el,\n    h('div', { class: 'card' }, form.el),\n    item?.date_updated",
)

edit('css/app.css', "/* surfaces */", ".banner { display: flex; gap: .7rem; align-items: center; padding: .5rem 1rem; margin-bottom: 1rem; border: 1px solid var(--accent); border-radius: var(--radius); background: var(--hover); }\n.banner[hidden] { display: none; }\n\n/* surfaces */")
print('ok')
