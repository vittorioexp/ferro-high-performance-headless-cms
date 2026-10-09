// GraphQL playground: run queries/mutations as the signed-in user, download the schema.

import { api, download } from '../api.js';
import { add, button, fail, h } from '../dom.js';
import * as S from '../schema.js';
import { parseJson } from '../util.js';

/** A starter query for the first collection, using its plain fields. */
export function sampleQuery() {
  const c = S.userCollections().find((x) => !x.meta?.hidden && !x.meta?.singleton);
  if (!c) return '{\n  ferro_files(limit: 5) {\n    id\n    filename_download\n  }\n}\n';
  const fields = S.scalarFields(c)
    .filter((f) => !['json', 'm2a_item'].includes(f.type) && !['m2o', 'file'].includes(f.type))
    .slice(0, 4)
    .map((f) => `    ${f.name}`);
  return `{\n  ${c.name}(limit: 5) {\n    id\n${fields.join('\n')}\n  }\n}\n`;
}

export async function graphqlView(root) {
  const query = h('textarea', { rows: 14, class: 'mono', spellcheck: 'false' }, sampleQuery());
  query.value = sampleQuery();
  const variables = h('textarea', { rows: 4, class: 'mono', spellcheck: 'false', placeholder: '{ "id": "…" }' });
  const result = h('pre', { class: 'result' }, '');

  async function run() {
    result.textContent = 'Running…';
    const vars = parseJson(variables.value, 'Variables');
    const started = performance.now();
    try {
      const res = await api.post('/graphql', { query: query.value, variables: vars || undefined });
      result.textContent = `${JSON.stringify(res, null, 2)}\n\n// ${Math.round(performance.now() - started)} ms`;
    } catch (err) {
      result.textContent = err.message;
      throw err;
    }
  }

  add(root, 
    h(
      'div',
      { class: 'page-head' },
      h('h1', {}, 'GraphQL'),
      h('span', { class: 'spacer' }),
      button('Download schema (SDL)', () => download('/graphql/schema', 'schema.graphql').catch(fail)),
      button('Run', run, 'primary'),
    ),
    h(
      'div',
      { class: 'two-col' },
      h('div', { class: 'card' }, h('h3', {}, 'Query'), query, h('h3', {}, 'Variables (JSON)'), variables),
      h('div', { class: 'card' }, h('h3', {}, 'Result'), result),
    ),
    h(
      'p',
      { class: 'muted' },
      'Endpoint: POST /graphql with the same bearer token as REST. Filters use the REST format (JSON). ',
      'Blocks (M2A) are unions: item { __typename ... on block_text { body } }. Introspection is not available: use the downloaded SDL for code generation.',
    ),
  );
}
