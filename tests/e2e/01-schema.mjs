// Schema changes on a live database: renames, type changes, destructive operations, rollbacks, locks.
import { rnd } from './lib.mjs';

export default async function ({ a, check }) {
  const name = `qc_books_${rnd()}`;
  const fields = async (c) => (await a.rest('GET', `/collections/${c}`)).data?.fields?.map((f) => f.name) ?? null;
  const hash = async () => (await a.rest('GET', '/server/info')).data.schema_hash;

  let r = await a.rest('POST', '/collections', { name, fields: [{ name: 'title', type: 'string', required: true }, { name: 'pages', type: 'integer' }] });
  check('create a collection', r.status === 200 && r.json.meta.applied === true, r.error);
  const b1 = (await a.rest('POST', `/items/${name}`, { title: 'Dune', pages: 412 })).data;
  const b2 = (await a.rest('POST', `/items/${name}`, { title: 'Emma', pages: 300 })).data;
  check('items can be written to the new table', !!b1?.id && !!b2?.id);

  // dry run
  const h0 = await hash();
  r = await a.rest('POST', `/fields/${name}?dry_run=true`, { name: 'isbn', type: 'string' });
  check('dry run reports the change without applying it', r.status === 200 && r.json.meta.applied === false && r.json.meta.changes.some((c) => c.includes('isbn')) && !(await fields(name)).includes('isbn') && (await hash()) === h0);

  // rename a field: the data stays
  r = await a.rest('PATCH', `/fields/${name}/title`, { name: 'name', type: 'string', required: true });
  check('rename a field', r.status === 200 && r.json.meta.changes.some((c) => c.includes('title -> name')), r.error);
  check('rename keeps the data', (await a.rest('GET', `/items/${name}/${b1.id}?fields=name`)).data?.name === 'Dune');

  // widening is safe, narrowing needs consent
  r = await a.rest('PATCH', `/fields/${name}/pages`, { name: 'pages', type: 'bigint' });
  check('integer → bigint is a safe change', r.status === 200 && !r.json.meta.changes.some((c) => c.includes('[destructive]')), r.error);
  r = await a.rest('PATCH', `/fields/${name}/pages`, { name: 'pages', type: 'string' });
  check('bigint → string is refused without allow_destructive (409)', r.status === 409 && /destructive/.test(r.error), r.error);
  check('…and nothing changed', (await a.rest('GET', `/collections/${name}`)).data.fields.find((f) => f.name === 'pages').type === 'bigint');
  r = await a.rest('PATCH', `/fields/${name}/pages?allow_destructive=true`, { name: 'pages', type: 'string' });
  check('…but works with allow_destructive', r.status === 200, r.error);
  check('values are converted by the type change', (await a.rest('GET', `/items/${name}/${b1.id}?fields=pages`)).data?.pages === '412');

  // a failing change rolls back completely
  const hBefore = await hash();
  r = await a.rest('POST', `/fields/${name}`, { name: 'must_have', type: 'string', required: true });
  check('NOT NULL column on a populated table fails (400)', r.status === 400 && /schema change failed/.test(r.error), r.error);
  check('…and leaves schema and hash untouched', !(await fields(name)).includes('must_have') && (await hash()) === hBefore);
  await a.rest('POST', `/fields/${name}`, { name: 'code', type: 'string' });
  await a.rest('PATCH', `/items/${name}/${b1.id}`, { code: 'X' });
  await a.rest('PATCH', `/items/${name}/${b2.id}`, { code: 'X' });
  r = await a.rest('PATCH', `/fields/${name}/code`, { name: 'code', type: 'string', unique: true });
  check('UNIQUE on duplicate data fails and rolls back', r.status === 400 && (await a.rest('GET', `/collections/${name}`)).data.fields.find((f) => f.name === 'code').unique !== true, r.error);
  await a.rest('PATCH', `/items/${name}/${b2.id}`, { code: 'Y' });
  r = await a.rest('PATCH', `/fields/${name}/code`, { name: 'code', type: 'string', unique: true });
  check('…and succeeds once the data is fixed', r.status === 200, r.error);
  check('the unique constraint is enforced', (await a.rest('PATCH', `/items/${name}/${b2.id}`, { code: 'X' })).status === 409);

  // optimistic lock
  const snap = (await a.rest('GET', '/schema/snapshot')).data;
  r = await a.rest('POST', '/schema/apply', { snapshot: snap, expected_hash: 'f'.repeat(64) });
  check('a stale expected_hash is refused (409)', r.status === 409 && /changed since/.test(r.error), r.error);
  r = await a.rest('POST', '/schema/apply', { snapshot: snap, expected_hash: await hash() });
  check('an up-to-date expected_hash is accepted (no-op)', r.status === 200 && r.json.data.applied === false);

  // validation of names and structure
  for (const [label, body] of [
    ['reserved prefix', { name: 'ferro_things', fields: [] }],
    ['invalid identifier', { name: '1bad', fields: [] }],
    ['SQL in the name', { name: 'x"; DROP TABLE ferro_users; --', fields: [] }],
    ['declaring id', { name: `qc_${rnd()}`, fields: [{ name: 'id', type: 'string' }] }],
    ['duplicate field', { name: `qc_${rnd()}`, fields: [{ name: 'a', type: 'string' }, { name: 'a', type: 'text' }] }],
    ['m2o to a missing collection', { name: `qc_${rnd()}`, fields: [{ name: 'x', type: 'm2o', relation: { related: 'nope_nope' } }] }],
    ['reserved system field', { name: `qc_${rnd()}`, fields: [{ name: 'date_created', type: 'datetime' }] }],
  ]) {
    const x = await a.rest('POST', '/collections', body);
    check(`rejected: ${label}`, x.status === 400, x.error?.slice(0, 80));
  }
  check('the users table is intact after the injection attempt', (await a.rest('GET', '/users?limit=1')).status === 200);
  check('existing collection cannot be created twice', (await a.rest('POST', '/collections', { name, fields: [] })).status === 400);

  // referential actions
  const parent = `qc_p_${rnd()}`, child = `qc_c_${rnd()}`, strict = `qc_s_${rnd()}`;
  await a.rest('POST', '/collections', { name: parent, fields: [{ name: 'n', type: 'string' }] });
  await a.rest('POST', '/collections', { name: child, fields: [{ name: 'parent', type: 'm2o', relation: { related: parent, on_delete: 'cascade' } }] });
  await a.rest('POST', '/collections', { name: strict, fields: [{ name: 'parent', type: 'm2o', relation: { related: parent, on_delete: 'restrict' } }] });
  const p1 = (await a.rest('POST', `/items/${parent}`, { n: 'p1' })).data;
  const c1 = (await a.rest('POST', `/items/${child}`, { parent: p1.id })).data;
  check('FK to a missing row is rejected', (await a.rest('POST', `/items/${child}`, { parent: '00000000-0000-4000-8000-000000000000' })).status === 400);
  await a.rest('POST', `/items/${strict}`, { parent: p1.id });
  check('on_delete restrict blocks deleting the parent', (await a.rest('DELETE', `/items/${parent}/${p1.id}`)).status === 400);
  await a.rest('DELETE', `/items/${strict}`, [(await a.rest('GET', `/items/${strict}?fields=id`)).data[0].id]);
  check('on_delete cascade deletes the children', (await a.rest('DELETE', `/items/${parent}/${p1.id}`)).status === 204 && (await a.rest('GET', `/items/${child}/${c1.id}`)).status === 404);

  // rename a collection: data and constraints follow
  const renamed = `qc_titles_${rnd()}`;
  r = await a.rest('PATCH', `/collections/${name}`, { ...(await a.rest('GET', `/collections/${name}`)).data, name: renamed });
  check('rename a collection', r.status === 200 && r.json.meta.changes.some((c) => c.includes(`${name} -> ${renamed}`)), r.error);
  check('the data followed the rename', (await a.rest('GET', `/items/${renamed}/${b1.id}?fields=name`)).data?.name === 'Dune' && (await a.rest('GET', `/items/${name}`)).status === 404);
  check('the unique constraint survived the rename', (await a.rest('PATCH', `/items/${renamed}/${b2.id}`, { code: 'X' })).status === 409);

  // dropping
  r = await a.rest('DELETE', `/collections/${renamed}?dry_run=true`);
  check('drop dry run flags the data loss', r.json.meta.changes.some((c) => c.includes('[destructive]')) && (await a.rest('GET', `/items/${renamed}`)).status === 200);
  r = await a.rest('DELETE', `/collections/${renamed}`);
  check('drop a collection', r.status === 200 && (await a.rest('GET', `/items/${renamed}`)).status === 404, r.error);
  for (const c of [child, strict, parent]) await a.rest('DELETE', `/collections/${c}`);

  // history and exports
  const hist = (await a.rest('GET', '/schema/history?limit=100')).data;
  check('schema history records the applied plans', hist.length >= 10 && hist.every((h) => h.from_hash && h.to_hash && h.plan.changes));
  const yaml = await a.rest('GET', '/schema/snapshot?format=yaml');
  check('snapshot as YAML', yaml.status === 200 && /collections:/.test(yaml.text) && /name: pages/.test(yaml.text));
  const sdl = await a.rest('GET', '/graphql/schema');
  check('GraphQL schema follows the model', sdl.status === 200 && /type pages \{/.test(sdl.text) && !sdl.text.includes(renamed));
}
