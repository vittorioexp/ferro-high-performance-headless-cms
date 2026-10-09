// Who is logged in and which model the server exposes.

import { api } from './api.js';
import { setCollections } from './schema.js';

export const session = { me: null, info: null };

let navListeners = [];
export const onModelChange = (fn) => navListeners.push(fn);
export const clearModelListeners = () => (navListeners = []);

export async function reloadCollections() {
  const r = await api.get('/collections');
  setCollections(r.data);
  navListeners.forEach((fn) => fn());
}

export async function loadContext() {
  const [me, info] = await Promise.all([api.get('/users/me'), api.get('/server/info')]);
  session.me = me.data;
  session.info = info.data;
  await reloadCollections();
}

export const isAdmin = () => !!session.me?.admin_access;
