import assert from 'node:assert/strict';
import { test } from 'node:test';

import { COLLECTIONS } from './fixture.mjs';
import { FakeEl, findAll, fire, install } from './fakedom.mjs';

const body = install();
const sockets = [];
globalThis.WebSocket = class {
  constructor(url) {
    this.url = url;
    this.sent = [];
    this.readyState = 0;
    sockets.push(this);
  }
  send(d) {
    this.sent.push(JSON.parse(d));
  }
  close() {
    this.readyState = 3;
  }
};
globalThis.location = { hash: '', reload() {} };
const realSetTimeout = globalThis.setTimeout;
globalThis.setTimeout = (fn, ms) => {
  const t = realSetTimeout(fn, ms);
  t.unref?.();
  return t;
};

const wait = (ms) => new Promise((r) => realSetTimeout(r, ms));
const FLOW = {
  id: '0199a000-0000-7000-8000-0000000a0001',
  name: 'Notify',
  trigger: { type: 'event', events: ['pages.items.update'] },
  operations: [{ key: 'notify', type: 'request', options: { method: 'POST', url: 'https://example.com/hook' } }],
};
const calls = [];
const PAGE = { id: 'p1', title: 'Home', slug: 'home', status: 'draft', category: null, cover: null, date_updated: '2026-01-01T10:00:00Z', blocks: [], tags: [] };

function respond(method, url) {
  const u = new URL(url, 'http://x');
  const p = u.pathname;
  const ok = (data, meta) => ({ data, ...(meta ? { meta } : {}) });
  if (p === '/collections' && method === 'GET') return ok(COLLECTIONS);
  if (p === '/items/pages' && method === 'GET') return ok([PAGE], { filter_count: 1 });
  if (p === '/items/pages/p1' && method === 'GET') return ok(PAGE);
  if (p === '/items/pages/p1' && method === 'PATCH') return ok(PAGE);
  if (p.startsWith('/items/')) return ok([]);
  if (p === '/roles') return ok([{ id: '00000000-0000-7000-8000-000000000002', name: 'Administrator', policies: ['00000000-0000-7000-8000-000000000001'] }]);
  if (p === '/policies') return ok([{ id: '00000000-0000-7000-8000-000000000001', name: 'Administrator', admin_access: true, app_access: true, permissions: [] }]);
  if (p === '/users') return ok([{ id: 'u1', email: 'a@b.c', role: '00000000-0000-7000-8000-000000000002', status: 'active' }]);
  if (p === '/files') return ok([{ id: 'f1', title: 'Logo', filename_download: 'logo.png', type: 'image/png', filesize: 2048 }], { filter_count: 1 });
  if (p === '/folders') return ok([{ id: 'd1', name: 'Docs' }]);
  if (p === '/activity') return ok([{ timestamp: '2026-01-01T10:00:00Z', user: { email: 'a@b.c' }, action: 'update', collection: 'pages', item: 'p1', origin: 'api' }], { filter_count: 1 });
  if (p === '/schema/history') return ok([{ applied_at: '2026-01-01T10:00:00Z', from_hash: 'aaaaaaaaaaaa', to_hash: 'bbbbbbbbbbbb', plan: { changes: [{ op: 'add_field', collection: 'pages', field: { name: 'x' } }] } }]);
  if (p === '/revisions') return ok([]);
  if (p === '/flows' && method === 'GET') return ok([FLOW]);
  if (p === '/flow-actions') return ok([{ id: 'f-manual', name: 'Reprocess' }]);
  if (p === `/flows/${FLOW.id}` && method === 'PUT') return { data: FLOW, meta: { applied: !u.searchParams.get('dry_run'), changes: ['~ flow Notify'], sql: [] } };
  if (p === '/flows/f-manual/run' && method === 'POST') return { data: null, meta: { ok: true, error: null, log: [] } };
  if (p === '/graphql' && method === 'POST') return { data: { pages: [{ id: 'p1' }] } };
  if (p.startsWith('/fields/') || p.startsWith('/collections')) {
    return { data: {}, meta: { applied: !u.searchParams.get('dry_run'), changes: ['+ field pages.subtitle (string)'], sql: ['ALTER TABLE "pages" ADD COLUMN "subtitle" varchar(255)'] } };
  }
  throw new Error(`unmocked ${method} ${url}`);
}

globalThis.fetch = async (url, init = {}) => {
  const method = init.method || 'GET';
  calls.push({ method, url: String(url), body: init.body });
  const json = respond(method, String(url));
  return { ok: true, status: 200, text: async () => JSON.stringify(json) };
};

const S = await import('../js/schema.js');
const { session } = await import('../js/session.js');
const { contentList, contentItem } = await import('../js/views/content.js');
const { modelHome, modelCollection } = await import('../js/views/model.js');
const { accessHome } = await import('../js/views/access.js');
const { filesView } = await import('../js/views/files.js');
const { usersView } = await import('../js/views/users.js');
const { graphqlView, sampleQuery } = await import('../js/views/graphql.js');
const { flowsView } = await import('../js/views/flows.js');
const { schemaView, activityView } = await import('../js/views/system.js');

S.setCollections(COLLECTIONS);
session.info = { environment: 'dev', schema_hash: 'abcdef012345', collections: COLLECTIONS.length };

const render = async (view, params) => {
  const root = new FakeEl('div');
  await view(root, params);
  return root;
};
const buttonByText = (root, text) => findAll(root, (n) => n.tagName === 'BUTTON' && n.text === text)[0];

test('content list shows rows and asks only for scalar fields', async () => {
  calls.length = 0;
  const root = await render(contentList, { collection: 'pages' });
  assert.match(root.text, /Home/);
  assert.match(root.text, /1 items/);
  const req = decodeURIComponent(calls.find((c) => c.url.startsWith('/items/pages?')).url);
  assert.ok(req.includes('fields=id,title,slug,status'), req);
  assert.ok(!req.includes('blocks'), 'aliases must not be requested in listings');
});

test('item editor loads relations and saves with PATCH', async () => {
  calls.length = 0;
  const root = await render(contentItem, { collection: 'pages', id: 'p1' });
  const get = calls.find((c) => c.url.startsWith('/items/pages/p1'));
  assert.ok(decodeURIComponent(get.url).includes('blocks.item.cards.*'));
  const save = buttonByText(root, 'Save');
  await fire(save, 'click');
  await wait(20);
  const patch = calls.find((c) => c.method === 'PATCH');
  assert.ok(patch, 'PATCH sent');
});

test('model, access, files, users, environment and activity views render', async () => {
  assert.match((await render(modelHome)).text, /pages_blocks/);
  assert.match((await render(modelCollection, { name: 'pages' })).text, /Fields/);
  assert.match((await render(accessHome)).text, /Administrator/);
  assert.match((await render(filesView)).text, /Logo/);
  assert.match((await render(usersView)).text, /a@b\.c/);
  const env = await render(schemaView);
  assert.match(env.text, /abcdef012345/);
  assert.match(env.text, /aaaaaaaa → bbbbbbbb/);
  assert.match((await render(activityView)).text, /update/);
});

test('adding a field: dry-run preview first, then apply', async () => {
  const root = await render(modelCollection, { name: 'pages' });
  await fire(buttonByText(root, '+ New field'), 'click');
  const overlay = body.children.at(-1);
  const name = findAll(overlay, (n) => n.tagName === 'INPUT' && n.attrs.placeholder === 'field_name')[0];
  name.value = 'subtitle';
  await fire(name, 'input');

  calls.length = 0;
  const save = buttonByText(overlay, 'Save');
  save.handlers.click[0]({ target: save }); // resolves only after the user confirms the plan
  await wait(30);

  assert.equal(calls.length, 1);
  assert.equal(calls[0].method, 'POST');
  assert.match(calls[0].url, /\/fields\/pages\?.*dry_run=true/);
  const preview = body.children.at(-1);
  assert.match(preview.text, /\+ field pages\.subtitle/);
  assert.match(preview.text, /ALTER TABLE/);

  const apply = buttonByText(preview, 'Apply');
  await fire(apply, 'click');
  await wait(30);
  const posts = calls.filter((c) => c.method === 'POST');
  assert.equal(posts.length, 2);
  assert.ok(!posts[1].url.includes('dry_run'), posts[1].url);
  assert.ok(calls.some((c) => c.method === 'GET' && c.url === '/collections'), 'model reloaded after apply');
});

test('graphql playground suggests a query and runs it', async () => {
  const q = sampleQuery();
  assert.match(q, /\(limit: 5\)/);
  assert.ok(q.includes('    id'));
  const root = await render(graphqlView);
  calls.length = 0;
  await fire(buttonByText(root, 'Run'), 'click');
  await wait(20);
  const post = calls.find((c) => c.method === 'POST' && c.url === '/graphql');
  assert.ok(post, 'POST /graphql');
  const pre = findAll(root, (n) => n.tagName === 'PRE')[0];
  assert.match(pre.textContent, /"pages"/);
});

test('item editor shows manual flow buttons and runs them', async () => {
  const root = await render(contentItem, { collection: 'pages', id: 'p1' });
  const btn = buttonByText(root, '▶ Reprocess');
  assert.ok(btn, 'manual flow button');
  calls.length = 0;
  await fire(btn, 'click');
  await wait(20);
  const run = calls.find((c) => c.method === 'POST' && c.url === '/flows/f-manual/run');
  assert.ok(run, 'POST /flows/f-manual/run');
  assert.match(String(run.body), /"keys":\["p1"\]/);
});

test('flows: list, edit, preview the plan, then save', async () => {
  const root = await render(flowsView);
  assert.match(root.text, /Notify/);
  assert.match(root.text, /after pages\.items\.update/);
  const row = findAll(root, (n) => n.tagName === 'TR' && n.text.includes('Notify'))[0];
  await fire(row, 'click');
  const overlay = body.children.at(-1);
  const save = buttonByText(overlay, 'Save');
  calls.length = 0;
  save.handlers.click[0]({ target: save });
  await wait(30);
  const dry = calls.find((c) => c.method === 'PUT');
  assert.ok(dry.url.includes(`/flows/${FLOW.id}`) && dry.url.includes('dry_run=true'), dry.url);
  const sent = JSON.parse(dry.body);
  assert.equal(sent.operations[0].options.url, 'https://example.com/hook', 'options survive the editor untouched');
  assert.equal(sent.trigger.type, 'event');
  assert.ok(!('active' in sent) && !('run_as' in sent), 'defaults are not stored');
  const preview = body.children.at(-1);
  assert.match(preview.text, /~ flow Notify/);
  await fire(buttonByText(preview, 'Apply'), 'click');
  await wait(30);
  assert.equal(calls.filter((c) => c.method === 'PUT').length, 2);
});

test('content list subscribes to realtime changes and shows a banner', async () => {
  (await import('../js/lifecycle.js')).runLeave(); // drop the subscriptions of the previous tests
  sockets.length = 0;
  const list = await render(contentList, { collection: 'pages' });
  const sock = sockets.at(-1);
  sock.readyState = 1;
  await sock.onopen();
  const sub = sock.sent.find((m) => m.type === 'subscribe');
  assert.equal(sub.collection, 'pages');
  assert.equal(sub.query.fields, 'id');
  const banner = findAll(list, (n) => n.className === 'banner')[0];
  assert.equal(banner.attrs.hidden, '', 'hidden until something changes');
  await sock.onmessage({ data: JSON.stringify({ type: 'subscription', event: 'create', uid: sub.uid, data: [{ id: 'x' }] }) });
  assert.match(banner.children[0].textContent, /added items/);
  assert.equal(banner.hidden, false);
});

test('item editor: another user changing the item shows a banner, our own save does not', async () => {
  const { runLeave } = await import('../js/lifecycle.js');
  runLeave();
  sockets.length = 0;
  const root = await render(contentItem, { collection: 'pages', id: 'p1' });
  const sock = sockets.at(-1);
  sock.readyState = 1;
  await sock.onopen();
  const sub = sock.sent.find((m) => m.type === 'subscribe');
  assert.deepEqual(sub.query.filter, { id: { _eq: 'p1' } });
  const banner = findAll(root, (n) => n.className === 'banner')[0];
  const event = (e, data) => sock.onmessage({ data: JSON.stringify({ type: 'subscription', event: e, uid: sub.uid, data }) });

  await event('update', [{ id: 'p1' }]);
  assert.match(banner.children[0].textContent, /changed by someone else/);

  banner.hidden = true;
  await event('delete', ['other-item']);
  assert.equal(banner.hidden, true, 'deleting another item is not our business');
  await event('delete', ['p1']);
  assert.match(banner.children[0].textContent, /was deleted/);

  // our own save echoes back through the socket: no banner
  banner.hidden = true;
  await fire(buttonByText(root, 'Save'), 'click');
  await wait(30);
  await event('update', [{ id: 'p1' }]);
  assert.equal(banner.hidden, true, 'the echo of our own save is ignored');
  runLeave();
  assert.equal(sock.readyState, 3, 'leaving the view closes the socket when nothing else is subscribed');
});
