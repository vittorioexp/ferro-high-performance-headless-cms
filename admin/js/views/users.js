// User management (administrators).

import { api } from '../api.js';
import { add, button, h, inputEl, labeled, modal, selectEl, toast } from '../dom.js';

export async function usersView(root) {
  const [users, roles] = await Promise.all([
    api.get('/users', { fields: 'id,email,first_name,last_name,role,status,last_access', sort: 'email', limit: 200 }),
    api.get('/roles'),
  ]);
  const roleName = (id) => roles.data.find((r) => r.id === id)?.name || '';
  const rows = users.data.map((u) =>
    h(
      'tr',
      { class: 'clickable', onClick: () => editor(u, roles.data) },
      h('td', {}, u.email),
      h('td', {}, [u.first_name, u.last_name].filter(Boolean).join(' ')),
      h('td', {}, roleName(u.role)),
      h('td', {}, u.status),
      h('td', {}, u.last_access ? new Date(u.last_access).toLocaleString() : '—'),
    ),
  );
  add(root, 
    h('div', { class: 'page-head' }, h('h1', {}, 'Users'), h('span', { class: 'spacer' }), button('+ New user', () => editor(null, roles.data), 'primary')),
    h('div', { class: 'table-wrap' }, h('table', {}, h('thead', {}, h('tr', {}, ['Email', 'Name', 'Role', 'Status', 'Last access'].map((t) => h('th', {}, t)))), h('tbody', {}, rows))),
  );
}

function editor(user, roles) {
  const draft = user
    ? { first_name: user.first_name || '', last_name: user.last_name || '', role: user.role || null, status: user.status, password: '' }
    : { email: '', password: '', first_name: '', last_name: '', role: null };
  const roleOptions = [{ value: '', label: '— no role —' }, ...roles.map((r) => ({ value: r.id, label: r.name }))];
  const dlg = modal({
    title: user ? user.email : 'New user',
    body: h(
      'div',
      {},
      user ? null : labeled('Email', inputEl('', (v) => (draft.email = v.trim()))),
      labeled('First name', inputEl(draft.first_name, (v) => (draft.first_name = v))),
      labeled('Last name', inputEl(draft.last_name, (v) => (draft.last_name = v))),
      labeled('Role', selectEl(roleOptions, draft.role || '', (v) => (draft.role = v || null))),
      user ? labeled('Status', selectEl(['active', 'suspended', 'invited'], draft.status, (v) => (draft.status = v))) : null,
      labeled(user ? 'New password (leave empty to keep)' : 'Password', inputEl('', (v) => (draft.password = v), { type: 'password', autocomplete: 'new-password' }), 'At least 8 characters.'),
      user
        ? h(
            'div',
            { class: 'field' },
            button(
              'Generate API token',
              async () => {
                const r = await api.post(`/users/${user.id}/token`);
                modal({
                  title: 'API token',
                  body: h('div', {}, h('p', {}, 'Copy it now: it is shown only once. Use it as Bearer token (CLI, CI, scripts).'), h('pre', { class: 'token' }, r.data.token)),
                  buttons: [{ label: 'Done' }],
                });
              },
              'sm',
            ),
          )
        : null,
    ),
    buttons: [
      user
        ? {
            label: 'Delete',
            class: 'danger',
            close: false,
            onClick: async () => {
              await api.del(`/users/${user.id}`);
              dlg.close();
              location.reload();
            },
          }
        : { label: 'Cancel' },
      {
        label: 'Save',
        class: 'primary',
        onClick: async () => {
          if (user) {
            const body = { first_name: draft.first_name, last_name: draft.last_name, role: draft.role, status: draft.status };
            if (draft.password) body.password = draft.password;
            await api.patch(`/users/${user.id}`, body);
          } else {
            if (!draft.email || draft.password.length < 8) throw new Error('Email and a password of at least 8 characters are required');
            await api.post('/users', draft);
          }
          toast('Saved', 'ok');
          location.reload();
        },
      },
    ],
  });
  return dlg;
}

