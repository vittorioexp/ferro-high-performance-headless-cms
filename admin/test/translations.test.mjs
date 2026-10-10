// The translations editor in item forms, and the guided "make translatable" set-up in the data model.
import assert from 'node:assert/strict';
import { test } from 'node:test';

import { FakeEl, findAll, fire, install } from './fakedom.mjs';

const body = install();
globalThis.location = { hash: '', reload() { this.reloaded = true; } };
const realSetTimeout = globalThis.setTimeout;
globalThis.setTimeout = (fn, ms) => {
  const t = realSetTimeout(fn, ms);
  t.unref?.();
  return t;
};
const wait = (ms) => new Promise((r) => realSetTimeout(r, ms));

const LANGS = [
  { id: 'L-it', code: 'it-IT', name: 'Italiano' },
  { id: 'L-en', code: 'en-US', name: 'English' },
  { id: 'L-fr', code: 'fr-FR', name: 'Français' },
];
const MODEL = [
  { name: 'articles', fields: [{ name: 'slug', type: 'string', required: true }, { name: 'translations', type: 'translations', relation: { related: 'articles_translations', foreign_field: 'articles_id', language_field: 'languages_id' } }] },
  {
    name: 'articles_translations',
    system_fields: false,
    meta: { hidden: true },
    fields: [
      { name: 'articles_id', type: 'm2o', required: true, relation: { related: 'articles', on_delete: 'cascade' } },
      { name: 'languages_id', type: 'm2o', required: true, unique: true, unique_with: ['articles_id'], relation: { related: 'languages', on_delete: 'restrict' } },
      { name: 'title', type: 'string', required: true },
      { name: 'body', type: 'text' },
    ],
  },
  { name: 'languages', fields: [{ name: 'code', type: 'string', required: true, unique: true }, { name: 'name', type: 'string' }] },
];

const calls = [];
let languagesReply = () => ({ status: 200, json: { data: LANGS } });
globalThis.fetch = async (url, init = {}) => {
  const method = init.method || 'GET';
  const u = new URL(String(url), 'http://x');
  calls.push({ method, path: u.pathname, query: u.search, body: init.body ? JSON.parse(init.body) : undefined });
  let r;
  if (u.pathname === '/items/languages') r = languagesReply();
  else if (u.pathname === '/collections') r = { status: 200, json: { data: MODEL } };
  else if (u.pathname === '/collections/articles' && method === 'GET') r = { status: 200, json: { data: MODEL[0] } };
  else if (u.pathname === '/collections/articles/translations') r = { status: 200, json: { data: {}, meta: { applied: !u.searchParams.get('dry_run'), changes: ['+ collection articles_translations (4 fields)', '+ field articles.translations (translations)'], sql: ['CREATE TABLE "articles_translations" (...)'] } } };
  else throw new Error(`unmocked ${method} ${u.pathname}`);
  return { ok: r.status < 400, status: r.status, text: async () => JSON.stringify(r.json) };
};

const S = await import('../js/schema.js');
const { renderForm, clearOptionCache, FormError } = await import('../js/form.js');
const { modelCollection } = await import('../js/views/model.js');
S.setCollections(MODEL);

const item = (rows) => ({ id: 'A1', slug: 'hello', translations: rows });
const row = (lang, extra) => ({ id: `T-${lang}`, articles_id: 'A1', languages_id: lang, ...extra });
const tabs = (root) => findAll(root, (n) => n.tagName === 'BUTTON' && n.attrs.role === 'tab');
const tabNames = (root) => tabs(root).map((t) => t.attrs['data-language']);
const activeTab = (root) => tabs(root).find((t) => t.className.includes('active'))?.attrs['data-language'];
const inputs = (root, pred = () => true) => findAll(root, (n) => (n.tagName === 'INPUT' || n.tagName === 'TEXTAREA') && pred(n));
const typeInto = async (el, value) => {
  el.value = value;
  await fire(el, 'input');
};
const panel = (root) => findAll(root, (n) => n.attrs.role === 'tabpanel')[0];
const panelInputs = (root) => inputs(panel(root));
const addSelect = (root) => findAll(root, (n) => n.tagName === 'SELECT' && n.attrs['aria-label'] === 'Add a language')[0];
const mount = async (rows, opts) => {
  clearOptionCache();
  const form = renderForm('articles', item(rows), opts);
  const root = new FakeEl('div');
  root.append(form.el);
  await wait(20);
  return { form, root };
};

test('one tab per existing translation, the first one open, languages still to add offered', async () => {
  const { root } = await mount([row('L-en', { title: 'Hello' }), row('L-it', { title: 'Ciao', body: 'Corpo' })]);
  assert.deepEqual(tabNames(root), ['en-US', 'it-IT'], 'sorted by language code');
  assert.equal(activeTab(root), 'en-US');
  const add = addSelect(root);
  const offered = findAll(add, (n) => n.tagName === 'OPTION').map((o) => o.text);
  assert.deepEqual(offered, ['+ Add a language', 'fr-FR — Français'], 'only the languages without a translation');
  assert.equal(findAll(root, (n) => n.tagName === 'INPUT' && n.value === 'Hello').length, 1, 'the open tab shows its fields');
});

test('the link fields (item and language) are not shown in a translation', async () => {
  const { root } = await mount([row('L-it', { title: 'Ciao' })]);
  const labels = findAll(root, (n) => n.tagName === 'LABEL').map((l) => l.text.replace(/\s*\*$/, '').trim());
  assert.ok(labels.includes('Title') && labels.includes('Body'));
  assert.ok(!labels.includes('Articles Id') && !labels.includes('Languages Id'), labels.join('|'));
});

test('the value written back is keyed by language code', async () => {
  const { form, root } = await mount([row('L-en', { title: 'Hello' }), row('L-it', { title: 'Ciao', body: 'Corpo' })]);
  await typeInto(inputs(root, (n) => n.value === 'Hello')[0], 'Hello again');
  const payload = form.collect();
  assert.equal(payload.slug, 'hello');
  assert.deepEqual(Object.keys(payload.translations).sort(), ['en-US', 'it-IT']);
  assert.equal(payload.translations['en-US'].title, 'Hello again');
  assert.equal(payload.translations['it-IT'].title, 'Ciao');
  assert.equal(payload.translations['it-IT'].body, 'Corpo');
  assert.ok(!('articles_id' in payload.translations['it-IT']) && !('languages_id' in payload.translations['it-IT']));
});

test('adding a language opens its tab; removing one saved earlier sends null, removing a new one sends nothing', async () => {
  const { form, root } = await mount([row('L-it', { title: 'Ciao' })]);
  const add = addSelect(root);
  add.value = 'L-fr';
  await fire(add, 'change');
  assert.deepEqual(tabNames(root), ['fr-FR', 'it-IT']);
  assert.equal(activeTab(root), 'fr-FR', 'the new tab is opened');
  await typeInto(panelInputs(root)[0], 'Bonjour');
  let payload = form.collect();
  assert.equal(payload.translations['fr-FR'].title, 'Bonjour');
  assert.equal(payload.translations['it-IT'].title, 'Ciao');

  // a language added by mistake and removed again leaves no trace
  const removeNew = findAll(root, (n) => n.tagName === 'BUTTON' && n.text === 'Remove fr-FR')[0];
  await fire(removeNew, 'click');
  payload = form.collect();
  assert.deepEqual(Object.keys(payload.translations), ['it-IT']);

  // removing a translation that exists on the server is an explicit null
  await fire(findAll(root, (n) => n.tagName === 'BUTTON' && n.text === 'Remove it-IT')[0], 'click');
  payload = form.collect();
  assert.deepEqual(payload.translations, { 'it-IT': null });
  assert.match(root.text, /No translations yet/);
  // and it can be added back: the pending deletion is cancelled
  const again = addSelect(root);
  again.value = 'L-it';
  await fire(again, 'change');
  await typeInto(panelInputs(root)[0], 'Ciao di nuovo');
  assert.equal(form.collect().translations['it-IT'].title, 'Ciao di nuovo');
});

test('a required translatable field is enforced for the languages that are present', async () => {
  const { form, root } = await mount([row('L-it', { title: 'Ciao' })]);
  await typeInto(inputs(root, (n) => n.value === 'Ciao')[0], '');
  assert.throws(() => form.collect(), (e) => e instanceof FormError && /Title/.test(e.message));
});

test('a new item starts with no translations; nothing is sent until a language is added', async () => {
  clearOptionCache();
  const form = renderForm('articles', null);
  const root = new FakeEl('div');
  root.append(form.el);
  await wait(20);
  assert.deepEqual(tabNames(root), []);
  assert.match(root.text, /No translations yet/);
  await typeInto(inputs(root, (n) => n.tagName === 'INPUT')[0], 'new-slug');
  assert.deepEqual(form.collect().translations, {}, 'an empty map: the API ignores it');
});

test('read-only: no add, no remove, nothing written back', async () => {
  const { form, root } = await mount([row('L-it', { title: 'Ciao' })], {});
  // read-only through field meta
  clearOptionCache();
  const field = MODEL[0].fields[1];
  field.meta = { readonly: true };
  const ro = renderForm('articles', item([row('L-it', { title: 'Ciao' })]));
  const r2 = new FakeEl('div');
  r2.append(ro.el);
  await wait(20);
  delete field.meta;
  assert.equal(addSelect(r2), undefined);
  assert.equal(findAll(r2, (n) => n.tagName === 'BUTTON' && n.text.startsWith('Remove')).length, 0);
  assert.ok(!('translations' in ro.collect()), 'a read-only field is never written');
  assert.ok(form && root);
});

test('without languages the editor says where to add them; a failing load is reported', async () => {
  languagesReply = () => ({ status: 200, json: { data: [] } });
  let { root } = await mount([]);
  assert.match(root.text, /There are no languages yet/);
  assert.ok(findAll(root, (n) => n.tagName === 'A' && n.attrs.href === '#/content/languages')[0], 'links to the languages collection');
  languagesReply = () => ({ status: 500, json: { errors: [{ message: 'boom' }] } });
  ({ root } = await mount([]));
  assert.match(root.text, /Cannot load the languages: boom/);
  languagesReply = () => ({ status: 200, json: { data: LANGS } });
});

test('a translation in a language that no longer exists is not shown (and not touched)', async () => {
  const { form, root } = await mount([row('L-gone', { title: 'Orphan' }), row('L-it', { title: 'Ciao' })]);
  assert.deepEqual(tabNames(root), ['it-IT']);
  assert.deepEqual(Object.keys(form.collect().translations), ['it-IT']);
});

// ---- guided set-up ---------------------------------------------------------------------------------------------------------------------------
test('"New field" of type translations is a guided set-up that posts to /collections/:name/translations', async () => {
  const root = new FakeEl('div');
  await modelCollection(root, { name: 'articles' });
  const newField = findAll(root, (n) => n.tagName === 'BUTTON' && n.text === '+ New field')[0];
  await fire(newField, 'click');
  const dlg = body.children.at(-1);
  const type = findAll(dlg, (n) => n.tagName === 'SELECT').find((s) => findAll(s, (o) => o.tagName === 'OPTION').some((o) => o.value === 'translations'));
  type.value = 'translations';
  await fire(type, 'change');
  assert.match(dlg.text, /This creates, in one step/);
  assert.match(dlg.text, /Translatable fields/);
  assert.ok(!/Required \(NOT NULL\)/.test(dlg.text), 'an alias has no column options');
  const name = findAll(dlg, (n) => n.tagName === 'INPUT' && n.attrs.placeholder === 'field_name').at(-1);
  // the first placeholder belongs to the field name; the others to the translatable fields
  const fieldName = findAll(dlg, (n) => n.tagName === 'INPUT' && n.attrs.placeholder === 'field_name')[0];
  await typeInto(fieldName, 'texts');
  assert.ok(name);
  const langs = findAll(dlg, (n) => n.tagName === 'TEXTAREA' && n.attrs.name === 'languages')[0];
  await typeInto(langs, 'it-IT Italiano\nen-US English\nfr-FR');

  calls.length = 0;
  const save = findAll(dlg, (n) => n.tagName === 'BUTTON' && n.text === 'Save')[0];
  save.handlers.click[0]({ target: save });
  await wait(40);
  const dry = calls.find((c) => c.path === '/collections/articles/translations');
  assert.ok(dry && /dry_run=true/.test(dry.query), 'previewed first');
  assert.deepEqual(dry.body.fields, [{ name: 'title', type: 'string' }, { name: 'body', type: 'text' }]);
  assert.equal(dry.body.field_name, 'texts');
  assert.deepEqual(dry.body.languages, [{ code: 'it-IT', name: 'Italiano' }, { code: 'en-US', name: 'English' }, { code: 'fr-FR', name: 'fr-FR' }], 'a language without a name is named after its code');
  assert.ok(!('translations_collection' in dry.body) && !('languages_collection' in dry.body), 'defaults are left to the server');
  const preview = body.children.at(-1);
  assert.match(preview.text, /\+ collection articles_translations/);
  await fire(findAll(preview, (n) => n.tagName === 'BUTTON' && n.text === 'Apply')[0], 'click');
  await wait(40);
  const applied = calls.filter((c) => c.path === '/collections/articles/translations');
  assert.equal(applied.length, 2);
  assert.ok(!/dry_run/.test(applied[1].query));
});

test('the guided set-up needs a name and at least one translatable field', async () => {
  const root = new FakeEl('div');
  await modelCollection(root, { name: 'articles' });
  await fire(findAll(root, (n) => n.tagName === 'BUTTON' && n.text === '+ New field')[0], 'click');
  const dlg = body.children.at(-1);
  const type = findAll(dlg, (n) => n.tagName === 'SELECT')[0];
  type.value = 'translations';
  await fire(type, 'change');
  calls.length = 0;
  let save = findAll(dlg, (n) => n.tagName === 'BUTTON' && n.text === 'Save')[0];
  await fire(save, 'click');
  assert.equal(calls.filter((c) => c.path.endsWith('/translations')).length, 0, 'no name: nothing sent');
  const fieldName = findAll(dlg, (n) => n.tagName === 'INPUT' && n.attrs.placeholder === 'field_name')[0];
  await typeInto(fieldName, 'texts');
  // remove the translatable fields one by one (the list is redrawn after each removal)
  for (let remove = findAll(dlg, (n) => n.tagName === 'BUTTON' && n.text === '✕')[0]; remove; remove = findAll(dlg, (n) => n.tagName === 'BUTTON' && n.text === '✕')[0]) await fire(remove, 'click');
  save = findAll(dlg, (n) => n.tagName === 'BUTTON' && n.text === 'Save')[0];
  await fire(save, 'click');
  assert.equal(calls.filter((c) => c.path.endsWith('/translations')).length, 0, 'no translatable field: nothing sent');
});

test('an existing translations field is edited by hand (no guided set-up)', async () => {
  const root = new FakeEl('div');
  await modelCollection(root, { name: 'articles' });
  const row = findAll(root, (n) => n.tagName === 'TR' && n.text.includes('translations') && n.text.includes('articles_translations'))[0];
  assert.ok(row, 'listed with its related collection');
  await fire(row, 'click');
  const dlg = body.children.at(-1);
  assert.match(dlg.text, /Translations collection/);
  assert.match(dlg.text, /Language field/);
  assert.match(dlg.text, /Language code field/);
  assert.ok(!/This creates, in one step/.test(dlg.text));
});
