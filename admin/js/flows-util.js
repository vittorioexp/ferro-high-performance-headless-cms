// Pure helpers for the flows editor (no DOM, no network).

export const OP_TYPES = ['log', 'request', 'item_create', 'item_update', 'item_delete', 'item_read', 'condition', 'transform', 'trigger_flow', 'fail'];

/** Starting point for the options of each operation type. */
export const OP_TEMPLATES = {
  log: { message: 'Item {{ $trigger.key }} changed' },
  request: { method: 'POST', url: 'https://example.com/hook', headers: { Authorization: 'Bearer {{ $env.HOOK_TOKEN }}' }, body: { id: '{{ $trigger.key }}' } },
  item_create: { collection: 'collection', payload: { title: '{{ $trigger.payload.title }}' } },
  item_update: { collection: 'collection', ids: '{{ $trigger.key }}', payload: {} },
  item_delete: { collection: 'collection', ids: '{{ $trigger.keys }}' },
  item_read: { collection: 'collection', filter: {}, limit: 20 },
  condition: { rule: { $trigger: { payload: { status: { _eq: 'published' } } } } },
  transform: { value: { id: '{{ $trigger.key }}' } },
  trigger_flow: { flow: '00000000-0000-0000-0000-000000000000', payload: {} },
  fail: { message: 'Not allowed' },
};

export const TRIGGER_TYPES = ['event', 'schedule', 'webhook', 'manual'];

export function triggerSummary(flow) {
  const t = flow.trigger || {};
  switch (t.type) {
    case 'event':
      return `${t.blocking ? 'before' : 'after'} ${(t.events || []).join(', ')}`;
    case 'schedule':
      return `cron ${t.cron}`;
    case 'webhook': {
      const methods = (t.methods && t.methods.length ? t.methods : ['POST']).join('/');
      return `${methods} ${t.slug ? `/x/${t.slug}` : `/hooks/${String(flow.id).slice(0, 8)}…`}${t.run_async ? ' (async)' : ''}`;
    }
    case 'manual':
      return `manual${t.collections && t.collections.length ? ` on ${t.collections.join(', ')}` : ''}`;
    default:
      return '';
  }
}

/** Ready-made flows for the most common needs. `idGen` returns a fresh uuid. */
export function presets(idGen, collection = 'collection') {
  const base = (name, trigger, operations, extra = {}) => ({ id: idGen(), name, active: true, trigger, operations, ...extra });
  return [
    {
      key: 'webhook',
      label: 'Outbound webhook',
      description: 'Call an external URL when an item is created or updated. Keep secrets in FERRO_FLOW_ENV_* variables ({{ $env.NAME }}).',
      build: () =>
        base('Notify external system', { type: 'event', events: [`${collection}.items.create`, `${collection}.items.update`] }, [
          { key: 'notify', type: 'request', options: { ...OP_TEMPLATES.request, body: { event: '{{ $trigger.event }}', id: '{{ $trigger.key }}', item: '{{ $trigger.item }}' } } },
        ]),
    },
    {
      key: 'hook',
      label: 'Validation hook (blocking)',
      description: 'Runs before the write: rejects it (fail) or rewrites the payload (output).',
      build: () =>
        base(
          'Require a title',
          { type: 'event', events: [`${collection}.items.create`, `${collection}.items.update`], blocking: true },
          [
            {
              key: 'check',
              type: 'condition',
              // passes when `title` is not part of the payload (partial updates) or is not empty
              options: { rule: { _or: [{ $trigger: { payload: { title: { _null: true } } } }, { $trigger: { payload: { title: { _nempty: true } } } }] } },
              reject: 'deny',
            },
            { key: 'deny', type: 'fail', options: { message: 'The title cannot be empty' } },
          ],
        ),
    },
    {
      key: 'schedule',
      label: 'Scheduled job',
      description: 'Runs on a cron schedule (UTC).',
      build: () => base('Daily job', { type: 'schedule', cron: '0 6 * * *' }, [{ key: 'log', type: 'log', options: { message: 'Daily job ran' } }]),
    },
    {
      key: 'endpoint',
      label: 'Custom endpoint',
      description: 'Your own HTTP endpoint: /x/<slug>. The response is the flow output (default: the last result).',
      build: () =>
        base(
          'Hello endpoint',
          { type: 'webhook', slug: 'hello', methods: ['GET'], access: 'public' },
          [{ key: 'greet', type: 'transform', options: { value: { message: 'Hello {{ $trigger.query.name }}' } } }],
        ),
    },
    {
      key: 'manual',
      label: 'Manual action',
      description: 'A button on the item page of the chosen collections.',
      build: () => base('Reprocess item', { type: 'manual', collections: [collection] }, [{ key: 'log', type: 'log', options: { message: 'Reprocessing {{ $trigger.key }}' } }]),
    },
  ];
}

const isEmpty = (v) => v === undefined || v === null || v === '' || (Array.isArray(v) && v.length === 0);

/** Drop empty optional members so the stored YAML stays small. */
export function cleanFlow(flow) {
  const out = JSON.parse(JSON.stringify(flow));
  if (isEmpty(out.description)) delete out.description;
  if (isEmpty(out.start)) delete out.start;
  if (out.output === null || out.output === undefined || out.output === '') delete out.output;
  if (out.run_as === 'system') delete out.run_as;
  if (out.active === true) delete out.active;
  const t = out.trigger;
  for (const k of ['slug', 'secret_env', 'methods', 'collections', 'roles']) if (isEmpty(t[k])) delete t[k];
  for (const k of ['blocking', 'run_async']) if (!t[k]) delete t[k];
  if (t.access === 'authenticated') delete t.access;
  out.operations = (out.operations || []).map((op) => {
    const o = { ...op };
    if (isEmpty(o.resolve)) delete o.resolve;
    if (isEmpty(o.reject)) delete o.reject;
    if (o.options === null || o.options === undefined) delete o.options;
    return o;
  });
  return out;
}

export function csv(text) {
  return String(text || '')
    .split(',')
    .map((s) => s.trim())
    .filter(Boolean);
}
