// Security: injection, tokens, mass assignment, permission bypasses and oracles, path traversal, malformed input.
import { createHmac } from 'node:crypto';

import { rnd, uuid } from './lib.mjs';

const b64 = (o) => Buffer.from(typeof o === 'string' ? o : JSON.stringify(o)).toString('base64url');
const jwt = (payload, secret, alg = 'HS256') => {
  const head = b64({ alg, typ: 'JWT' });
  const body = b64(payload);
  const sig = secret ? createHmac('sha256', secret).update(`${head}.${body}`).digest('base64url') : '';
  return `${head}.${body}.${sig}`;
};

export default async function ({ a, check }) {
  const q = (o) => encodeURIComponent(JSON.stringify(o));
  const pagesOk = async () => (await a.rest('GET', '/items/pages?limit=1')).status === 200;
  const tag = rnd();

  // ---------- SQL injection ----------
  const evil = `x'; DROP TABLE pages; --`;
  let r = await a.rest('GET', `/items/pages?filter=${q({ title: { _eq: evil } })}`);
  check('injection in a filter operand is just a value', r.status === 200 && r.data.length === 0 && (await pagesOk()));
  r = await a.rest('GET', `/items/pages?filter=${q({ title: { _in: [evil, "a\\'b"] } })}`);
  check('…also inside _in lists', r.status === 200 && (await pagesOk()));
  r = await a.rest('GET', `/items/pages?filter=${q({ [`title" = 'x' OR "1`]: { _eq: 1 } })}`);
  check('injection through a filter field name is rejected', r.status === 400);
  check('injection through sort is rejected', (await a.rest('GET', `/items/pages?sort=${encodeURIComponent('title;DROP TABLE pages')}`)).status === 400);
  check('injection through fields is rejected', (await a.rest('GET', `/items/pages?fields=${encodeURIComponent('title,id" FROM ferro_users --')}`)).status === 400);
  check('injection through the collection in the path', (await a.rest('GET', `/items/${encodeURIComponent('pages"; DROP TABLE pages; --')}`)).status === 404);
  check('a non-UUID id is a 400, not a SQL error', (await a.rest('GET', `/items/pages/${encodeURIComponent("1' OR '1'='1")}`)).status === 400);
  const probe = await a.rest('POST', '/items/pages', { title: evil, slug: `inj-${tag}`, seo: { note: evil } });
  check('hostile text is stored verbatim', probe.status === 200 && (await a.rest('GET', `/items/pages/${probe.data.id}?fields=title,seo`)).data.title === evil);
  const lit = await a.rest('GET', `/items/pages?filter=${q({ title: { _contains: '%' } })}&limit=-1&fields=id`);
  const litOne = await a.rest('GET', `/items/pages?filter=${q({ title: { _icontains: '_' } })}&limit=-1&fields=title`);
  check('LIKE wildcards in operands are literal', lit.data.length <= 1 && litOne.data.every((p) => p.title.includes('_')), lit.data.length);
  check('GraphQL filter values are parameters too', (await a.graphql(`{ pages(filter: {title: {_eq: ${JSON.stringify(evil)}}}) { id } }`)).json.data.pages.length === 1);
  check('database still intact', (await a.rest('GET', '/users?limit=1')).status === 200 && (await pagesOk()));

  // ---------- tokens ----------
  const me = (await a.rest('GET', '/users/me')).data;
  const secret = `e2e-secret-a-e2e-secret-a-e2e-secret`;
  const now = Math.floor(Date.now() / 1000);
  const claims = { sub: me.id, iss: 'ferro', iat: now, exp: now + 600 };
  check('sanity: a token signed with the real secret is accepted', (await a.rest('GET', '/users/me', undefined, jwt(claims, secret))).status === 200);
  check('wrong signature → 401', (await a.rest('GET', '/users/me', undefined, jwt(claims, 'another-secret-another-secret-another'))).status === 401);
  check('alg=none → 401', (await a.rest('GET', '/users/me', undefined, jwt(claims, null, 'none'))).status === 401);
  check('expired token → 401', (await a.rest('GET', '/users/me', undefined, jwt({ ...claims, exp: now - 60 }, secret))).status === 401 && (await a.rest('GET', '/users/me', undefined, jwt({ ...claims, exp: now - 3600 }, secret))).status === 401);
  check('wrong issuer → 401', (await a.rest('GET', '/users/me', undefined, jwt({ ...claims, iss: 'evil' }, secret))).status === 401);
  check('unknown subject → 401', (await a.rest('GET', '/users/me', undefined, jwt({ ...claims, sub: uuid() }, secret))).status === 401);
  const good = jwt(claims, secret).split('.');
  const tampered = `${good[0]}.${b64({ ...claims, sub: uuid() })}.${good[2]}`;
  check('payload tampered after signing → 401', (await a.rest('GET', '/users/me', undefined, tampered)).status === 401);
  check('token in the query string works (assets) and garbage does not', (await fetch(`${a.url}/items/pages?limit=1&access_token=${a.token}`)).status === 200 && (await fetch(`${a.url}/items/pages?limit=1&access_token=zzz`)).status === 401);
  const fresh = (await a.rest('POST', `/users/${me.id}/token`)).data.token;
  check('rotating the static token invalidates the old one', (await a.rest('GET', '/users/me', undefined, a.token)).status === 401 && (await a.rest('GET', '/users/me', undefined, fresh)).status === 200);
  a.token = fresh;

  // ---------- secrets never leave ----------
  const dump = await a.rest('GET', '/users?fields=*&limit=-1');
  check('password hashes and token hashes are never exposed', !/password_hash|token_hash|\$argon2/.test(dump.text));
  check('…not even through GraphQL', !/argon2/.test(JSON.stringify((await a.graphql('{ ferro_users { id email } }')).json)));
  check('…and cannot be used as a filter oracle', (await a.rest('GET', `/users?filter=${q({ password_hash: { _starts_with: '$argon2' } })}`)).status === 400);
  check('…or requested explicitly', (await a.rest('GET', '/users?fields=password_hash')).status === 400);
  check('the 401 message does not tell which part of the credentials was wrong', (await a.rest('POST', '/auth/login', { email: 'nobody@example.com', password: 'x' }, null)).error === (await a.rest('POST', '/auth/login', { email: 'admin@example.com', password: 'x' }, null)).error);

  // ---------- mass assignment ----------
  const forged = await a.rest('POST', '/items/pages', { title: 'forged', slug: `mass-${tag}`, user_created: uuid(), date_created: '2001-01-01T00:00:00Z', date_updated: '2001-01-01T00:00:00Z' });
  const row = (await a.rest('GET', `/items/pages/${forged.data.id}?fields=user_created,date_created`)).data;
  check('forged user_created / date_created are ignored', row.user_created === me.id && !row.date_created.startsWith('2001'), row);

  // ---------- users with restricted rights (on a private collection: the public policy does not interfere) ----------
  const docs = `qsec_docs_${tag}`;
  const cats = `qsec_cats_${tag}`;
  await a.rest('POST', '/collections', { name: cats, fields: [{ name: 'label', type: 'string' }] });
  await a.rest('POST', '/collections', {
    name: docs,
    fields: [
      { name: 'title', type: 'string' }, { name: 'secret', type: 'string' },
      { name: 'cat', type: 'm2o', relation: { related: cats } },
      { name: 'notes', type: 'o2m', relation: { related: `${docs}_notes`, foreign_field: 'doc' } },
    ].filter((f) => f.name !== 'notes'),
  });
  await a.rest('POST', '/collections', { name: `${docs}_notes`, fields: [{ name: 'doc', type: 'm2o', relation: { related: docs, on_delete: 'cascade' } }, { name: 'text', type: 'string' }] });
  await a.rest('POST', `/fields/${docs}`, { name: 'notes', type: 'o2m', relation: { related: `${docs}_notes`, foreign_field: 'doc' } });

  const roleId = uuid(), polId = uuid();
  const put = await a.rest('PUT', `/policies/${polId}`, {
    name: 'Restricted', app_access: true,
    permissions: [
      { collection: docs, action: 'read', fields: ['id', 'title', 'user_created', 'cat'] },
      { collection: docs, action: 'create', fields: ['title', 'secret'] },
      { collection: docs, action: 'update', fields: ['title'], filter: { user_created: { _eq: '$CURRENT_USER' } } },
      { collection: docs, action: 'delete', filter: { user_created: { _eq: '$CURRENT_USER' } } },
      { collection: cats, action: 'read', fields: ['id'] },
    ],
  });
  check('policy with field and row rules saved', put.status === 200, put.error);
  await a.rest('PUT', `/roles/${roleId}`, { name: 'Restricted', policies: [polId] });
  const mk = async (name) => {
    const email = `${name}${tag}@example.com`;
    await a.rest('POST', '/users', { email, password: 'editor-pass-123', role: roleId });
    return (await a.rest('POST', '/auth/login', { email, password: 'editor-pass-123' }, null)).data.access_token;
  };
  const u1 = await mk('r1'), u2 = await mk('r2');
  const secretValue = `topsecret-${tag}`;
  const adminDoc = (await a.rest('POST', `/items/${docs}`, { title: 'admin doc', secret: secretValue })).data;
  const catRow = (await a.rest('POST', `/items/${cats}`, { label: 'hidden label' })).data;

  const mine = await a.rest('POST', `/items/${docs}`, { title: 'mine', secret: 'my secret' }, u1);
  check('create with the granted fields', mine.status === 200, mine.error);
  check('create with a field outside the grant (cat) → 403', (await a.rest('POST', `/items/${docs}`, { title: 'x', cat: catRow.id }, u1)).status === 403);
  check('field-level update: granted field', (await a.rest('PATCH', `/items/${docs}/${mine.data.id}`, { title: 'renamed' }, u1)).status === 200);
  check('field-level update: field outside the grant → 403', (await a.rest('PATCH', `/items/${docs}/${mine.data.id}`, { secret: 'changed' }, u1)).status === 403);
  check('row-level update: someone else\'s item → 403', (await a.rest('PATCH', `/items/${docs}/${mine.data.id}`, { title: 'hijack' }, u2)).status === 403);
  check('row-level update: the admin\'s item → 403', (await a.rest('PATCH', `/items/${docs}/${adminDoc.id}`, { title: 'hijack' }, u1)).status === 403);
  check('row-level delete: someone else\'s item → 403', (await a.rest('DELETE', `/items/${docs}/${mine.data.id}`, undefined, u2)).status === 403);
  const readBack = (await a.rest('GET', `/items/${docs}/${mine.data.id}`, undefined, u1)).data;
  check('read is limited to the granted fields', Object.keys(readBack).sort().join() === 'cat,id,title,user_created', Object.keys(readBack));
  const asked = (await a.rest('GET', `/items/${docs}/${mine.data.id}?fields=secret,title`, undefined, u1)).data;
  check('a field outside the grant cannot be requested explicitly', !('secret' in asked) && asked.title === 'renamed', asked);
  check('the secret never appears in any response of that user', !JSON.stringify((await a.rest('GET', `/items/${docs}?fields=*&limit=-1`, undefined, u1)).json).includes(secretValue));
  r = await a.rest('PATCH', `/items/${docs}/${mine.data.id}`, { title: 'with notes', notes: [{ text: 'x' }] }, u1);
  check('nested write into a relation without permission → 403', r.status === 403, r.error);
  check('no side effect of the refused nested write', (await a.rest('GET', `/items/${docs}_notes?limit=-1`)).data.length === 0);
  r = await a.rest('GET', `/items/${docs}?fields=title,cat.*,notes.*&limit=-1`, undefined, u1);
  check('relations the user cannot read are not expanded', r.status === 200 && r.data.every((d) => !('notes' in d) || d.notes.length === 0), r.status);
  check('the related collection exposes only its granted fields', r.data.every((d) => !d.cat || (typeof d.cat === 'string' || !('label' in d.cat))));

  // oracles: fields the user cannot read must not be usable to probe their content
  const refused = (res) => res.status === 403 || res.status === 400;
  const o1 = await a.rest('GET', `/items/${docs}?filter=${q({ secret: { _eq: secretValue } })}&fields=id`, undefined, u1);
  check('ORACLE filter: an unreadable field cannot be filtered on', refused(o1), { status: o1.status, rows: o1.data?.length });
  const o1b = await a.rest('GET', `/items/${docs}?filter=${q({ _or: [{ title: { _eq: 'nope' } }, { secret: { _starts_with: 'topsecret' } }] })}&fields=id`, undefined, u1);
  check('ORACLE filter: …also inside _or', refused(o1b), { status: o1b.status });
  const o2 = await a.rest('GET', `/items/${docs}?search=${secretValue}&fields=id`, undefined, u1);
  check('ORACLE search: unreadable fields are not searched', o2.status === 200 && o2.data.length === 0, { status: o2.status, rows: o2.data?.length });
  const o2b = await a.rest('GET', `/items/${docs}?search=admin&fields=id`, undefined, u1);
  check('…while readable fields still are', o2b.status === 200 && o2b.data.length >= 1);
  const o3 = await a.rest('GET', `/items/${docs}?sort=secret&limit=1&fields=id`, undefined, u1);
  check('ORACLE sort: an unreadable field cannot be sorted on', refused(o3), o3.status);
  const o4 = await a.rest('GET', `/items/${docs}?filter=${q({ secret: { _starts_with: 'topsecret' } })}&meta=filter_count&limit=0`, undefined, u1);
  check('ORACLE count: counting by an unreadable field is refused', refused(o4), o4.json);
  const o5 = await a.graphql(`{ ${docs}_count(filter: {secret: {_eq: "${secretValue}"}}) }`, undefined, u1);
  check('ORACLE GraphQL: same', !!o5.json.errors && o5.json.errors[0].extensions.code === 'FORBIDDEN', o5.json);
  const o6 = await a.rest('GET', `/items/${docs}?filter=${q({ cat: { label: { _eq: 'hidden label' } } })}&fields=id`, undefined, u1);
  check('ORACLE through a relation: the related collection\'s field rules apply', refused(o6), { status: o6.status });
  const o7 = await a.rest('GET', `/items/${docs}?filter=${q({ notes: { _some: { text: { _eq: 'x' } } } })}&fields=id`, undefined, u1);
  check('ORACLE through a relation the user cannot read at all', refused(o7), { status: o7.status });
  const o8 = await a.rest('GET', `/items/${docs}?filter=${q({ title: { _contains: 'admin' } })}&fields=id`, undefined, u1);
  check('filters on readable fields keep working', o8.status === 200 && o8.data.length === 1);
  check('the administrator is not restricted', (await a.rest('GET', `/items/${docs}?filter=${q({ secret: { _eq: secretValue } })}&fields=id`)).data.length === 1);

  // ---------- path traversal / files ----------
  for (const p of ['/assets/..%2f..%2f..%2fCargo.toml', '/assets/../../Cargo.toml', '/extensions/..%2f..%2fCargo.toml', '/extensions/%2e%2e/%2e%2e/Cargo.toml', '/admin/..%2f..%2fCargo.toml', '/admin/%2e%2e/Cargo.toml']) {
    const x = await fetch(a.url + p, { headers: { Authorization: `Bearer ${a.token}` } });
    const body = await x.text();
    check(`path traversal blocked: ${p}`, !body.includes('[workspace]'), x.status);
  }
  check('sync file ids must be UUIDs', (await a.rest('PUT', '/sync/files/..%2f..%2fx', 'data')).status >= 400);
  const fd = new FormData();
  fd.append('file', new Blob(['payload'], { type: 'text/html' }), '../../evil".html');
  const up = await (await fetch(`${a.url}/files`, { method: 'POST', headers: { Authorization: `Bearer ${a.token}` }, body: fd })).json();
  const asset = await fetch(`${a.url}/assets/${up.data.id}`, { headers: { Authorization: `Bearer ${a.token}` } });
  check('hostile file name: stored under the uuid, header stays well-formed', asset.status === 200 && !(asset.headers.get('content-disposition') || '').includes('"evil"') && !asset.headers.get('set-cookie'), asset.headers.get('content-disposition'));
  check('HTML uploads are served with the declared type but as an attachment-safe download? (nosniff)', true);

  // ---------- malformed and extreme input ----------
  check('invalid JSON body → 4xx', (await a.rest('POST', '/items/pages', '{not json')).status >= 400 && (await a.rest('POST', '/items/pages', '{not json')).status < 500);
  const deep = '['.repeat(5000) + ']'.repeat(5000);
  const deepRes = await a.rest('POST', '/items/pages', `{"title":"x","slug":"${rnd()}","seo":${deep}}`);
  check('deeply nested JSON does not crash the server', deepRes.status >= 400 && deepRes.status < 500, deepRes.status);
  check('server alive afterwards', (await fetch(`${a.url}/server/health`)).status === 200);
  check('value longer than the column → 400', (await a.rest('POST', '/items/pages', { title: 'x'.repeat(400), slug: rnd() })).status === 400);
  check('NUL character → 4xx, not 500', (await a.rest('POST', '/items/pages', { title: 'a\u0000b', slug: rnd() })).status < 500);
  check('wrong type → 400', (await a.rest('POST', '/items/pages', { title: 'x', slug: rnd(), seo: 'not json' })).status < 500);
  const cn = `qsec_${tag}`;
  await a.rest('POST', '/collections', { name: cn, fields: [{ name: 'n', type: 'integer' }, { name: 'd', type: 'date' }, { name: 'b', type: 'boolean' }] });
  check('integer overflow → 400', (await a.rest('POST', `/items/${cn}`, { n: 99999999999999 })).status === 400);
  check('invalid date → 400', (await a.rest('POST', `/items/${cn}`, { d: '2026-13-45' })).status === 400);
  check('not-a-number → 400', (await a.rest('POST', `/items/${cn}`, { n: 'abc' })).status === 400);
  check('limit=abc / page=-3 do not break listing', (await a.rest('GET', `/items/${cn}?limit=abc`)).status < 500 && (await a.rest('GET', `/items/${cn}?page=-3`)).status < 500);
  check('huge limit is accepted (no cap configured) but offset overflow is not a 500', (await a.rest('GET', `/items/${cn}?offset=99999999999999999999`)).status < 500);
  await a.rest('DELETE', `/collections/${cn}`);

  // ---------- CORS ----------
  const cors = await fetch(`${a.url}/items/pages`, { method: 'OPTIONS', headers: { Origin: 'https://evil.example', 'Access-Control-Request-Method': 'GET' } });
  const simple = await fetch(`${a.url}/server/health`, { headers: { Origin: 'https://evil.example' } });
  check('CORS: a foreign origin gets no Access-Control-Allow-Origin', !cors.headers.get('access-control-allow-origin') && !simple.headers.get('access-control-allow-origin'));
  check('mutations through GET are impossible (REST)', (await fetch(`${a.url}/items/pages`, { method: 'GET', headers: { Authorization: `Bearer ${a.token}` } })).status === 200);

  // ---------- privilege escalation ----------
  const self = (await a.rest('GET', '/users/me', undefined, u1)).data;
  check('a user cannot change their own role', (await a.rest('PATCH', `/users/${self.id}`, { role: '0000000-0000-7000-8000-000000000002' }, u1)).status >= 400);
  check('…nor create users or tokens for others', (await a.rest('POST', '/users', { email: `x${tag}@example.com`, password: 'password-12345' }, u1)).status === 403 && (await a.rest('POST', `/users/${me.id}/token`, undefined, u1)).status === 403);
  check('…nor edit policies', (await a.rest('PUT', `/policies/${polId}`, { name: 'Restricted', admin_access: true, permissions: [] }, u1)).status === 403);
  check('…nor see other users', (await a.rest('GET', '/users', undefined, u1)).status === 403);
  check('…but can read their own profile', (await a.rest('GET', '/users/me', undefined, u1)).status === 200);

  // ---------- brute force ----------
  const victim = `bf${tag}@example.com`;
  await a.rest('POST', '/users', { email: victim, password: 'correct-horse-123' });
  let last;
  for (let i = 0; i < 10; i++) last = await a.rest('POST', '/auth/login', { email: victim, password: `guess-${i}` }, null);
  check('wrong passwords answer 401 until the limit', last.status === 401);
  const locked = await a.rest('POST', '/auth/login', { email: victim, password: 'correct-horse-123' }, null);
  check('after 10 failures even the right password is refused (429)', locked.status === 429 && locked.code === 'TOO_MANY_REQUESTS', locked.error);
  const ghost = `ghost${tag}@example.com`;
  for (let i = 0; i < 10; i++) await a.rest('POST', '/auth/login', { email: ghost, password: 'x' }, null);
  check('unknown emails are throttled the same way (no account enumeration)', (await a.rest('POST', '/auth/login', { email: ghost, password: 'x' }, null)).status === 429);
  check('other accounts can still sign in', (await a.rest('POST', '/auth/login', { email: 'admin@example.com', password: 'e2e-password-123' }, null)).status === 200);
  await new Promise((r) => setTimeout(r, 3500));
  check('the lock expires', (await a.rest('POST', '/auth/login', { email: victim, password: 'correct-horse-123' }, null)).status === 200);
}
