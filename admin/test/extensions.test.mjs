import assert from 'node:assert/strict';
import { test } from 'node:test';

import { COLLECTIONS } from './fixture.mjs';
import { FakeEl, findAll, install } from './fakedom.mjs';

install();
globalThis.fetch = async () => ({ ok: true, status: 200, text: async () => JSON.stringify({ data: [] }) });

const S = await import('../js/schema.js');
const { registry, resetRegistry, itemActionsFor } = await import('../js/registry.js');
const { loadExtensions } = await import('../js/extensions.js');
const { renderForm } = await import('../js/form.js');
const { cleanFlow, csv, presets, triggerSummary, OP_TYPES, OP_TEMPLATES } = await import('../js/flows-util.js');

test('extensions register pages, interfaces and item actions; failures are isolated', async () => {
  resetRegistry();
  const modules = {
    'http://x/ok.js': {
      default(ferro) {
        ferro.addPage({ id: 'hello', title: 'Hello', render: (root) => root.append(ferro.dom.h('p', {}, 'hi')) });
        ferro.addInterface('stars', (field, value) => {
          const el = ferro.dom.h('input', { type: 'number', value: value ?? 0 });
          return { el, get: () => Number(el.value) };
        });
        ferro.addItemAction({ label: 'Ping', collections: ['pages'], run: async () => 'pong' });
      },
    },
    'http://x/bad.js': {
      default() {
        throw new Error('boom');
      },
    },
    'http://x/nofn.js': { default: 42 },
  };
  const report = await loadExtensions({
    list: [
      { id: 'ok', name: 'Ok', url: 'http://x/ok.js' },
      { id: 'bad', name: 'Bad', url: 'http://x/bad.js' },
      { id: 'nofn', name: 'NoFn', url: 'http://x/nofn.js' },
    ],
    importer: async (url) => modules[url],
  });
  assert.deepEqual(report.map((r) => [r.id, r.ok]), [['ok', true], ['bad', false], ['nofn', false]]);
  assert.match(report[1].error, /boom/);
  assert.equal(registry.pages.length, 1);

  const page = new FakeEl('div');
  registry.pages[0].render(page);
  assert.equal(page.text, 'hi');
  assert.equal(itemActionsFor('pages').length, 1);
  assert.equal(itemActionsFor('tags').length, 0);
});

test('a registered interface replaces the built-in widget', () => {
  S.setCollections([{ name: 'movies', meta: {}, fields: [{ name: 'rating', type: 'integer', meta: { interface: 'stars' } }, { name: 'title', type: 'string' }] }]);
  const form = renderForm('movies', { id: 'm1', rating: 4, title: 'x' });
  const custom = findAll(form.el, (n) => n.tagName === 'INPUT' && n.attrs.type === 'number');
  assert.equal(custom.length, 1);
  custom[0].value = '5';
  assert.deepEqual(form.collect(), { rating: 5, title: 'x' });
  S.setCollections(COLLECTIONS);
});

test('a broken interface falls back to the built-in widget', () => {
  registry.interfaces.broken = () => {
    throw new Error('nope');
  };
  S.setCollections([{ name: 'movies', meta: {}, fields: [{ name: 'rating', type: 'integer', meta: { interface: 'broken' } }] }]);
  const form = renderForm('movies', { id: 'm1', rating: 3 });
  assert.deepEqual(form.collect(), { rating: 3 });
  delete registry.interfaces.broken;
  S.setCollections(COLLECTIONS);
});

test('invalid registrations are rejected', async () => {
  resetRegistry();
  await loadExtensions({
    list: [{ id: 'e', name: 'E', url: 'http://x/e.js' }],
    importer: async () => ({
      default(ferro) {
        assert.throws(() => ferro.addPage({ id: 'Bad Id', render() {} }));
        assert.throws(() => ferro.addPage({ id: 'ok' }));
        assert.throws(() => ferro.addInterface('', () => {}));
        assert.throws(() => ferro.addItemAction({ label: 'x' }));
        ferro.addPage({ id: 'p', title: 'P', render() {} });
        assert.throws(() => ferro.addPage({ id: 'p', render() {} }), /already exists/);
      },
    }),
  });
  assert.equal(registry.loaded[0].ok, true);
  resetRegistry();
});

test('flow helpers', () => {
  assert.equal(triggerSummary({ trigger: { type: 'event', events: ['a.items.create'], blocking: true } }), 'before a.items.create');
  assert.equal(triggerSummary({ trigger: { type: 'schedule', cron: '*/5 * * * *' } }), 'cron */5 * * * *');
  assert.equal(triggerSummary({ id: 'abcdef012345', trigger: { type: 'webhook', slug: 'hello', methods: ['GET'] } }), 'GET /x/hello');
  assert.match(triggerSummary({ id: 'abcdef012345', trigger: { type: 'webhook', run_async: true } }), /^POST \/hooks\/abcdef01….*\(async\)$/);
  assert.deepEqual(csv(' a, b ,, c'), ['a', 'b', 'c']);
  assert.ok(OP_TYPES.every((t) => t in OP_TEMPLATES));

  let n = 0;
  const list = presets(() => `id-${++n}`, 'pages');
  assert.deepEqual(list.map((p) => p.key), ['webhook', 'hook', 'schedule', 'endpoint', 'manual']);
  const hook = list.find((p) => p.key === 'hook').build();
  assert.equal(hook.trigger.blocking, true);
  assert.equal(hook.trigger.events[0], 'pages.items.create');
  assert.equal(hook.operations.at(-1).type, 'fail');
  const keys = hook.operations.map((o) => o.key);
  assert.ok(hook.operations.every((o) => [o.resolve, o.reject].filter(Boolean).every((k) => keys.includes(k))));

  const cleaned = cleanFlow({
    id: 'x',
    name: 'n',
    description: '',
    active: true,
    run_as: 'system',
    start: '',
    trigger: { type: 'webhook', slug: '', methods: [], access: 'authenticated', secret_env: '', run_async: false },
    operations: [{ key: 'a', type: 'log', options: { message: 'm' }, resolve: '', reject: undefined }],
    output: '',
  });
  assert.deepEqual(cleaned, { id: 'x', name: 'n', trigger: { type: 'webhook' }, operations: [{ key: 'a', type: 'log', options: { message: 'm' } }] });
});
