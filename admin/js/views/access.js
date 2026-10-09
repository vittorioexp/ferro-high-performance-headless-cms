// Roles and policies (with their permissions). Saved through the same
// plan/apply pipeline as the model, so the change is previewed first.

import { api } from '../api.js';
import { add, applyWithPreview, button, checkbox, confirmDialog, fail, h, inputEl, labeled, modal, replace, selectEl, toast } from '../dom.js';
import * as S from '../schema.js';
import { newId, parseJson } from '../util.js';

const ACTIONS = ['create', 'read', 'update', 'delete'];
const clone = (v) => JSON.parse(JSON.stringify(v));

async function save(title, run, root) {
  const res = await applyWithPreview(title, run);
  if (res) {
    toast('Saved', 'ok');
    root.replaceChildren();
    await accessHome(root);
  }
  return res;
}

export async function accessHome(root) {
  const [roles, policies] = await Promise.all([api.get('/roles'), api.get('/policies')]);
  const policyName = (id) => policies.data.find((p) => p.id === id)?.name || id.slice(0, 8);

  const roleRows = roles.data.map((r) =>
    h(
      'tr',
      { class: 'clickable', onClick: () => roleEditor(root, r, policies.data) },
      h('td', {}, r.name),
      h('td', {}, (r.policies || []).map((id) => h('span', { class: 'chip' }, policyName(id)))),
    ),
  );
  const policyRows = policies.data.map((p) =>
    h(
      'tr',
      { class: 'clickable', onClick: () => policyEditor(root, p) },
      h('td', {}, p.name),
      h('td', {}, [p.admin_access ? 'admin' : null, p.app_access ? 'app' : null, p.public ? 'public' : null].filter(Boolean).join(', ')),
      h('td', {}, String((p.permissions || []).length)),
    ),
  );

  add(root, 
    h(
      'div',
      { class: 'page-head' },
      h('h1', {}, 'Access control'),
    ),
    h(
      'div',
      { class: 'card' },
      h('div', { class: 'page-head' }, h('h3', {}, 'Roles'), h('span', { class: 'spacer' }), button('+ New role', () => roleEditor(root, null, policies.data), 'primary sm')),
      h('table', {}, h('thead', {}, h('tr', {}, ['Name', 'Policies'].map((t) => h('th', {}, t)))), h('tbody', {}, roleRows)),
    ),
    h(
      'div',
      { class: 'card' },
      h('div', { class: 'page-head' }, h('h3', {}, 'Policies'), h('span', { class: 'spacer' }), button('+ New policy', () => policyEditor(root, null), 'primary sm')),
      h('table', {}, h('thead', {}, h('tr', {}, ['Name', 'Flags', 'Permissions'].map((t) => h('th', {}, t)))), h('tbody', {}, policyRows)),
    ),
    h('p', { class: 'muted' }, 'Roles and policies have stable ids and are part of the schema snapshot: they are promoted between environments together with the model.'),
  );
}

function roleEditor(root, existing, policies) {
  const role = existing ? clone(existing) : { id: newId(), name: '', policies: [] };
  const chosen = new Set(role.policies || []);
  const dlg = modal({
    title: existing ? `Role ${existing.name}` : 'New role',
    body: h(
      'div',
      {},
      labeled('Name', inputEl(role.name, (v) => (role.name = v))),
      labeled('Description', inputEl(role.description, (v) => (role.description = v || undefined))),
      h(
        'div',
        { class: 'field' },
        h('label', {}, 'Policies'),
        policies.map((p) =>
          checkbox(p.name, chosen.has(p.id), (on) => {
            if (on) chosen.add(p.id);
            else chosen.delete(p.id);
          }),
        ),
      ),
    ),
    buttons: [
      existing && existing.id !== '00000000-0000-7000-8000-000000000002'
        ? {
            label: 'Delete',
            class: 'danger',
            close: false,
            onClick: async () => {
              if (!(await confirmDialog(`Delete role "${existing.name}"?`, { danger: true, label: 'Delete' }))) return;
              if (await save(`Delete role ${existing.name}`, (q) => api.del(`/roles/${existing.id}`, q), root)) dlg.close();
            },
          }
        : { label: 'Cancel' },
      {
        label: 'Save',
        class: 'primary',
        close: false,
        onClick: async () => {
          if (!role.name) throw new Error('Name is required');
          role.policies = [...chosen];
          if (await save(`Save role ${role.name}`, (q) => api.put(`/roles/${role.id}`, role, q), root)) dlg.close();
        },
      },
    ],
  });
}

function emptyPermission() {
  return { collection: '', action: 'read', fields: ['*'] };
}

function policyEditor(root, existing) {
  const policy = existing ? clone(existing) : { id: newId(), name: '', admin_access: false, app_access: false, public: false, permissions: [] };
  policy.permissions ||= [];
  const collections = ['*', ...S.userCollections().map((c) => c.name), 'ferro_files', 'ferro_folders', 'ferro_users'];
  const isAdminPolicy = policy.id === '00000000-0000-7000-8000-000000000001';

  const permBox = h('div', { class: 'perm-list' });
  const draw = () =>
    replace(permBox, 
      ...policy.permissions.map((p, i) => {
        // JSON editors keep their raw text until the policy is saved
        const jsonArea = (key, label) =>
          labeled(
            label,
            h(
              'textarea',
              { rows: 2, class: 'mono', onInput: (e) => (p[`_${key}`] = e.target.value) },
              p[key] ? JSON.stringify(p[key]) : '',
            ),
          );
        return h(
          'div',
          { class: 'row-card' },
          h(
            'div',
            { class: 'row-head' },
            selectEl([{ value: '', label: '— collection —' }, ...collections.map((c) => ({ value: c, label: c }))], p.collection, (v) => (p.collection = v)),
            selectEl(ACTIONS, p.action, (v) => (p.action = v)),
            h('span', { class: 'spacer' }),
            button(
              '✕',
              () => {
                policy.permissions.splice(i, 1);
                draw();
              },
              'icon danger',
            ),
          ),
          labeled('Fields (comma separated, * = all)', inputEl((p.fields || ['*']).join(', '), (v) => (p.fields = v.split(',').map((s) => s.trim()).filter(Boolean)))),
          h('details', {}, h('summary', {}, 'Item filter, validation, presets'), jsonArea('filter', 'Item filter (JSON, supports $CURRENT_USER)'), jsonArea('validation', 'Payload validation (JSON)'), jsonArea('presets', 'Presets for create (JSON)')),
        );
      }),
    );
  draw();

  function build() {
    const out = clone(policy);
    out.permissions = out.permissions.map((p) => {
      const perm = { collection: p.collection, action: p.action, fields: p.fields?.length ? p.fields : ['*'] };
      if (!perm.collection) throw new Error('Every permission needs a collection');
      for (const key of ['filter', 'validation', 'presets']) {
        const raw = p[`_${key}`];
        const value = raw !== undefined ? parseJson(raw, key) : p[key];
        if (value) perm[key] = value;
      }
      return perm;
    });
    return out;
  }

  const dlg = modal({
    title: existing ? `Policy ${existing.name}` : 'New policy',
    wide: true,
    body: h(
      'div',
      {},
      labeled('Name', inputEl(policy.name, (v) => (policy.name = v))),
      checkbox('Administrator access (everything, including schema and sync)', policy.admin_access, (v) => (policy.admin_access = v)),
      checkbox('App access (may sign in to this admin app)', policy.app_access, (v) => (policy.app_access = v)),
      checkbox('Public (applies to unauthenticated requests too)', policy.public, (v) => (policy.public = v)),
      h('h4', {}, 'Permissions'),
      permBox,
      button(
        '+ Add permission',
        () => {
          policy.permissions.push(emptyPermission());
          draw();
        },
        'sm',
      ),
    ),
    buttons: [
      existing && !isAdminPolicy
        ? {
            label: 'Delete',
            class: 'danger',
            close: false,
            onClick: async () => {
              if (!(await confirmDialog(`Delete policy "${existing.name}"?`, { danger: true, label: 'Delete' }))) return;
              if (await save(`Delete policy ${existing.name}`, (q) => api.del(`/policies/${existing.id}`, q), root)) dlg.close();
            },
          }
        : { label: 'Cancel' },
      {
        label: 'Save',
        class: 'primary',
        close: false,
        onClick: async () => {
          if (!policy.name) throw new Error('Name is required');
          const body = build();
          if (await save(`Save policy ${body.name}`, (q) => api.put(`/policies/${body.id}`, body, q), root)) dlg.close();
        },
      },
    ],
  });
}
