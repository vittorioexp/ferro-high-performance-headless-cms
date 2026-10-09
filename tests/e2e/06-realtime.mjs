// Realtime over WebSocket: subscriptions, permissions per client, validation, limits, several instances.
import { Env, rnd, sleep } from './lib.mjs';

function client(url) {
  const ws = new WebSocket(url);
  const inbox = [];
  const waiters = [];
  ws.onmessage = (e) => {
    const m = JSON.parse(e.data);
    inbox.push(m);
    for (const w of [...waiters]) if (w.pred(m)) { waiters.splice(waiters.indexOf(w), 1); w.resolve(m); }
  };
  const opened = new Promise((res, rej) => { ws.onopen = res; ws.onerror = () => rej(new Error('ws error')); });
  return {
    ws, inbox, opened,
    send: (o) => ws.send(JSON.stringify(o)),
    next: (pred, ms = 4000) => {
      const hit = inbox.find(pred);
      if (hit) return Promise.resolve(hit);
      return new Promise((resolve) => {
        const t = setTimeout(() => resolve(null), ms);
        waiters.push({ pred, resolve: (m) => { clearTimeout(t); resolve(m); } });
      });
    },
    quiet: async (pred, ms = 1200) => { await sleep(ms); return !inbox.some(pred); },
  };
}

export default async function ({ a, check }) {
  const c = client(a.ws);
  await c.opened;
  c.send({ type: 'auth', access_token: a.token });
  check('auth accepted', (await c.next((m) => m.type === 'auth'))?.status === 'ok');
  c.send({ type: 'subscribe', uid: 'p', collection: 'pages', query: { fields: ['title', 'slug', 'status'] } });
  check('subscribe acknowledged', (await c.next((m) => m.event === 'init' && m.uid === 'p')) !== null);

  const slug = `rt-${rnd()}`;
  const created = await a.rest('POST', '/items/pages', { title: 'Realtime', slug, status: 'draft' });
  const id = created.data.id;
  const evCreate = await c.next((m) => m.event === 'create' && m.uid === 'p');
  check('create event carries exactly the requested fields', Object.keys(evCreate?.data?.[0] ?? {}).sort().join() === 'id,slug,status,title' && evCreate.data[0].slug === slug);
  await a.rest('PATCH', `/items/pages/${id}`, { title: 'Realtime v2' });
  check('update event carries the new value', (await c.next((m) => m.event === 'update' && m.uid === 'p'))?.data?.[0]?.title === 'Realtime v2');
  await a.rest('DELETE', `/items/pages/${id}`);
  check('delete event carries the id', JSON.stringify((await c.next((m) => m.event === 'delete' && m.uid === 'p'))?.data) === JSON.stringify([id]));

  // filters and event types
  c.send({ type: 'subscribe', uid: 'pub', collection: 'pages', event: 'create', query: { fields: 'id,status', filter: { status: { _eq: 'published' } } } });
  await c.next((m) => m.event === 'init' && m.uid === 'pub');
  await a.rest('POST', '/items/pages', { title: 'D', slug: `rt-d-${rnd()}`, status: 'draft' });
  check('filter: a draft does not reach a "published only" subscription', await c.quiet((m) => m.uid === 'pub' && m.event === 'create'));
  const pub = (await a.rest('POST', '/items/pages', { title: 'P', slug: `rt-p-${rnd()}`, status: 'published' })).data;
  check('filter: a published page does', (await c.next((m) => m.uid === 'pub' && m.event === 'create'))?.data?.[0]?.id === pub.id);
  await a.rest('PATCH', `/items/pages/${pub.id}`, { title: 'P2' });
  check('event filter: a create-only subscription ignores updates', await c.quiet((m) => m.uid === 'pub' && m.event === 'update'));
  c.send({ type: 'unsubscribe', uid: 'pub' });
  check('unsubscribe acknowledged', (await c.next((m) => m.event === 'unsubscribe'))?.removed === true);
  await a.rest('POST', '/items/pages', { title: 'after', slug: `rt-a-${rnd()}`, status: 'published' });
  check('no event after unsubscribing', await c.quiet((m) => m.uid === 'pub' && m.event === 'create' && m.data?.[0]?.id !== pub.id));

  // validation
  for (const [uid, msg, code] of [
    ['b1', { collection: 'nope' }, 'NOT_FOUND'],
    ['b2', { collection: 'ferro_users' }, null],
    ['b3', { collection: 'pages', query: { fields: ['nonexistent'] } }, 'INVALID_PAYLOAD'],
    ['b4', { collection: 'pages', query: { filter: { nonexistent: { _eq: 1 } } } }, null],
    ['b5', { collection: 'pages', event: 'explode' }, 'INVALID_PAYLOAD'],
  ]) {
    c.send({ type: 'subscribe', uid, ...msg });
    const e = await c.next((m) => m.type === 'error' && m.uid === uid);
    check(`rejected at subscribe time: ${JSON.stringify(msg).slice(0, 60)}`, !!e && (code === null || e.code === code), e?.code);
  }
  c.ws.send('not json');
  check('malformed frame → error', (await c.next((m) => m.type === 'error' && !m.uid))?.code === 'INVALID_PAYLOAD');
  c.send({ type: 'ping' });
  check('ping → pong', (await c.next((m) => m.type === 'pong')) !== null);
  c.send({ type: 'whatever' });
  check('unknown message type → error', (await c.next((m) => m.type === 'error' && m.message === 'unknown message type')) !== null);
  for (let i = 0; i < 60; i++) c.send({ type: 'subscribe', uid: `many-${i}`, collection: 'categories' });
  await sleep(2000);
  check('at most 50 subscriptions per connection', c.inbox.some((m) => m.code === 'LIMIT'));
  c.send({ type: 'subscribe', uid: 'p', collection: 'pages', query: { fields: ['title'] } });
  check('re-subscribing with an existing uid replaces it (also when the limit is reached)', (await c.next((m) => m.event === 'init' && m.uid === 'p' && c.inbox.filter((x) => x.uid === 'p' && x.event === 'init').length >= 2, 2500)) !== null);

  // anonymous clients: public policy
  const anon = client(a.ws);
  await anon.opened;
  anon.send({ type: 'subscribe', uid: 'p', collection: 'pages', query: { fields: ['title', 'status'] } });
  check('anonymous may subscribe to what the public policy reads', (await anon.next((m) => m.event === 'init')) !== null);
  const sd = `rt-an-${rnd()}`;
  await a.rest('POST', '/items/pages', { title: 'secret draft', slug: sd, status: 'draft' });
  check('anonymous does not receive drafts', await anon.quiet((m) => m.event === 'create'));
  await a.rest('POST', '/items/pages', { title: 'visible', slug: `${sd}-p`, status: 'published' });
  check('anonymous receives published pages', (await anon.next((m) => m.event === 'create'))?.data?.[0]?.title === 'visible');
  anon.send({ type: 'subscribe', uid: 's', collection: 'settings' });
  check('anonymous cannot subscribe to a collection it cannot read', (await anon.next((m) => m.uid === 's'))?.type === 'error');

  // identity: token in the URL, suspended users, bad tokens
  const email = `rt${rnd()}@example.com`;
  await a.rest('POST', '/users', { email, password: 'editor-pass-123', role: '0199a000-0000-7000-8000-00000000e001' });
  const tok = (await a.rest('POST', '/auth/login', { email, password: 'editor-pass-123' }, null)).data.access_token;
  const ed = client(`${a.ws}?access_token=${tok}`);
  await ed.opened;
  check('token in the query string accepted', (await ed.next((m) => m.type === 'auth'))?.status === 'ok');
  ed.send({ type: 'subscribe', uid: 'e', collection: 'pages', query: { fields: ['title'] } });
  await ed.next((m) => m.event === 'init');
  await a.rest('POST', '/items/pages', { title: 'for editors', slug: `rt-e-${rnd()}` });
  check('editor receives the event', (await ed.next((m) => m.event === 'create'))?.data?.[0]?.title === 'for editors');
  const uid = (await a.rest('GET', `/users?filter=${encodeURIComponent(JSON.stringify({ email: { _eq: email } }))}&fields=id`)).data[0].id;
  await a.rest('PATCH', `/users/${uid}`, { status: 'suspended' });
  await a.rest('POST', '/items/pages', { title: 'after suspension', slug: `rt-e2-${rnd()}` });
  check('a suspended user is disconnected at the next event', (await ed.next((m) => m.type === 'auth' && m.status === 'error'))?.code === 'UNAUTHORIZED');
  const bad = client(`${a.ws}?access_token=garbage`);
  await bad.opened.catch(() => {});
  check('an invalid token is refused', (await bad.next((m) => m.type === 'auth' && m.status === 'error'))?.code === 'UNAUTHORIZED');

  // permissions are applied when the data is read for each client (field rules + row rules)
  const docs = `qrt_${rnd()}`;
  await a.rest('POST', '/collections', { name: docs, fields: [{ name: 'title', type: 'string' }, { name: 'secret', type: 'string' }] });
  const polId = globalThis.crypto.randomUUID(), roleId = globalThis.crypto.randomUUID();
  await a.rest('PUT', `/policies/${polId}`, { name: 'RT', app_access: true, permissions: [{ collection: docs, action: 'read', fields: ['id', 'title'], filter: { title: { _starts_with: 'ok' } } }] });
  await a.rest('PUT', `/roles/${roleId}`, { name: 'RT', policies: [polId] });
  const mail = `rt2${rnd()}@example.com`;
  await a.rest('POST', '/users', { email: mail, password: 'editor-pass-123', role: roleId });
  const t2 = (await a.rest('POST', '/auth/login', { email: mail, password: 'editor-pass-123' }, null)).data.access_token;
  const lim = client(a.ws);
  await lim.opened;
  lim.send({ type: 'auth', access_token: t2 });
  lim.send({ type: 'subscribe', uid: 'l', collection: docs, query: { fields: ['title', 'secret'] } });
  await lim.next((m) => m.event === 'init');
  await a.rest('POST', `/items/${docs}`, { title: 'not for you', secret: 'S1' });
  check('row rule: an item outside the permission filter is not delivered', await lim.quiet((m) => m.event === 'create'));
  await a.rest('POST', `/items/${docs}`, { title: 'ok item', secret: 'S2' });
  const got = await lim.next((m) => m.event === 'create');
  check('field rule: the secret field is stripped from what is delivered', got?.data?.[0]?.title === 'ok item' && !('secret' in got.data[0]) && !JSON.stringify(lim.inbox).includes('S2'), got?.data);

  // GraphQL and flows write through the same path
  const g = client(a.ws);
  await g.opened;
  g.send({ type: 'auth', access_token: a.token });
  g.send({ type: 'subscribe', uid: 'g', collection: 'categories', query: { fields: ['code'] } });
  await g.next((m) => m.event === 'init');
  await a.graphql(`mutation { create_categories_item(data: { code: "rt-${rnd()}" }) { id } }`);
  check('GraphQL mutations notify', (await g.next((m) => m.uid === 'g' && m.event === 'create')) !== null);

  // rollback: nothing is published when the write fails
  const seen = g.inbox.length;
  const dup = `dup-${rnd()}`;
  await a.rest('POST', '/items/categories', { code: dup });
  await sleep(500);
  const after = g.inbox.length;
  const r409 = await a.rest('POST', '/items/categories', { code: dup });
  await sleep(800);
  check('a failed (rolled back) write publishes nothing', r409.status === 409 && g.inbox.length === after && after === seen + 1);

  // a second instance on the same database receives what the first one writes
  const a2 = new Env('a2', 18057, a.db, { FERRO_SECRET: a.env.FERRO_SECRET }, { reuseDb: true, token: a.token });
  await a2.start();
  const c2 = client(a2.ws);
  await c2.opened;
  c2.send({ type: 'auth', access_token: a.token });
  c2.send({ type: 'subscribe', uid: 'x', collection: 'categories', query: { fields: ['code'] } });
  await c2.next((m) => m.event === 'init');
  const code = `x-${rnd()}`;
  await a.rest('POST', '/items/categories', { code });
  check('a write on instance 1 reaches a client of instance 2', (await c2.next((m) => m.event === 'create'))?.data?.[0]?.code === code);
  const code2 = `y-${rnd()}`;
  await a2.rest('POST', '/items/categories', { code: code2 });
  check('…and the other way round', (await g.next((m) => m.event === 'create' && m.data?.[0]?.code === code2)) !== null);

  // schema changes propagate between instances too
  const hashOf = async (env) => (await env.rest('GET', '/server/info')).data.schema_hash;
  await a.rest('POST', `/fields/${docs}`, { name: 'extra', type: 'string' });
  await sleep(1500);
  check('the other instance reloads the schema on its own', (await hashOf(a)) === (await hashOf(a2)));
  await a2.stop();
  for (const x of [c, anon, ed, lim, g, c2]) x.ws.close();
}
