// Extension registry. Extensions are ES modules placed in FERRO_EXTENSIONS_DIR;
// each one default-exports `register(ferro)` and uses the API below to add
// pages, field interfaces and item actions to the admin app.
//
//   // extensions/hello/index.js
//   export default function register(ferro) {
//     ferro.addPage({ id: 'hello', title: 'Hello', render(root) { root.append(ferro.dom.h('p', {}, 'Hi!')); } });
//     ferro.addInterface('stars', (field, value) => { ...; return { el, get: () => value }; });
//     ferro.addItemAction({ id: 'ping', label: 'Ping', collections: ['pages'], run: async ({ id }) => ferro.api.post(`/flows/...`) });
//   }
//
// This module has no DOM or network dependency so the form code can import it.

export const registry = {
  /** { id, title, render(root, ctx) } */
  pages: [],
  /** interface name -> (field, value, ctx) => { el, get() } */
  interfaces: {},
  /** { id, label, collections?, run(target, ctx) } */
  itemActions: [],
  /** loading report: { id, name, ok, error? } */
  loaded: [],
};

export function resetRegistry() {
  registry.pages.length = 0;
  registry.itemActions.length = 0;
  registry.loaded.length = 0;
  for (const k of Object.keys(registry.interfaces)) delete registry.interfaces[k];
}

const ID = /^[a-z0-9][a-z0-9_-]{0,63}$/;

/** The object handed to `register(ferro)`; `deps` carries api, dom helpers and the schema module. */
export function createExtensionApi(ext, deps) {
  const own = (id) => `${ext.id}:${id}`;
  return {
    version: 1,
    extension: { id: ext.id, name: ext.name, version: ext.version },
    api: deps.api,
    dom: deps.dom,
    schema: deps.schema,
    realtime: deps.realtime,
    onLeave: deps.onLeave,
    addPage(def) {
      if (!def || !ID.test(def.id || '') || typeof def.render !== 'function') throw new Error('addPage needs { id (a-z0-9_-), title, render(root, ctx) }');
      if (registry.pages.some((p) => p.id === def.id)) throw new Error(`page "${def.id}" already exists`);
      registry.pages.push({ id: def.id, title: def.title || def.id, render: def.render, extension: ext.id });
    },
    addInterface(name, factory) {
      if (!name || typeof factory !== 'function') throw new Error('addInterface needs (name, factory)');
      if (registry.interfaces[name]) throw new Error(`interface "${name}" already exists`);
      registry.interfaces[name] = factory;
    },
    addItemAction(def) {
      if (!def || !def.label || typeof def.run !== 'function') throw new Error('addItemAction needs { label, run(target, ctx), collections? }');
      registry.itemActions.push({ id: def.id || own(def.label), label: def.label, collections: def.collections || [], run: def.run, extension: ext.id });
    },
  };
}

export function itemActionsFor(collection) {
  return registry.itemActions.filter((a) => a.collections.length === 0 || a.collections.includes(collection));
}
