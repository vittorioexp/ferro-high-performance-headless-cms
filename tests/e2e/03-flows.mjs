// Flows: outbound requests, the data chain, item operations, loops, secrets, async runs, chaining,
// caller permissions, blocking hooks on update/delete, disabled flows, SSRF protection.
import { createServer } from 'node:http';

import { rnd, sleep, uuid } from './lib.mjs';

async function waitFor(fn, ms = 9000) {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    const v = await fn();
    if (v) return v;
    await sleep(250);
  }
  return null;
}

export default async function ({ a, b, check }) {
  // ---- a local HTTP server that records what the flows send --------------------------------
  const hits = [];
  const echo = createServer((req, res) => {
    let raw = '';
    req.on('data', (c) => (raw += c));
    req.on('end', () => {
      let body = null;
      try {
        body = raw ? JSON.parse(raw) : null;
      } catch {
        body = raw;
      }
      hits.push({ method: req.method, url: req.url, headers: req.headers, body });
      if (req.url.startsWith('/fail')) {
        res.writeHead(500, { 'Content-Type': 'application/json' }).end('{"error":"boom"}');
      } else {
        res.writeHead(200, { 'Content-Type': 'application/json' }).end(JSON.stringify({ ok: true, n: 42, echoed: body }));
      }
    });
  });
  await new Promise((r) => echo.listen(19001, '127.0.0.1', r));
  const BASE = 'http://127.0.0.1:19001';
  const hitsFor = (path) => hits.filter((h) => h.url.startsWith(path));

  const coll = `qf_items_${rnd()}`;
  await a.rest('POST', '/collections', { name: coll, fields: [{ name: 'title', type: 'string' }, { name: 'n', type: 'integer' }, { name: 'status', type: 'string' }] });
  const putFlow = async (flow, env = a) => {
    const id = flow.id || uuid();
    const r = await env.rest('PUT', `/flows/${id}`, { ...flow, id });
    if (r.status !== 200) console.log('   (flow rejected:', r.error, ')');
    return { id, status: r.status, error: r.error };
  };
  const runs = async (id, env = a) => (await env.rest('GET', `/flows/${id}/runs`)).data;

  // ---- 1. outbound webhook with secrets and typed values from the data chain ------------------
  const hook = await putFlow({
    name: 'Outbound', trigger: { type: 'event', events: [`${coll}.items.create`] },
    operations: [{ key: 'call', type: 'request', options: { method: 'POST', url: `${BASE}/hook`, headers: { Authorization: 'Bearer {{ $env.PUBLISH_HOOK_TOKEN }}', 'X-Event': '{{ $trigger.event }}' }, body: { id: '{{ $trigger.key }}', n: '{{ $trigger.item.n }}', title: '{{ $trigger.item.title }}', note: 'item {{ $trigger.item.title }} has {{ $trigger.item.n }}' } } }],
  });
  check('flow saved', hook.status === 200, hook.error);
  const item = (await a.rest('POST', `/items/${coll}`, { title: 'first', n: 5 })).data;
  const h1 = await waitFor(() => hitsFor('/hook')[0]);
  check('the event reached the HTTP endpoint', !!h1);
  check('secret from the environment is injected into the header', h1?.headers.authorization === 'Bearer tok123');
  check('template in a header', h1?.headers['x-event'] === `${coll}.items.create`);
  check('a lone reference keeps its JSON type (number)', h1?.body?.n === 5 && h1.body.id === item.id, h1?.body);
  check('embedded references are interpolated', h1?.body?.note === 'item first has 5');
  const run1 = (await waitFor(async () => (await runs(hook.id)).find((r) => r.status === 'ok')))?.log;
  check('the run is recorded with its log', run1?.length === 1 && run1[0].key === 'call' && run1[0].ok === true);

  // ---- 2. the data chain between operations, item operations, no re-triggering ---------------------
  const before = hitsFor('/hook').length;
  const chain = await putFlow({
    name: 'Chain', trigger: { type: 'manual', collections: [coll] },
    operations: [
      { key: 'fetch', type: 'request', options: { method: 'GET', url: `${BASE}/json` }, resolve: 'save' },
      { key: 'save', type: 'item_update', options: { collection: coll, ids: '{{ $trigger.key }}', payload: { n: '{{ fetch.data.n }}', status: 'done' } }, resolve: 'read' },
      { key: 'read', type: 'item_read', options: { collection: coll, filter: { id: { _eq: '{{ $trigger.key }}' } } } },
    ],
    output: { n: '{{ read.0.n }}', echoed_status: '{{ fetch.status }}' },
  });
  const rc = await a.rest('POST', `/flows/${chain.id}/run`, { collection: coll, keys: [item.id] });
  check('manual flow: request → item_update → item_read', rc.json?.meta?.ok === true && rc.data?.n === 42 && rc.data?.echoed_status === 200, rc.json);
  check('the item was updated with the typed value', (await a.rest('GET', `/items/${coll}/${item.id}?fields=n,status`)).data.n === 42);
  await sleep(1500);
  check('writes made by a flow do not trigger other flows', hitsFor('/hook').length === before);

  // ---- 3. conditions, reject branches, fail_on_status ------------------------------------------------
  const cond = await putFlow({
    name: 'Branches', trigger: { type: 'manual' },
    operations: [
      { key: 'check', type: 'condition', options: { rule: { $trigger: { payload: { n: { _gt: 10 } } } } }, resolve: 'big', reject: 'small' },
      { key: 'big', type: 'transform', options: { value: 'big' } },
      { key: 'small', type: 'transform', options: { value: 'small' } },
    ],
    output: { size: '{{ $last }}' },
  });
  check('condition: matching rule → resolve branch', (await a.rest('POST', `/flows/${cond.id}/run`, { payload: { n: 50 } })).data?.size === 'big');
  check('condition: non-matching rule → reject branch', (await a.rest('POST', `/flows/${cond.id}/run`, { payload: { n: 1 } })).data?.size === 'small');
  const st = await putFlow({
    name: 'Status', trigger: { type: 'manual' },
    operations: [
      { key: 'call', type: 'request', options: { url: `${BASE}/fail`, fail_on_status: true }, reject: 'recover' },
      { key: 'recover', type: 'transform', options: { value: 'recovered' } },
    ],
  });
  const rs = await a.rest('POST', `/flows/${st.id}/run`, {});
  check('fail_on_status + reject branch recovers', rs.json.meta.ok === true && rs.data === 'recovered');
  const st2 = await putFlow({ name: 'Status2', trigger: { type: 'manual' }, operations: [{ key: 'call', type: 'request', options: { url: `${BASE}/fail`, fail_on_status: true } }] });
  const rs2 = await a.rest('POST', `/flows/${st2.id}/run`, {});
  check('without a reject branch the flow fails and says why', rs2.json.meta.ok === false && /HTTP 500/.test(rs2.json.meta.error), rs2.json.meta.error);
  check('a failed run is recorded as failed', (await runs(st2.id))[0]?.status === 'failed');

  // ---- 4. a flow that writes inside a transaction: failure rolls everything back --------------------------
  const countBefore = (await a.rest('GET', `/items/${coll}?limit=-1&fields=id`)).data.length;
  const atomic = await putFlow({
    name: 'Atomic', trigger: { type: 'manual' },
    operations: [
      { key: 'one', type: 'item_create', options: { collection: coll, payload: { title: 'created by the flow' } }, resolve: 'two' },
      { key: 'two', type: 'fail', options: { message: 'stop here' } },
    ],
  });
  const ra = await a.rest('POST', `/flows/${atomic.id}/run`, {});
  check('failing flow reports the error', ra.json.meta.ok === false && /stop here/.test(ra.json.meta.error));
  check('…and its earlier writes were rolled back', (await a.rest('GET', `/items/${coll}?limit=-1&fields=id`)).data.length === countBefore);

  // ---- 5. an event flow that creates items does not loop -----------------------------------------------
  const noLoop = await putFlow({
    name: 'Spawner', trigger: { type: 'event', events: [`${coll}.items.create`] },
    operations: [{ key: 'clone', type: 'condition', options: { rule: { $trigger: { payload: { title: { _eq: 'spawn' } } } } }, resolve: 'make' }, { key: 'make', type: 'item_create', options: { collection: coll, payload: { title: 'spawned child' } } }],
  });
  await a.rest('POST', `/items/${coll}`, { title: 'spawn' });
  await waitFor(async () => (await runs(noLoop.id)).length >= 1);
  await sleep(2500);
  const spawned = (await a.rest('GET', `/items/${coll}?filter=${encodeURIComponent(JSON.stringify({ title: { _eq: 'spawned child' } }))}&limit=-1`)).data.length;
  check('flow-made writes do not retrigger the flow (exactly one child)', spawned === 1, spawned);
  await a.rest('DELETE', `/flows/${noLoop.id}`);

  // ---- 6. webhook secret / methods / async ------------------------------------------------------------------
  const wh = await putFlow({
    name: 'Secret hook', trigger: { type: 'webhook', slug: `sec-${rnd()}`, methods: ['POST'], access: 'public', secret_env: 'FERRO_WEBHOOK_SECRET' },
    operations: [{ key: 'echo', type: 'transform', options: { value: { got: '{{ $trigger.body.x }}', q: '{{ $trigger.query.q }}', method: '{{ $trigger.method }}' } } }],
    output: { received: '{{ echo }}' },
  });
  const slug = (await a.rest('GET', '/flows')).data.find((f) => f.id === wh.id).trigger.slug;
  const call = (headers, method = 'POST') => fetch(`${a.url}/x/${slug}?q=7`, { method, headers: { 'Content-Type': 'application/json', ...headers }, body: method === 'GET' ? undefined : JSON.stringify({ x: 'hello' }) });
  check('secret required: missing → 403', (await call({})).status === 403);
  check('secret required: wrong → 403', (await call({ 'X-Ferro-Secret': 'nope' })).status === 403);
  const good = await call({ 'X-Ferro-Secret': 's3cret' });
  const goodBody = (await good.json()).received;
  check('correct secret → 200 with body, query and method in the data chain', good.status === 200 && goodBody.got === 'hello' && goodBody.q === '7' && goodBody.method === 'POST', goodBody);
  check('wrong method → 405', (await call({ 'X-Ferro-Secret': 's3cret' }, 'GET')).status === 405);
  check('also reachable by id', (await fetch(`${a.url}/hooks/${wh.id}`, { method: 'POST', headers: { 'X-Ferro-Secret': 's3cret', 'Content-Type': 'application/json' }, body: '{}' })).status === 200);

  const async_ = await putFlow({
    name: 'Async hook', trigger: { type: 'webhook', slug: `as-${rnd()}`, access: 'public', run_async: true },
    operations: [{ key: 'call', type: 'request', options: { method: 'POST', url: `${BASE}/async`, body: { v: '{{ $trigger.body.v }}' } } }],
  });
  const aslug = (await a.rest('GET', '/flows')).data.find((f) => f.id === async_.id).trigger.slug;
  const ar = await fetch(`${a.url}/x/${aslug}`, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: '{"v":"later"}' });
  check('run_async answers 202 immediately', ar.status === 202);
  const ah = await waitFor(() => hitsFor('/async')[0]);
  check('…and the job runs in the background worker', ah?.body?.v === 'later');
  check('…and is recorded', !!(await waitFor(async () => (await runs(async_.id))[0])));

  // ---- 7. chaining ---------------------------------------------------------------------------------------------
  const inner = await putFlow({ name: 'Inner', trigger: { type: 'manual' }, operations: [{ key: 't', type: 'transform', options: { value: { doubled: '{{ $trigger.payload.n }}', tag: 'from-inner' } } }] });
  const outer = await putFlow({
    name: 'Outer', trigger: { type: 'manual' },
    operations: [{ key: 'go', type: 'trigger_flow', options: { flow: inner.id, payload: { n: '{{ $trigger.payload.n }}' } } }],
    output: { inner: '{{ go }}' },
  });
  const ro = await a.rest('POST', `/flows/${outer.id}/run`, { payload: { n: 21 } });
  check('trigger_flow returns the result of the other flow', ro.data?.inner?.tag === 'from-inner' && ro.data.inner.doubled === 21, ro.json);
  const selfId = uuid();
  const selfy = await putFlow({ id: selfId, name: 'Self', trigger: { type: 'manual' }, operations: [{ key: 'again', type: 'trigger_flow', options: { flow: selfId } }] });
  const rself = await a.rest('POST', `/flows/${selfy.id}/run`, {});
  check('a flow calling itself stops at the depth limit', rself.json.meta.ok === false && /levels deep/.test(rself.json.meta.error), rself.json.meta.error);

  // ---- 8. run_as caller ---------------------------------------------------------------------------------------------
  const email = `fl${rnd()}@example.com`;
  await a.rest('POST', '/users', { email, password: 'editor-pass-123', role: '0199a000-0000-7000-8000-00000000e001' });
  const ed = (await a.rest('POST', '/auth/login', { email, password: 'editor-pass-123' }, null)).data.access_token;
  const mk = (run_as) => ({ name: `As ${run_as}`, run_as, trigger: { type: 'manual' }, operations: [{ key: 'w', type: 'item_create', options: { collection: coll, payload: { title: `by ${run_as}` } } }] });
  const asSystem = await putFlow(mk('system'));
  const asCaller = await putFlow(mk('caller'));
  check('run_as system: an editor without rights on the collection can run it', (await a.rest('POST', `/flows/${asSystem.id}/run`, {}, ed)).json.meta.ok === true);
  const denied = await a.rest('POST', `/flows/${asCaller.id}/run`, {}, ed);
  check('run_as caller: the same flow is refused with the editor permissions', denied.json.meta.ok === false && /permission/.test(denied.json.meta.error), denied.json.meta.error);
  check('run_as caller: an administrator can', (await a.rest('POST', `/flows/${asCaller.id}/run`, {})).json.meta.ok === true);
  check('anonymous cannot run manual flows', (await a.rest('POST', `/flows/${asSystem.id}/run`, {}, null)).status === 403);

  // ---- 9. blocking hooks on update and delete -----------------------------------------------------------------------------
  await putFlow({
    name: 'Guard update', trigger: { type: 'event', events: [`${coll}.items.update`], blocking: true },
    operations: [{ key: 'c', type: 'condition', options: { rule: { $trigger: { payload: { n: { _gt: 100 } } } } }, resolve: 'deny' }, { key: 'deny', type: 'fail', options: { message: 'n is too big' } }],
  });
  const keep = (await a.rest('POST', `/items/${coll}`, { title: 'keep', n: 1 })).data;
  check('blocking update hook: allowed value passes', (await a.rest('PATCH', `/items/${coll}/${keep.id}`, { n: 50 })).status === 200);
  const big = await a.rest('PATCH', `/items/${coll}/${keep.id}`, { n: 500 });
  check('blocking update hook: refused value → 400 with the message', big.status === 400 && /n is too big/.test(big.error), big.error);
  check('…and the value was not written', (await a.rest('GET', `/items/${coll}/${keep.id}?fields=n`)).data.n === 50);
  await putFlow({
    name: 'Guard delete', trigger: { type: 'event', events: [`${coll}.items.delete`], blocking: true },
    operations: [
      { key: 'r', type: 'item_read', options: { collection: coll, filter: { id: { _in: '{{ $trigger.keys }}' } } }, resolve: 't' },
      { key: 't', type: 'transform', options: { value: { title: '{{ r.0.title }}' } }, resolve: 'c' },
      { key: 'c', type: 'condition', options: { rule: { t: { title: { _eq: 'keep' } } } }, resolve: 'deny' },
      { key: 'deny', type: 'fail', options: { message: 'this item cannot be deleted' } },
    ],
  });
  const del = await a.rest('DELETE', `/items/${coll}/${keep.id}`);
  check('blocking delete hook refuses the deletion', del.status === 400 && /cannot be deleted/.test(del.error), del.error);
  check('…and the item is still there', (await a.rest('GET', `/items/${coll}/${keep.id}`)).status === 200);
  const other = (await a.rest('POST', `/items/${coll}`, { title: 'disposable' })).data;
  check('other items can be deleted', (await a.rest('DELETE', `/items/${coll}/${other.id}`)).status === 204);

  // ---- 10. disabled flows do nothing -----------------------------------------------------------------------------------------
  const flowsNow = (await a.rest('GET', '/flows')).data;
  const outbound = flowsNow.find((f) => f.id === hook.id);
  await a.rest('PUT', `/flows/${hook.id}`, { ...outbound, active: false });
  const hb = hitsFor('/hook').length;
  await a.rest('POST', `/items/${coll}`, { title: 'silent', n: 1 });
  await sleep(2500);
  check('a disabled flow is not triggered', hitsFor('/hook').length === hb);
  const man = await a.rest('POST', `/flows/${hook.id}/run`, {});
  check('…and cannot be run', man.status === 404 || man.status === 403);

  // ---- 11. SSRF: B does not allow private addresses -------------------------------------------------------------------------------
  const ssrf = await putFlow({ name: 'Internal call', trigger: { type: 'manual' }, operations: [{ key: 'call', type: 'request', options: { url: `${BASE}/secret` } }] }, b);
  const rb = await b.rest('POST', `/flows/${ssrf.id}/run`, {});
  check('a request to a loopback address is refused by default', rb.json.meta.ok === false && /private/.test(rb.json.meta.error), rb.json.meta.error);
  check('…and nothing reached the server', hitsFor('/secret').length === 0);
  for (const target of ['http://169.254.169.254/latest/meta-data', 'http://[::1]:19001/x', 'http://localhost:19001/x', 'http://10.0.0.1/', 'file:///etc/passwd', 'ftp://example.com/']) {
    const f = await putFlow({ name: `ssrf ${rnd()}`, trigger: { type: 'manual' }, operations: [{ key: 'call', type: 'request', options: { url: target } }] }, b);
    const rr = await b.rest('POST', `/flows/${f.id}/run`, {});
    check(`SSRF target refused: ${target}`, rr.json.meta.ok === false && /(private|scheme)/.test(rr.json.meta.error), rr.json.meta.error);
  }
  const templated = await putFlow({ name: 'Templated host', trigger: { type: 'manual' }, operations: [{ key: 'call', type: 'request', options: { url: 'http://{{ $trigger.payload.host }}/x' } }] }, b);
  const rt = await b.rest('POST', `/flows/${templated.id}/run`, { payload: { host: '127.0.0.1:19001' } });
  check('a host assembled from user input is checked too', rt.json.meta.ok === false && /private/.test(rt.json.meta.error));

  echo.close();
}
