import assert from 'node:assert/strict';
import { test } from 'node:test';

import { COLLECTIONS } from './fixture.mjs';
import * as S from '../js/schema.js';
import { buildFilter, formatBytes, isoToLocalInput, localInputToIso, matchRoute, parseJson, titleCase, truncate } from '../js/util.js';

test('matchRoute extracts params and ignores other shapes', () => {
  const patterns = ['/content/:collection', '/content/:collection/:id', '/files'];
  assert.deepEqual(matchRoute('/content/pages/abc%20d', patterns), { pattern: '/content/:collection/:id', params: { collection: 'pages', id: 'abc d' } });
  assert.deepEqual(matchRoute('/files', patterns).params, {});
  assert.equal(matchRoute('/nope', patterns), null);
});

test('buildFilter', () => {
  assert.equal(buildFilter([]), null);
  assert.equal(buildFilter([{ field: 'title', op: '_icontains', value: '' }]), null);
  assert.deepEqual(buildFilter([{ field: 'title', op: '_icontains', value: 'ro' }]), { title: { _icontains: 'ro' } });
  assert.deepEqual(
    buildFilter([
      { field: 'status', op: '_in', value: 'a, b' },
      { field: 'cover', op: '_null', value: '' },
    ]),
    { _and: [{ status: { _in: ['a', 'b'] } }, { cover: { _null: true } }] },
  );
});

test('small helpers', () => {
  assert.equal(truncate('abcdef', 4), 'abc…');
  assert.equal(titleCase('pages_blocks'), 'Pages Blocks');
  assert.equal(titleCase('ferro_files'), 'Files');
  assert.equal(formatBytes(2048), '2.0 KB');
  assert.throws(() => parseJson('{x', 'Seo'), /Seo/);
  assert.equal(parseJson('  '), null);
  const iso = '2026-01-02T03:04:00.000Z';
  assert.equal(localInputToIso(isoToLocalInput(iso)), iso);
  assert.equal(localInputToIso(''), null);
});

test('schema helpers build the fields needed by the form', () => {
  S.setCollections(COLLECTIONS);
  const param = S.editFieldsParam('pages').split(',');
  assert.ok(param.includes('*'));
  assert.ok(param.includes('blocks.*'));
  assert.ok(param.includes('blocks.item.*'));
  assert.ok(param.includes('blocks.item.cards.*'), 'nested o2m inside a block');
  assert.ok(param.includes('tags.*'));
  assert.ok(param.includes('cover.*'), 'file fields load their metadata');
  assert.equal(S.junctionTarget(S.getCollection('pages').fields.find((f) => f.name === 'tags')), 'tags');

  const list = S.listFieldsParam(S.getCollection('pages'));
  assert.ok(list.includes('title') && !list.includes('blocks') && !list.includes('seo'));

  assert.equal(S.labelOf('pages', { id: 'abcdef012345', title: 'Home' }), 'Home');
  assert.equal(S.labelOf('categories', { id: 'abcdef012345', code: 'corp' }), 'corp');
  assert.equal(S.labelOf('categories', { id: 'abcdef012345' }), 'abcdef01');
  assert.equal(S.getCollection('ferro_files').name, 'ferro_files');
});

test('thumbUrl asks for a cropped thumbnail only for rasters the server can resize', async () => {
  const { configure, thumbUrl, assetUrl } = await import('../js/api.js');
  configure({ base: 'http://x' });
  const id = '0199a000-0000-7000-8000-000000000001';
  for (const type of ['image/jpeg', 'image/png', 'image/gif', 'image/webp']) {
    assert.match(thumbUrl(id, type, 160), /\/assets\/[^?]+\?access_token=[^&]*&width=160&height=160&fit=cover$/, type);
  }
  for (const type of ['image/svg+xml', 'application/pdf', 'text/plain', '', undefined, null]) {
    assert.equal(thumbUrl(id, type), assetUrl(id), String(type));
  }
  assert.match(thumbUrl(id, 'image/png'), /width=240&height=240/, 'default size');
});
