// Loads the extensions listed by the server. A broken extension is reported in
// the console and in `registry.loaded`; it never prevents the app from starting.

import { api, baseUrl } from './api.js';
import * as dom from './dom.js';
import { createExtensionApi, registry } from './registry.js';
import { onLeave } from './lifecycle.js';
import { realtime } from './realtime.js';
import * as schema from './schema.js';

/**
 * @param {object} [opts]
 * @param {Array}  [opts.list]     extensions to load (default: GET /server/extensions)
 * @param {Function} [opts.importer] module loader (default: dynamic import)
 */
export async function loadExtensions({ list, importer = (url) => import(/* @vite-ignore */ url) } = {}) {
  let exts = list;
  if (!exts) {
    try {
      exts = (await api.get('/server/extensions')).data || [];
    } catch (err) {
      console.warn('Cannot list extensions:', err.message);
      return registry.loaded;
    }
  }
  for (const ext of exts) {
    try {
      const mod = await importer(/^https?:/.test(ext.url) ? ext.url : `${baseUrl()}${ext.url}`);
      const register = mod.default || mod.register;
      if (typeof register !== 'function') throw new Error('the module must export a default function register(ferro)');
      await register(createExtensionApi(ext, { api, dom, schema, realtime, onLeave }));
      registry.loaded.push({ id: ext.id, name: ext.name, ok: true });
    } catch (err) {
      console.error(`Extension "${ext.id}" failed to load:`, err);
      registry.loaded.push({ id: ext.id, name: ext.name, ok: false, error: err.message });
    }
  }
  return registry.loaded;
}
