// Ferro admin: shell, navigation and hash routing.

import { api, configure, logout, restoreSession } from './api.js';
import { loadExtensions } from './extensions.js';
import { registry, resetRegistry } from './registry.js';
import { add, button, h, replace } from './dom.js';
import * as S from './schema.js';
import { clearModelListeners, isAdmin, loadContext, onModelChange, session } from './session.js';
import { runLeave } from './lifecycle.js';
import { realtime } from './realtime.js';
import { matchRoute } from './util.js';
import { accessHome } from './views/access.js';
import { contentItem, contentList } from './views/content.js';
import { filesView } from './views/files.js';
import { flowsView } from './views/flows.js';
import { graphqlView } from './views/graphql.js';
import { loginView } from './views/login.js';
import { modelCollection, modelHome } from './views/model.js';
import { activityView, schemaView } from './views/system.js';
import { usersView } from './views/users.js';

const app = document.getElementById('app');

// API base: same origin by default; `?api=https://host` for development against another server.
const params = new URLSearchParams(location.search);
if (params.get('api')) sessionStorage.setItem('ferro_api', params.get('api'));
configure({ base: params.get('api') || sessionStorage.getItem('ferro_api') || '', onAuthLost: () => showLogin('Your session has expired.') });

async function home(root) {
  const cols = S.userCollections().filter((c) => !c.meta?.hidden);
  add(root, 
    h('div', { class: 'page-head' }, h('h1', {}, 'Content')),
    cols.length
      ? h('div', { class: 'tiles' }, cols.map((c) => h('a', { class: 'tile', href: `#/content/${encodeURIComponent(c.name)}` }, h('strong', {}, S.collectionLabel(c)), h('span', { class: 'muted' }, `${(c.fields || []).length} fields`))))
      : h('p', { class: 'muted' }, isAdmin() ? 'No collections yet. Create the first one in "Data model".' : 'You have no accessible collections.'),
  );
}

async function extensionPage(root, { id }) {
  const page = registry.pages.find((p) => p.id === id);
  if (!page) throw new Error(`Extension page "${id}" not found`);
  await page.render(root, { api, schema: S });
}

// [pattern, view, adminOnly]
const ROUTES = [
  ['/content', home, false],
  ['/content/:collection', contentList, false],
  ['/content/:collection/:id', contentItem, false],
  ['/files', filesView, false],
  ['/graphql', graphqlView, false],
  ['/ext/:id', extensionPage, false],
  ['/flows', flowsView, true],
  ['/users', usersView, true],
  ['/model', modelHome, true],
  ['/model/:name', modelCollection, true],
  ['/access', accessHome, true],
  ['/schema', schemaView, true],
  ['/activity', activityView, true],
];

let routeToken = 0;

async function route() {
  const main = document.getElementById('main');
  if (!main) return;
  runLeave();
  highlightNav();
  const path = location.hash.slice(1) || '/content';
  const match = matchRoute(path, ROUTES.map((r) => r[0]));
  main.replaceChildren();
  const container = h('div', { class: 'view' });
  main.append(container);
  if (!match) return container.append(h('p', {}, 'Page not found.'));
  const [, view, adminOnly] = ROUTES.find((r) => r[0] === match.pattern);
  if (adminOnly && !isAdmin()) return container.append(h('div', { class: 'error-box' }, 'Administrator access required.'));
  const token = ++routeToken;
  try {
    await view(container, match.params);
  } catch (err) {
    console.error(err);
    if (token === routeToken) replace(container, h('div', { class: 'error-box' }, err.message || String(err)));
  }
}

// ---- shell -------------------------------------------------------------------

let navLinks = [];

function highlightNav() {
  const here = location.hash || '#/content';
  for (const { el, href } of navLinks) el.className = here === href || here.startsWith(`${href}/`) ? 'active' : '';
}

function drawNav() {
  const nav = document.getElementById('nav');
  if (!nav) return;
  navLinks = [];
  const link = (label, href) => {
    const el = h('a', { href }, label);
    navLinks.push({ el, href });
    return el;
  };
  const section = (title, items) => (items.length ? h('div', { class: 'nav-section' }, h('h4', {}, title), items) : null);

  const groups = new Map();
  for (const c of S.userCollections().filter((x) => !x.meta?.hidden)) {
    const g = c.meta?.group || '';
    if (!groups.has(g)) groups.set(g, []);
    groups.get(g).push(link(S.collectionLabel(c), `#/content/${encodeURIComponent(c.name)}`));
  }
  const sections = [];
  for (const [g, items] of [...groups.entries()].sort((a, b) => a[0].localeCompare(b[0]))) sections.push(section(g ? S.collectionLabel(g) : 'Content', items));
  if (registry.pages.length) sections.push(section('Extensions', registry.pages.map((p) => link(p.title, `#/ext/${p.id}`))));
  sections.push(section('Library', [link('Files', '#/files'), link('GraphQL', '#/graphql')]));
  if (isAdmin()) {
    sections.push(
      section('Administration', [link('Users', '#/users'), link('Data model', '#/model'), link('Access control', '#/access'), link('Automations', '#/flows'), link('Environment', '#/schema'), link('Activity', '#/activity')]),
    );
  }
  replace(nav, ...sections.filter(Boolean));
  highlightNav();
}

function renderShell() {
  const who = session.me?.email || '';
  const env = session.info?.environment;
  replace(app, 
    h(
      'div',
      { class: 'shell' },
      h(
        'header',
        { class: 'topbar' },
        h('a', { class: 'brand', href: '#/content' }, 'Ferro'),
        env ? h('span', { class: `env env-${String(env).toLowerCase()}` }, env) : null,
        h('span', { class: 'spacer' }),
        h('span', { class: 'muted' }, who),
        button('Sign out', async () => {
          await logout();
          showLogin();
        }),
      ),
      h('nav', { id: 'nav', class: 'sidebar' }),
      h('main', { id: 'main' }),
    ),
  );
  clearModelListeners();
  onModelChange(drawNav);
  drawNav();
}

async function start() {
  await loadContext();
  resetRegistry();
  await loadExtensions();
  if (!session.me.app_access && !session.me.admin_access) {
    await logout();
    return showLogin('This account has no access to the admin app.');
  }
  renderShell();
  await route();
}

function showLogin(message) {
  routeToken++;
  runLeave();
  realtime.close();
  loginView(app, start, message);
}

window.addEventListener('hashchange', () => route());

(async () => {
  try {
    if (await restoreSession()) {
      await start();
      return;
    }
  } catch (err) {
    console.error(err);
  }
  showLogin();
})();

// exposed for debugging in the browser console
window.ferro = { api, session };
