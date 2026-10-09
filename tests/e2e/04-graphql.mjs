// GraphQL: language features, root fields, mutations, errors, permissions.
import { rnd } from './lib.mjs';

export default async function ({ a, check }) {
  const gql = (q, v, token) => a.graphql(q, v, token);
  const tag = rnd();
  const slug = `gq-${tag}`;

  // seed through GraphQL itself (nested write inside a mutation)
  let r = await gql(
    `mutation($data: JSON!) { create_pages_item(data: $data) { id title blocks { collection item { __typename ... on block_text { body } } } } }`,
    { data: { title: 'GQL page', slug, status: 'published', blocks: [{ collection: 'block_text', item: { body: 'hello' }, sort: 1 }] } },
  );
  const page = r.json?.data?.create_pages_item;
  check('mutation with variables and a nested write', r.status === 200 && page?.title === 'GQL page' && page.blocks[0].item.body === 'hello' && page.blocks[0].item.__typename === 'block_text', r.json);

  // variables, defaults, aliases, root __typename
  r = await gql(`query($f: JSON = {slug: {_eq: "nothing"}}) { first: pages(filter: $f) { id } t: __typename }`, { f: { slug: { _eq: slug } } });
  check('variable overrides the default; alias; root __typename', r.json.data.first.length === 1 && r.json.data.t === 'Query');
  r = await gql(`query($f: JSON = {slug: {_eq: "nothing"}}) { pages(filter: $f) { id } }`);
  check('default value of a variable is used when not provided', r.json.data.pages.length === 0);

  // fragments and directives
  r = await gql(
    `query($withBlocks: Boolean!, $hide: Boolean!) {
       pages(filter: {slug: {_eq: "${slug}"}}) { ...basics blocks @include(if: $withBlocks) { collection } status @skip(if: $hide) }
     }
     fragment basics on pages { id title }`,
    { withBlocks: false, hide: true },
  );
  const p0 = r.json.data.pages[0];
  check('named fragment, @include(false) and @skip(true)', p0.title === 'GQL page' && !('blocks' in p0) && !('status' in p0), p0);
  r = await gql(`query($w: Boolean!) { pages(filter: {slug: {_eq: "${slug}"}}) { blocks @include(if: $w) { collection } } }`, { w: true });
  check('@include(true)', r.json.data.pages[0].blocks[0].collection === 'block_text');
  r = await gql(`query A { pages(limit: 1) { id } } query B { categories(limit: 1) { id } }`);
  check('several operations require operationName', r.json.errors?.[0]?.extensions?.code === 'GRAPHQL_VALIDATION_FAILED', r.json);
  r = await a.rest('POST', '/graphql', { query: `query A { pages(limit: 1) { id } } query B { categories(limit: 1) { code } }`, operationName: 'B' });
  check('operationName picks the operation', Array.isArray(r.json.data.categories) && !r.json.data.pages);

  // pagination / sort / counts
  const codes = [0, 1, 2, 3, 4].map((i) => `${tag}-c${i}`);
  await gql(`mutation($d: [JSON!]!) { create_categories_items(data: $d) { code } }`, { d: codes.map((code) => ({ code, label: 'x' })) });
  const mine = `{ code: { _starts_with: "${tag}-c" } }`;
  r = await gql(`{ categories(filter: ${mine}, sort: ["code"], limit: 2, page: 2) { code } n: categories_count(filter: ${mine}) }`);
  check('limit + page + sort', JSON.stringify(r.json.data.categories.map((c) => c.code)) === JSON.stringify([codes[2], codes[3]]), r.json);
  check('_count honours the filter', r.json.data.n === 5);
  r = await gql(`{ categories(filter: ${mine}, sort: "-code", limit: 1, offset: 4) { code } }`);
  check('offset and descending sort (string form)', r.json.data.categories[0].code === codes[0]);
  r = await gql(`{ categories(search: "${tag}-c3") { code } }`);
  check('search argument', r.json.data.categories.length === 1 && r.json.data.categories[0].code === codes[3]);

  // many: update and delete
  const ids = (await gql(`{ categories(filter: ${mine}) { id } }`)).json.data.categories.map((c) => c.id);
  r = await gql(`mutation($ids: [ID!]!) { update_categories_items(ids: $ids, data: {label: "changed"}) { label } }`, { ids });
  check('update_*_items updates every item', r.json.data.update_categories_items.length === 5 && r.json.data.update_categories_items.every((c) => c.label === 'changed'));
  r = await gql(`mutation($ids: [ID!]!) { delete_categories_items(ids: $ids) { ids } d: __typename }`, { ids: ids.slice(0, 3) });
  check('delete_*_items returns the ids', r.json.data.delete_categories_items.ids.length === 3, r.json);
  r = await gql(`mutation { delete_categories_item(id: "${ids[3]}") { id __typename } }`);
  check('delete_*_item', r.json.data.delete_categories_item.id === ids[3] && r.json.data.delete_categories_item.__typename === 'delete_one');
  r = await gql(`{ categories_by_id(id: "${ids[3]}") { id } }`);
  check('_by_id of a deleted item is null, not an error', r.json.data.categories_by_id === null && !r.data.errors);

  // singleton
  r = await gql(`mutation { update_settings(data: {site_name: "Ferro"}) { id site_name } }`);
  const sid = r.json.data.update_settings.id;
  r = await gql(`mutation { update_settings(data: {site_name: "Ferro 2"}) { id site_name } }`);
  check('singleton: update creates then updates the same row', r.json.data.update_settings.id === sid && r.json.data.update_settings.site_name === 'Ferro 2');
  r = await gql(`{ settings { site_name } }`);
  check('singleton query returns the object', r.json.data.settings.site_name === 'Ferro 2');

  // GET
  const q = encodeURIComponent(`query($s: JSON) { pages(filter: $s) { slug } }`);
  const get = await fetch(`${a.url}/graphql?query=${q}&variables=${encodeURIComponent(JSON.stringify({ s: { slug: { _eq: slug } } }))}`, { headers: { Authorization: `Bearer ${a.token}` } });
  check('GET with variables works for queries', (await get.json()).data.pages[0].slug === slug);
  const getMut = await fetch(`${a.url}/graphql?query=${encodeURIComponent('mutation { create_categories_item(data: {code: "x"}) { id } }')}`, { headers: { Authorization: `Bearer ${a.token}` } });
  check('GET refuses mutations (CSRF-style protection)', (await getMut.json()).errors?.[0]?.extensions?.code === 'METHOD_NOT_ALLOWED');

  // errors
  r = await gql(`{ pages { id `);
  check('syntax error → GRAPHQL_PARSE_FAILED', r.status === 200 && r.json.errors[0].extensions.code === 'GRAPHQL_PARSE_FAILED');
  r = await gql(`{ pages { nope } }`);
  check('unknown field → clear message', /Cannot query field "nope" on type "pages"/.test(r.json.errors[0].message));
  r = await gql(`{ nope { id } }`);
  check('unknown root field', /Cannot query field "nope" on type "Query"/.test(r.json.errors[0].message));
  r = await gql(`{ __schema { types { name } } }`);
  check('introspection explains what to do', /SDL/.test(r.json.errors[0].message));
  r = await gql(`{ pages }`);
  check('an object field needs a selection', /must have a selection/.test(r.json.errors[0].message));
  r = await gql(`{ ${'a:pages{id '.repeat(40)}${'}'.repeat(40)} }`);
  check('absurd nesting is refused', r.json.errors?.[0]?.extensions?.code === 'GRAPHQL_PARSE_FAILED' && /deeply/.test(r.json.errors[0].message));
  r = await gql(`subscription { pages { id } }`);
  check('subscriptions are refused with a clear message', /not supported/.test(r.json.errors[0].message));
  r = await gql(`fragment x on pages { id } { pages { ...nope } }`);
  check('unknown fragment', /unknown fragment/.test(r.json.errors[0].message));
  r = await gql(`fragment x on pages { ...y } fragment y on pages { ...x } { pages(limit: 1) { ...x } }`);
  check('fragment cycles are cut', /nested too deeply/.test(r.json.errors[0].message));

  // atomic mutations
  const dup = `${tag}-atomic`;
  r = await gql(`mutation { one: create_categories_item(data: {code: "${dup}"}) { id } two: create_categories_item(data: {code: "${dup}"}) { id } }`);
  check('a failing mutation field fails the whole request (data null)', r.json.data === null && r.json.errors[0].extensions.code === 'RECORD_NOT_UNIQUE', r.json);
  r = await gql(`{ categories(filter: {code: {_eq: "${dup}"}}) { id } }`);
  check('…and the first field was rolled back', r.json.data.categories.length === 0);

  // permissions
  const email = `gq${tag}@example.com`;
  await a.rest('POST', '/users', { email, password: 'editor-pass-123', role: '0199a000-0000-7000-8000-00000000e001' });
  const ed = (await a.rest('POST', '/auth/login', { email, password: 'editor-pass-123' }, null)).data.access_token;
  r = await gql(`{ pages(limit: 1) { id } ferro_users { id } }`, undefined, ed);
  check('partial data: one forbidden root field does not hide the others', Array.isArray(r.json.data.pages) && r.json.data.ferro_users === null && r.json.errors[0].path[0] === 'ferro_users' && r.json.errors[0].extensions.code === 'FORBIDDEN', r.json);
  r = await gql(`mutation { delete_pages_item(id: "${page.id}") { id } }`, undefined, ed);
  check('editor cannot delete through GraphQL', r.json.errors[0].extensions.code === 'FORBIDDEN');
  r = await gql(`mutation { create_categories_item(data: {code: "x"}) { id } }`, undefined, null);
  check('anonymous cannot mutate', r.json.errors[0].extensions.code === 'FORBIDDEN');
  await a.rest('PATCH', `/items/pages/${page.id}`, { status: 'draft' });
  r = await gql(`{ pages(filter: {slug: {_eq: "${slug}"}}) { id } }`, undefined, null);
  check('anonymous does not see a draft', r.json.data.pages.length === 0);
  r = await gql(`{ pages(limit: 1) { title user_created } }`, undefined, ed);
  check('fields not granted are not exposed (user_created is granted by `*`)', r.json.data.pages.length === 1);
  check('SDL needs authentication', (await a.rest('GET', '/graphql/schema', undefined, null)).status === 401);
}
