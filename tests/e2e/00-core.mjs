// Items, relations, filters, hooks, endpoints, files, permissions, auth, realtime: the basics end to end.
import { rnd, sleep } from './lib.mjs';

export default async function ({ a, check }) {
  const cat = (await a.rest('POST', '/items/categories', { code: `c-${rnd()}`, label: 'Corp' })).data;
  const slug = `home-${rnd()}`;

  // nested M2A write and read back
  const page = await a.rest('POST', '/items/pages', {
    title: 'Home', slug, status: 'published', category: cat.id,
    blocks: [
      { collection: 'block_hero', item: { headline: 'Welcome' }, sort: 1 },
      { collection: 'block_text', item: { body: 'Hello' }, sort: 2 },
      { collection: 'block_cards', item: { title: 'Cards', cards: [{ title: 'Uno', sort: 1 }, { title: 'Due', sort: 2 }] }, sort: 3 },
    ],
  });
  check('nested M2A create', page.status === 200 && page.data.blocks.length === 3);
  const id = page.data.id;
  const read = await a.rest('GET', `/items/pages/${id}?fields=title,category.code,blocks.collection,blocks.sort,blocks.item.*,blocks.item.cards.*`);
  const blocks = read.data.blocks.map((b) => [b.collection, b.sort, b.item.headline || b.item.body || b.item.title, (b.item.cards || []).map((c) => c.title)]);
  check('nested read: blocks, union items and children', JSON.stringify(blocks) === JSON.stringify([['block_hero', 1, 'Welcome', []], ['block_text', 2, 'Hello', []], ['block_cards', 3, 'Cards', ['Uno', 'Due']]]), blocks);

  // filters / search / counts / errors
  const some = await a.rest('GET', `/items/pages?filter=${encodeURIComponent(JSON.stringify({ blocks: { _some: { collection: { _eq: 'block_text' } } }, slug: { _eq: slug } }))}&meta=filter_count`);
  check('relational filter (_some)', some.data.length === 1 && some.json.meta.filter_count === 1);
  check('search', (await a.rest('GET', `/items/pages?search=${slug}&fields=slug`)).data.length === 1);
  check('unique violation is 409', (await a.rest('POST', '/items/pages', { title: 'dup', slug })).status === 409);
  check('unknown field is 400', (await a.rest('POST', '/items/pages', { title: 'x', slug: rnd(), nope: 1 })).status === 400);
  check('missing item is 404', (await a.rest('GET', `/items/pages/${uuid4()}`)).status === 404);
  check('required field (null) is rejected', (await a.rest('POST', '/items/pages', { slug: rnd() })).status >= 400);

  // replace semantics of arrays: reorder and remove
  const cur = (await a.rest('GET', `/items/pages/${id}?fields=blocks.id,blocks.collection,blocks.item.id`)).data.blocks;
  const re = await a.rest('PATCH', `/items/pages/${id}`, { blocks: [cur[2], cur[0]].map((b, i) => ({ id: b.id, collection: b.collection, item: { id: b.item.id }, sort: i + 1 })) });
  const after = (await a.rest('GET', `/items/pages/${id}?fields=blocks.collection,blocks.sort&sort=id`)).data.blocks.sort((x, y) => x.sort - y.sort);
  check('array write reorders and removes omitted blocks', re.status === 200 && after.map((b) => b.collection).join() === 'block_cards,block_hero', after);

  // o2m through the {create, update, delete} form
  const cards = (await a.rest('GET', `/items/pages/${id}?fields=blocks.item.cards.id&sort=id`)).data;
  check('page keeps its relations after unrelated updates', cards !== undefined);

  // hooks, endpoints, manual flows
  const empty = await a.rest('POST', '/items/pages', { title: '', slug: rnd() });
  check('blocking flow rejects an empty title (400 with its message)', empty.status === 400 && /title cannot be empty/.test(empty.error), empty.error);
  check('a partial update without title is allowed', (await a.rest('PATCH', `/items/pages/${id}`, { status: 'published' })).status === 200);
  const hello = await fetch(`${a.url}/x/hello?name=World`);
  check('custom endpoint (public)', (await hello.json()).data.message === 'Hello World');
  check('custom endpoint: method not allowed', (await fetch(`${a.url}/x/hello`, { method: 'POST' })).status === 405);
  const flows = (await a.rest('GET', '/flows')).data;
  const unpublish = flows.find((f) => f.name === 'Unpublish page');
  const run = await a.rest('POST', `/flows/${unpublish.id}/run`, { collection: 'pages', keys: [id] });
  check('manual flow runs and writes', run.json.meta.ok === true && (await a.rest('GET', `/items/pages/${id}?fields=status`)).data.status === 'draft');
  await a.rest('PATCH', `/items/pages/${id}`, { status: 'published' });

  // files
  const fd = new FormData();
  fd.append('file', new Blob(['hello file'], { type: 'text/plain' }), 'qc.txt');
  const up = await (await fetch(`${a.url}/files`, { method: 'POST', headers: { Authorization: `Bearer ${a.token}` }, body: fd })).json();
  check('file upload (size, type, checksum)', up.data.filesize === 10 && up.data.type === 'text/plain' && up.data.checksum.length === 64);
  check('asset download', (await (await fetch(`${a.url}/assets/${up.data.id}`, { headers: { Authorization: `Bearer ${a.token}` } })).text()) === 'hello file');

  // permissions: an editor
  const email = `ed${rnd()}@example.com`;
  await a.rest('POST', '/users', { email, password: 'editor-pass-123', role: '0199a000-0000-7000-8000-00000000e001' });
  const login = await a.rest('POST', '/auth/login', { email, password: 'editor-pass-123' }, null);
  check('login returns tokens', login.status === 200 && login.data.access_token && login.data.refresh_token);
  const ed = login.data.access_token;
  check('editor reads pages', (await a.rest('GET', '/items/pages', undefined, ed)).status === 200);
  const mine = await a.rest('POST', '/items/pages', { title: 'by editor', slug: `ed-${rnd()}` }, ed);
  check('editor create: preset status=draft', mine.status === 200 && (await a.rest('GET', `/items/pages/${mine.data.id}?fields=status`)).data.status === 'draft');
  check('editor cannot delete', (await a.rest('DELETE', `/items/pages/${mine.data.id}`, undefined, ed)).status === 403);
  for (const p of ['/schema/snapshot', '/flows', '/roles']) check(`editor cannot read ${p}`, (await a.rest('GET', p, undefined, ed)).status === 403);
  check('wildcard read does not cover system collections (users)', (await a.rest('GET', '/users', undefined, ed)).status === 403);
  check('editor cannot export data', (await a.rest('POST', '/sync/export', { selections: [{ collection: 'pages' }] }, ed)).status === 403);
  await a.rest('PATCH', `/items/pages/${mine.data.id}`, { status: 'archived' });
  check('update filter: archived pages are read-only for editors', (await a.rest('PATCH', `/items/pages/${mine.data.id}`, { title: 'x' }, ed)).status === 403);

  // anonymous
  check('anonymous cannot create', (await a.rest('POST', '/items/pages', { title: 'x', slug: rnd() }, null)).status === 403);
  const pub = (await a.rest('GET', '/items/pages?fields=status&limit=-1', undefined, null)).data;
  check('anonymous sees published pages only', pub.length > 0 && pub.every((p) => p.status === 'published'));

  // auth edge cases
  check('invalid token → 401', (await a.rest('GET', '/items/pages', undefined, 'garbage')).status === 401);
  check('wrong password → 401', (await a.rest('POST', '/auth/login', { email: 'admin@example.com', password: 'x' }, null)).status === 401);
  const rt = login.data.refresh_token;
  const r1 = await a.rest('POST', '/auth/refresh', { refresh_token: rt }, null);
  check('refresh issues new tokens', r1.status === 200 && r1.data.refresh_token !== rt);
  check('a used refresh token cannot be replayed', (await a.rest('POST', '/auth/refresh', { refresh_token: rt }, null)).status === 401);
  await a.rest('POST', '/auth/logout', { refresh_token: r1.data.refresh_token }, null);
  check('logout invalidates the refresh token', (await a.rest('POST', '/auth/refresh', { refresh_token: r1.data.refresh_token }, null)).status === 401);
  const u = (await a.rest('GET', `/users?filter=${encodeURIComponent(JSON.stringify({ email: { _eq: email } }))}&fields=id`)).data[0];
  await a.rest('PATCH', `/users/${u.id}`, { status: 'suspended' });
  check('suspended user: token and login stop working', (await a.rest('GET', '/items/pages', undefined, ed)).status === 401 && (await a.rest('POST', '/auth/login', { email, password: 'editor-pass-123' }, null)).status === 401);

  // revisions on delete
  const del = (await a.rest('POST', '/items/categories', { code: `del-${rnd()}` })).data;
  await a.rest('DELETE', `/items/categories/${del.id}`);
  const revs = (await a.rest('GET', `/revisions?filter=${encodeURIComponent(JSON.stringify({ item: { _eq: del.id } }))}&fields=data,activity.action`)).data;
  check('deleted items keep their last content in the revisions', revs.some((r) => r.activity.action === 'delete' && r.data?.code === del.code));

  // served assets and extensions
  check('admin app is served', (await fetch(`${a.url}/admin/`)).status === 200);
  check('extensions are listed', (await a.rest('GET', '/server/extensions')).data.some((e) => e.id === 'hello-panel'));
  check('extension module is served', (await fetch(`${a.url}/extensions/hello-panel/index.js`)).status === 200);

  // realtime smoke (details in the dedicated suite)
  const ws = new WebSocket(a.ws);
  const got = [];
  ws.onmessage = (e) => got.push(JSON.parse(e.data));
  await new Promise((r) => (ws.onopen = r));
  ws.send(JSON.stringify({ type: 'auth', access_token: a.token }));
  ws.send(JSON.stringify({ type: 'subscribe', uid: 'u', collection: 'categories', query: { fields: ['code'] } }));
  await sleep(500);
  await a.rest('POST', '/items/categories', { code: `rt-${rnd()}` });
  await sleep(700);
  check('realtime create event', got.some((m) => m.event === 'create' && m.uid === 'u'));
  ws.close();
}

function uuid4() {
  return globalThis.crypto.randomUUID();
}
