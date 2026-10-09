// Automations: list, editor (trigger + operations), run history.
// Saved through the plan/apply pipeline, so flows are promoted between
// environments together with the model.

import { api } from '../api.js';
import { add, applyWithPreview, button, checkbox, confirmDialog, h, inputEl, labeled, modal, replace, selectEl, toast } from '../dom.js';
import * as S from '../schema.js';
import { cleanFlow, csv, OP_TEMPLATES, OP_TYPES, presets, TRIGGER_TYPES, triggerSummary } from '../flows-util.js';
import { newId, parseJson } from '../util.js';

const clone = (v) => JSON.parse(JSON.stringify(v));

async function save(title, run, root) {
  const res = await applyWithPreview(title, run);
  if (res) {
    toast('Saved', 'ok');
    root.replaceChildren();
    await flowsView(root);
  }
  return res;
}

export async function flowsView(root) {
  const [flows, roles] = await Promise.all([api.get('/flows'), api.get('/roles')]);
  const rows = flows.data.map((f) =>
    h(
      'tr',
      { class: 'clickable', onClick: () => flowEditor(root, f, roles.data) },
      h('td', {}, f.name),
      h('td', {}, f.trigger?.type || ''),
      h('td', { class: 'small' }, triggerSummary(f)),
      h('td', {}, String((f.operations || []).length)),
      h('td', {}, f.active === false ? h('span', { class: 'muted' }, 'off') : 'on'),
    ),
  );
  add(root, 
    h('div', { class: 'page-head' }, h('h1', {}, 'Automations'), h('span', { class: 'spacer' }), button('+ New flow', () => newFlow(root, roles.data), 'primary')),
    h('div', { class: 'table-wrap' }, h('table', {}, h('thead', {}, h('tr', {}, ['Name', 'Trigger', 'When', 'Steps', 'Active'].map((t) => h('th', {}, t)))), h('tbody', {}, rows.length ? rows : [h('tr', {}, h('td', { colspan: 5, class: 'empty' }, 'No flows yet'))]))),
    h(
      'p',
      { class: 'muted' },
      'Flows cover webhooks, hooks (before/after a write), scheduled jobs, custom endpoints and manual actions. They are part of the schema snapshot: promote them between environments with "ferro schema". Secrets belong in FERRO_FLOW_ENV_<NAME> variables, referenced as {{ $env.NAME }}.',
    ),
  );
}

function newFlow(root, roles) {
  const first = S.userCollections().find((c) => !c.meta?.hidden)?.name || 'collection';
  const list = presets(newId, first);
  const picker = modal({
    title: 'New flow',
    body: h(
      'div',
      { class: 'tiles' },
      list.map((p) =>
        h(
          'button',
          {
            type: 'button',
            class: 'tile',
            onClick: () => {
              picker.close();
              flowEditor(root, p.build(), roles, true);
            },
          },
          h('strong', {}, p.label),
          h('span', { class: 'muted small' }, p.description),
        ),
      ),
    ),
    buttons: [{ label: 'Cancel' }],
  });
}

function defaultsFor(type) {
  switch (type) {
    case 'event':
      return { type, events: [] };
    case 'schedule':
      return { type, cron: '0 * * * *' };
    case 'webhook':
      return { type, methods: ['POST'], access: 'authenticated' };
    default:
      return { type: 'manual', collections: [] };
  }
}

function flowEditor(root, existing, roles, isNew = false) {
  const flow = clone(existing);
  flow.operations ||= [];
  flow.trigger ||= defaultsFor('event');
  // option texts live next to the model so invalid JSON is reported on save
  const optText = new Map(flow.operations.map((o) => [o, JSON.stringify(o.options ?? {}, null, 2)]));
  let outputText = flow.output === undefined ? '' : JSON.stringify(flow.output, null, 2);

  const triggerBox = h('div');
  const opsBox = h('div', { class: 'rows' });

  function drawTrigger() {
    const t = flow.trigger;
    const parts = [
      labeled(
        'Trigger',
        selectEl(TRIGGER_TYPES, t.type, (v) => {
          flow.trigger = defaultsFor(v);
          drawTrigger();
        }),
      ),
    ];
    if (t.type === 'event') {
      parts.push(
        labeled('Events (comma separated)', inputEl((t.events || []).join(', '), (v) => (t.events = csv(v)), { placeholder: 'pages.items.create, *.items.update' }), '<collection>.items.<create|update|delete|*>; * matches any collection.'),
        checkbox('Blocking: run before the write, may reject it or rewrite the payload (use "fail" and Output)', t.blocking, (v) => (t.blocking = v)),
      );
    } else if (t.type === 'schedule') {
      parts.push(labeled('Cron (UTC)', inputEl(t.cron, (v) => (t.cron = v.trim())), 'minute hour day month weekday, e.g. */15 * * * *'));
    } else if (t.type === 'webhook') {
      parts.push(
        labeled('Slug', inputEl(t.slug, (v) => (t.slug = v.trim() || undefined)), `Endpoint: /x/<slug>. Always available at /hooks/${String(flow.id).slice(0, 8)}…`),
        labeled('Methods', inputEl((t.methods || []).join(', '), (v) => (t.methods = csv(v).map((m) => m.toUpperCase())), { placeholder: 'POST' })),
        labeled('Access', selectEl(['public', 'authenticated', 'admin'], t.access || 'authenticated', (v) => (t.access = v))),
        labeled('Secret variable', inputEl(t.secret_env, (v) => (t.secret_env = v.trim() || undefined), { placeholder: 'MY_HOOK_SECRET' }), 'Name of an environment variable; callers send its value in the X-Ferro-Secret header.'),
        checkbox('Answer 202 immediately and run in the background', t.run_async, (v) => (t.run_async = v)),
      );
    } else {
      const chosen = new Set(t.roles || []);
      parts.push(
        labeled('Collections (empty = any)', inputEl((t.collections || []).join(', '), (v) => (t.collections = csv(v)))),
        h(
          'div',
          { class: 'field' },
          h('label', {}, 'Roles allowed (none = every app user)'),
          roles.map((r) =>
            checkbox(r.name, chosen.has(r.id), (on) => {
              if (on) chosen.add(r.id);
              else chosen.delete(r.id);
              t.roles = [...chosen];
            }),
          ),
        ),
      );
    }
    replace(triggerBox, ...parts);
  }

  function otherKeys(op) {
    return [{ value: '', label: '— end —' }, ...flow.operations.filter((o) => o !== op && o.key).map((o) => ({ value: o.key, label: o.key }))];
  }

  function drawOps() {
    replace(opsBox, 
      ...flow.operations.map((op, i) =>
        h(
          'div',
          { class: 'row-card' },
          h(
            'div',
            { class: 'row-head' },
            inputEl(op.key, (v) => (op.key = v.trim()), { placeholder: 'key', style: { width: '10rem' } }),
            selectEl(OP_TYPES, op.type, (v) => {
              op.type = v;
              optText.set(op, JSON.stringify(OP_TEMPLATES[v] ?? {}, null, 2));
              drawOps();
            }),
            h('span', { class: 'spacer' }),
            button('↑', () => move(i, -1), 'icon'),
            button('↓', () => move(i, 1), 'icon'),
            button(
              '✕',
              () => {
                flow.operations.splice(i, 1);
                optText.delete(op);
                drawOps();
              },
              'icon danger',
            ),
          ),
          h(
            'div',
            { class: 'field' },
            h('label', {}, 'Options (JSON, {{ path }} references the data chain)'),
            h('textarea', { rows: 6, class: 'mono', spellcheck: 'false', onInput: (e) => optText.set(op, e.target.value) }, optText.get(op) ?? '{}'),
          ),
          h(
            'div',
            { class: 'two-col' },
            labeled(op.type === 'condition' ? 'If it matches, continue with' : 'On success, continue with', selectEl(otherKeys(op), op.resolve || '', (v) => (op.resolve = v || undefined))),
            labeled(op.type === 'condition' ? 'If it does not match, continue with' : 'On failure, continue with', selectEl(otherKeys(op), op.reject || '', (v) => (op.reject = v || undefined))),
          ),
        ),
      ),
    );
  }

  function move(i, d) {
    const j = i + d;
    if (j < 0 || j >= flow.operations.length) return;
    [flow.operations[i], flow.operations[j]] = [flow.operations[j], flow.operations[i]];
    drawOps();
  }

  function build() {
    if (!flow.name?.trim()) throw new Error('Name is required');
    const out = clone(flow);
    out.operations = flow.operations.map((op) => {
      if (!op.key) throw new Error('Every operation needs a key');
      const { options: _ignored, ...rest } = clone(op);
      return { ...rest, options: parseJson(optText.get(op) ?? '{}', `Options of ${op.key}`) ?? {} };
    });
    out.output = outputText.trim() ? parseJson(outputText, 'Output') : undefined;
    return cleanFlow(out);
  }

  drawTrigger();
  drawOps();

  const dlg = modal({
    title: isNew ? 'New flow' : flow.name,
    wide: true,
    body: h(
      'div',
      {},
      labeled('Name', inputEl(flow.name, (v) => (flow.name = v))),
      labeled('Description', inputEl(flow.description, (v) => (flow.description = v || undefined))),
      h('div', { class: 'two-col' }, checkbox('Active', flow.active !== false, (v) => (flow.active = v)), labeled('Item operations run as', selectEl(['system', 'caller'], flow.run_as || 'system', (v) => (flow.run_as = v)), 'system = unrestricted; caller = permissions of the user who caused the run (blocking, webhook, manual).')),
      triggerBox,
      h('h4', {}, 'Operations'),
      opsBox,
      button(
        '+ Add operation',
        () => {
          const op = { key: `step${flow.operations.length + 1}`, type: 'log' };
          // operations run along resolve/reject links: chain the new one after the last
          const last = flow.operations.at(-1);
          if (last && !last.resolve && last.type !== 'fail') last.resolve = op.key;
          flow.operations.push(op);
          optText.set(op, JSON.stringify(OP_TEMPLATES.log, null, 2));
          drawOps();
        },
        'sm',
      ),
      labeled('Start with', selectEl([{ value: '', label: 'the first operation' }, ...flow.operations.map((o) => ({ value: o.key, label: o.key }))], flow.start || '', (v) => (flow.start = v || undefined))),
      labeled('Output (JSON template)', h('textarea', { rows: 4, class: 'mono', spellcheck: 'false', onInput: (e) => (outputText = e.target.value) }, outputText), 'Webhook: the HTTP response (default: last result). Blocking event: the new payload (default: unchanged).'),
    ),
    buttons: [
      !isNew
        ? {
            label: 'Delete',
            class: 'danger',
            close: false,
            onClick: async () => {
              if (!(await confirmDialog(`Delete flow "${existing.name}"?`, { danger: true, label: 'Delete' }))) return;
              if (await save(`Delete flow ${existing.name}`, (q) => api.del(`/flows/${existing.id}`, q), root)) dlg.close();
            },
          }
        : { label: 'Cancel' },
      !isNew ? { label: 'Runs', close: false, onClick: () => showRuns(existing) } : { label: 'Close' },
      !isNew && flow.trigger.type === 'manual'
        ? {
            label: 'Run now',
            close: false,
            onClick: async () => {
              const r = await api.post(`/flows/${existing.id}/run`, {});
              toast(r.meta.ok ? 'Flow completed' : `Flow failed: ${r.meta.error}`, r.meta.ok ? 'ok' : 'error');
            },
          }
        : null,
      {
        label: 'Save',
        class: 'primary',
        close: false,
        onClick: async () => {
          const body = build();
          if (await save(`Save flow ${body.name}`, (q) => api.put(`/flows/${flow.id}`, body, q), root)) dlg.close();
        },
      },
    ].filter(Boolean),
  });
}

async function showRuns(flow) {
  const r = await api.get(`/flows/${flow.id}/runs`, { limit: 50 });
  const rows = r.data.map((run) =>
    h(
      'tr',
      {},
      h('td', {}, new Date(run.started).toLocaleString()),
      h('td', {}, run.source),
      h('td', { class: run.status === 'ok' ? '' : 'bad' }, run.status),
      h('td', { class: 'small' }, run.error || ''),
      h('td', {}, h('details', {}, h('summary', {}, `${(run.log || []).length} steps`), h('pre', { class: 'small' }, JSON.stringify(run.log, null, 2)))),
    ),
  );
  modal({
    title: `Runs of ${flow.name}`,
    wide: true,
    body: rows.length ? h('table', {}, h('thead', {}, h('tr', {}, ['Started', 'Source', 'Status', 'Error', 'Log'].map((t) => h('th', {}, t)))), h('tbody', {}, rows)) : h('p', { class: 'muted' }, 'No runs recorded (kept for 30 days).'),
    buttons: [{ label: 'Close' }],
  });
}

