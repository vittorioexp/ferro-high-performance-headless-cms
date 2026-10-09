import assert from 'node:assert/strict';
import { test } from 'node:test';

import { COLLECTIONS } from './fixture.mjs';
import { findAll, fire, install } from './fakedom.mjs';

install();
const calls = [];
globalThis.fetch = async (url) => {
  calls.push(String(url));
  const body = String(url).includes('/items/categories')
    ? [{ id: 'cat1', code: 'corp' }]
    : String(url).includes('/items/tags')
      ? [{ id: 'tag1', name: 'News' }, { id: 'tag2', name: 'Press' }]
      : [];
  return { ok: true, status: 200, text: async () => JSON.stringify({ data: body }) };
};

const S = await import('../js/schema.js');
const { renderForm, FormError } = await import('../js/form.js');
S.setCollections(COLLECTIONS);

const page = {
  id: 'p1',
  title: 'Home',
  slug: 'home',
  status: 'published',
  views: 3,
  featured: true,
  seo: { title: 'x' },
  cover: null,
  category: 'cat1',
  tags: [{ id: 'jt1', pages_id: 'p1', tags_id: 'tag1' }],
  blocks: [
    { id: 'j1', pages_id: 'p1', collection: 'block_text', sort: 1, item: { id: 'b1', body: 'Hello' } },
    {
      id: 'j2',
      pages_id: 'p1',
      collection: 'block_cards',
      sort: 2,
      item: { id: 'b2', title: 'Cards', cards: [{ id: 'c1', block_cards_id: 'b2', title: 'One', sort: 1 }] },
    },
  ],
};

const inputs = (root, pred) => findAll(root, pred);

test('unchanged form round-trips the nested payload', () => {
  const form = renderForm('pages', page);
  const out = form.collect();
  assert.equal(out.title, 'Home');
  assert.equal(out.views, 3);
  assert.equal(out.featured, true);
  assert.deepEqual(out.seo, { title: 'x' });
  assert.equal(out.category, 'cat1');
  assert.deepEqual(out.tags, [{ tags_id: 'tag1', id: 'jt1', }]);
  assert.deepEqual(out.blocks, [
    { id: 'j1', collection: 'block_text', item: { body: 'Hello', id: 'b1' }, sort: 1 },
    {
      id: 'j2',
      collection: 'block_cards',
      item: { title: 'Cards', cards: [{ title: 'One', id: 'c1', sort: 1 }], id: 'b2' },
      sort: 2,
    },
  ]);
});

test('editing a block text and reordering', async () => {
  const form = renderForm('pages', page);
  const area = inputs(form.el, (n) => n.tagName === 'TEXTAREA' && n.value === 'Hello')[0];
  assert.ok(area, 'block body textarea');
  area.value = 'Hi';
  // move the first block down
  const down = inputs(form.el, (n) => n.tagName === 'BUTTON' && n.text === '↓')[0];
  await fire(down, 'click');
  const out = form.collect();
  assert.deepEqual(out.blocks.map((b) => [b.id, b.sort]), [['j2', 1], ['j1', 2]]);
  assert.equal(out.blocks[1].item.body, 'Hi');
});

test('adding a new block creates it without ids', async () => {
  const form = renderForm('pages', { ...page, blocks: [] });
  const add = inputs(form.el, (n) => n.tagName === 'BUTTON' && n.text === '+ Add block')[0];
  await fire(add, 'click');
  const area = inputs(form.el, (n) => n.tagName === 'TEXTAREA' && n.value === '')[0];
  area.value = 'New block';
  const out = form.collect();
  assert.deepEqual(out.blocks, [{ collection: 'block_text', item: { body: 'New block' }, sort: 1 }]);
});

test('required fields are enforced and absent fields are not overwritten', () => {
  const empty = renderForm('pages', null);
  assert.throws(() => empty.collect(), (e) => e instanceof FormError && /Title/.test(e.message) && /Slug/.test(e.message));

  // partial read (no permission on `views`): the field must not be sent back as null
  const { views, ...partial } = page;
  const out = renderForm('pages', partial).collect();
  assert.ok(!('views' in out));
});

test('new item gets defaults', () => {
  const form = renderForm('pages', null);
  const title = inputs(form.el, (n) => n.tagName === 'INPUT' && n.attrs.type === 'text')[0];
  title.value = 'T';
  const slug = inputs(form.el, (n) => n.tagName === 'INPUT' && n.attrs.type === 'text')[1];
  slug.value = 's';
  const out = form.collect();
  assert.equal(out.status, 'draft');
  assert.deepEqual(out.blocks, []);
});

test('relation options are fetched once per collection', async () => {
  await new Promise((r) => setTimeout(r, 10));
  const n = calls.filter((u) => u.includes('/items/categories')).length;
  assert.equal(n, 1);
});
