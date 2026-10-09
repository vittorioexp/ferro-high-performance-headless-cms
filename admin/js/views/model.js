// Data model editor. Every save is previewed as a plan (dry run) before it is applied.

import { api } from '../api.js';
import { add, applyWithPreview, button, checkbox, confirmDialog, fail, h, inputEl, labeled, modal, replace, selectEl, toast } from '../dom.js';
import * as S from '../schema.js';
import { reloadCollections } from '../session.js';
import { parseJson } from '../util.js';

const go = (hash) => (location.hash = hash);
const clone = (v) => JSON.parse(JSON.stringify(v));

async function applyChange(title, run) {
  const res = await applyWithPreview(title, run);
  if (res) {
    await reloadCollections();
    toast('Model updated', 'ok');
  }
  return res;
}

// ---------------------------------------------------------------------------
// collections list
// ---------------------------------------------------------------------------

export async function modelHome(root) {
  const rows = S.userCollections().map((c) =>
    h(
      'tr',
      { class: 'clickable', onClick: () => go(`#/model/${encodeURIComponent(c.name)}`) },
      h('td', {}, c.name),
      h('td', {}, String((c.fields || []).length)),
      h('td', {}, [c.meta?.singleton ? 'singleton' : null, c.meta?.hidden ? 'hidden' : null, c.sync?.exclude ? 'not synced' : null].filter(Boolean).join(', ')),
    ),
  );
  add(root, 
    h('div', { class: 'page-head' }, h('h1', {}, 'Data model'), h('span', { class: 'spacer' }), button('+ New collection', newCollection, 'primary')),
    h('div', { class: 'table-wrap' }, h('table', {}, h('thead', {}, h('tr', {}, ['Collection', 'Fields', 'Flags'].map((t) => h('th', {}, t)))), h('tbody', {}, rows))),
    h('p', { class: 'muted' }, 'Changes are checked and shown as a plan before they are applied. Use "ferro schema pull" to commit the result to git.'),
  );
}

function newCollection() {
  const draft = { name: '', system_fields: true, meta: { icon: '', singleton: false } };
  modal({
    title: 'New collection',
    body: h(
      'div',
      {},
      labeled('Name', inputEl('', (v) => (draft.name = v.trim()), { placeholder: 'e.g. articles' }), 'Letters, digits and underscore.'),
      checkbox('Add date_created / user_created / date_updated / user_updated', true, (v) => (draft.system_fields = v)),
      checkbox('Singleton (a single item, e.g. site settings)', false, (v) => (draft.meta.singleton = v)),
    ),
    buttons: [
      { label: 'Cancel' },
      {
        label: 'Create',
        class: 'primary',
        onClick: async () => {
          if (!draft.name) throw new Error('Name is required');
          const body = { name: draft.name, system_fields: draft.system_fields, meta: { singleton: draft.meta.singleton }, fields: [] };
          const res = await applyChange(`Create ${draft.name}`, (q) => api.post('/collections', body, q));
          if (!res) return false;
          go(`#/model/${encodeURIComponent(draft.name)}`);
        },
      },
    ],
  });
}

// ---------------------------------------------------------------------------
// one collection
// ---------------------------------------------------------------------------

export async function modelCollection(root, { name }) {
  const c = S.getCollection(name);
  if (!c) throw new Error(`Collection "${name}" not found`);
  const draft = clone(c);
  draft.meta ||= {};
  draft.sync ||= {};

  const meta = h(
    'div',
    { class: 'card' },
    h('h3', {}, 'Settings'),
    labeled('Name', inputEl(draft.name, (v) => (draft.name = v.trim())), 'Renaming keeps the data, but relations pointing here must be updated.'),
    labeled('Icon', inputEl(draft.meta.icon, (v) => (draft.meta.icon = v || null))),
    labeled('Note', inputEl(draft.meta.note, (v) => (draft.meta.note = v || null))),
    labeled('Display template', inputEl(draft.meta.display_template, (v) => (draft.meta.display_template = v || null), { placeholder: '{{title}}' })),
    labeled('Default sort field', inputEl(draft.meta.sort_field, (v) => (draft.meta.sort_field = v || null))),
    labeled('Group', inputEl(draft.meta.group, (v) => (draft.meta.group = v || null))),
    labeled(
      'Natural key (comma separated)',
      inputEl((draft.sync.natural_key || []).join(', '), (v) => (draft.sync.natural_key = v.split(',').map((s) => s.trim()).filter(Boolean))),
      'Fields identifying the same item in different environments (e.g. slug). Used by data promotion.',
    ),
    checkbox('Singleton', draft.meta.singleton, (v) => (draft.meta.singleton = v)),
    checkbox('Hidden in navigation', draft.meta.hidden, (v) => (draft.meta.hidden = v)),
    checkbox('Store revisions', draft.meta.revisions !== false, (v) => (draft.meta.revisions = v)),
    checkbox('Never export in data promotion', draft.sync.exclude, (v) => (draft.sync.exclude = v)),
    checkbox('System fields (date_created, user_created, ...)', draft.system_fields !== false, (v) => (draft.system_fields = v)),
    h(
      'div',
      { class: 'row-actions' },
      button(
        'Save settings',
        () =>
          applyChange(`Update ${c.name}`, (q) => api.patch(`/collections/${encodeURIComponent(c.name)}`, draft, q)).then((res) => {
            if (res && draft.name !== c.name) go(`#/model/${encodeURIComponent(draft.name)}`);
          }).catch(fail),
        'primary',
      ),
      button(
        'Delete collection',
        async () => {
          if (!(await confirmDialog(`Delete "${c.name}" and all its data?`, { label: 'Continue', danger: true }))) return;
          const res = await applyChange(`Delete ${c.name}`, (q) => api.del(`/collections/${encodeURIComponent(c.name)}`, q));
          if (res !== null) go('#/model');
        },
        'danger',
      ),
    ),
  );

  const fieldRows = (c.fields || []).map((f) =>
    h(
      'tr',
      { class: 'clickable', onClick: () => fieldEditor(c, f) },
      h('td', {}, f.name),
      h('td', {}, f.type),
      h('td', {}, S.relatedCollection(f) || ''),
      h('td', {}, [f.required ? 'required' : null, f.unique ? 'unique' : null, f.indexed ? 'indexed' : null, f.meta?.hidden ? 'hidden' : null].filter(Boolean).join(', ')),
    ),
  );
  const fields = h(
    'div',
    { class: 'card' },
    h('div', { class: 'page-head' }, h('h3', {}, 'Fields'), h('span', { class: 'spacer' }), button('+ New field', () => fieldEditor(c, null), 'primary sm')),
    h('p', { class: 'muted' }, `id${c.system_fields !== false ? ', date_created, user_created, date_updated, user_updated' : ''} are implicit.`),
    h('table', {}, h('thead', {}, h('tr', {}, ['Name', 'Type', 'Related', 'Flags'].map((t) => h('th', {}, t)))), h('tbody', {}, fieldRows)),
  );

  add(root, h('div', { class: 'page-head' }, h('a', { href: '#/model', class: 'back' }, '‹ Data model'), h('h1', {}, c.name)), h('div', { class: 'two-col' }, meta, fields));
}

// ---------------------------------------------------------------------------
// field editor
// ---------------------------------------------------------------------------

const INTERFACES = ['input', 'input-multiline', 'input-rich-text-html', 'select-dropdown', 'toggle', 'datetime', 'tags', 'file', 'file-image', 'list-m2o', 'list-o2m', 'list-m2m', 'list-m2a', 'translations'];
const ON_DELETE = ['set_null', 'cascade', 'restrict', 'no_action'];
const SYNC = ['default', 'deep', 'reference', 'skip'];

function collectionOptions(extra = []) {
  return ['', ...S.userCollections().map((c) => c.name), ...extra].map((v) => ({ value: v, label: v || '— choose —' }));
}

function fieldNameOptions(collection, pred) {
  const col = S.getCollection(collection);
  return [{ value: '', label: '— choose —' }, ...(col?.fields || []).filter(pred).map((f) => ({ value: f.name, label: f.name }))];
}

function fieldEditor(collection, existing) {
  const f = existing ? clone(existing) : { name: '', type: 'string' };
  f.relation ||= {};
  f.meta ||= {};
  const originalName = existing?.name;

  const common = h('div');
  const relationBox = h('div');
  const extraBox = h('div');

  const isRel = () => ['m2o', 'file', 'o2m', 'm2m', 'm2a', 'm2a_item'].includes(f.type);

  function drawCommon() {
    const alias = ['o2m', 'm2m', 'm2a'].includes(f.type);
    replace(common, 
      alias
        ? h('p', { class: 'muted' }, 'Alias field: it has no column of its own.')
        : h(
            'div',
            {},
            checkbox('Required (NOT NULL)', f.required, (v) => (f.required = v)),
            checkbox('Unique', f.unique, (v) => (f.unique = v)),
            checkbox('Indexed', f.indexed, (v) => (f.indexed = v)),
            labeled(
              'Default value',
              inputEl(f.default === undefined || f.default === null ? '' : typeof f.default === 'string' ? f.default : JSON.stringify(f.default), (v) => {
                if (v === '') delete f.default;
                else {
                  try {
                    f.default = ['integer', 'bigint', 'float', 'boolean', 'json'].includes(f.type) ? JSON.parse(v) : v;
                  } catch {
                    f.default = v;
                  }
                }
              }),
              'Use $NOW for the current time on datetime fields.',
            ),
          ),
      f.type === 'string' ? labeled('Max length', inputEl(f.max_length ?? '', (v) => (f.max_length = v ? Number(v) : undefined), { type: 'number' })) : null,
      f.type === 'decimal'
        ? h(
            'div',
            { class: 'two-col' },
            labeled('Precision', inputEl(f.precision ?? '', (v) => (f.precision = v ? Number(v) : undefined), { type: 'number' })),
            labeled('Scale', inputEl(f.scale ?? '', (v) => (f.scale = v ? Number(v) : undefined), { type: 'number' })),
          )
        : null,
    );
  }

  function drawRelation() {
    const r = f.relation;
    const parts = [];
    if (f.type === 'm2o') {
      parts.push(
        labeled('Related collection', selectEl(collectionOptions(['ferro_users', 'ferro_files']), r.related || '', (v) => (r.related = v))),
        labeled('On delete of the related item', selectEl(ON_DELETE, r.on_delete || 'set_null', (v) => (r.on_delete = v))),
        labeled('Data promotion', selectEl(SYNC, r.sync || 'default', (v) => (r.sync = v)), 'default = export the target too; reference/skip = it must already exist in the target environment.'),
      );
    } else if (f.type === 'file') {
      r.related = 'ferro_files';
      parts.push(labeled('On delete of the file', selectEl(ON_DELETE, r.on_delete || 'set_null', (v) => (r.on_delete = v))));
    } else if (['o2m', 'm2m', 'm2a'].includes(f.type)) {
      const child = r.related;
      parts.push(
        labeled(
          f.type === 'o2m' ? 'Child collection' : 'Junction collection',
          selectEl(collectionOptions(), child || '', (v) => {
            r.related = v;
            drawRelation();
          }),
          f.type === 'o2m' ? null : 'A normal collection declared in the model (create it first).',
        ),
        labeled(
          'Field pointing back here',
          selectEl(fieldNameOptions(child, (x) => x.type === 'm2o' && x.relation?.related === collection.name), r.foreign_field || '', (v) => (r.foreign_field = v)),
          'An m2o field of the related collection whose target is this collection.',
        ),
        f.type === 'm2m'
          ? labeled('Field towards the other side', selectEl(fieldNameOptions(child, (x) => x.type === 'm2o' || x.type === 'file'), r.junction_field || '', (v) => (r.junction_field = v)))
          : null,
        labeled('Sort field (integer)', selectEl(fieldNameOptions(child, (x) => ['integer', 'bigint'].includes(x.type)), r.sort_field || '', (v) => (r.sort_field = v || undefined))),
        labeled('Data promotion', selectEl(SYNC, r.sync || 'default', (v) => (r.sync = v))),
      );
    } else if (f.type === 'm2a_item') {
      const allowed = new Set(r.allowed || []);
      parts.push(
        labeled(
          'Field holding the collection name',
          selectEl(fieldNameOptions(collection.name, (x) => x.type === 'string'), r.collection_field || '', (v) => (r.collection_field = v)),
        ),
        h(
          'div',
          { class: 'field' },
          h('label', {}, 'Allowed collections'),
          S.userCollections().map((c) =>
            checkbox(c.name, allowed.has(c.name), (on) => {
              if (on) allowed.add(c.name);
              else allowed.delete(c.name);
              r.allowed = [...allowed];
            }),
          ),
        ),
      );
    }
    replace(relationBox, ...parts);
  }

  function redraw() {
    drawCommon();
    drawRelation();
    relationBox.hidden = !isRel();
  }

  const typeSelect = selectEl(S.FIELD_TYPES, f.type, (v) => {
    f.type = v;
    redraw();
  });
  if (existing) typeSelect.disabled = false;

  extraBox.append(
    h(
      'details',
      {},
      h('summary', {}, 'Admin appearance & validation'),
      labeled('Interface', selectEl(['', ...INTERFACES], f.meta.interface || '', (v) => (f.meta.interface = v || undefined))),
      labeled('Interface options (JSON)', h('textarea', { rows: 3, class: 'mono', onInput: (e) => (f.meta._options = e.target.value) }, f.meta.options ? JSON.stringify(f.meta.options, null, 2) : '')),
      labeled('Note', inputEl(f.meta.note, (v) => (f.meta.note = v || undefined))),
      labeled('Width', selectEl(['full', 'half'], f.meta.width || 'full', (v) => (f.meta.width = v === 'half' ? 'half' : undefined))),
      checkbox('Hidden in forms', f.meta.hidden, (v) => (f.meta.hidden = v || undefined)),
      checkbox('Read only', f.meta.readonly, (v) => (f.meta.readonly = v || undefined)),
      labeled('Validation (JSON filter)', h('textarea', { rows: 2, class: 'mono', onInput: (e) => (f._validation = e.target.value) }, f.validation ? JSON.stringify(f.validation) : ''), 'e.g. {"_nempty": true}'),
    ),
  );

  redraw();

  function build() {
    const out = clone(f);
    if (!out.name) throw new Error('Name is required');
    if (out.meta._options !== undefined) {
      const parsed = parseJson(out.meta._options, 'Interface options');
      if (parsed) out.meta.options = parsed;
      else delete out.meta.options;
      delete out.meta._options;
    }
    if (out._validation !== undefined) {
      const parsed = parseJson(out._validation, 'Validation');
      if (parsed) out.validation = parsed;
      else delete out.validation;
      delete out._validation;
    }
    if (!isRel() || Object.keys(out.relation).length === 0) delete out.relation;
    if (['o2m', 'm2m', 'm2a'].includes(out.type)) {
      delete out.required;
      delete out.unique;
      delete out.indexed;
      delete out.default;
    }
    return out;
  }

  const dlg = modal({
    title: existing ? `Field ${existing.name}` : 'New field',
    wide: true,
    body: h('div', {}, labeled('Name', inputEl(f.name, (v) => (f.name = v.trim()), { placeholder: 'field_name' }), existing ? 'Renaming keeps the data.' : null), labeled('Type', typeSelect), common, relationBox, extraBox),
    buttons: [
      existing
        ? {
            label: 'Delete field',
            class: 'danger',
            close: false,
            onClick: async () => {
              const res = await applyChange(`Delete ${collection.name}.${existing.name}`, (q) => api.del(`/fields/${encodeURIComponent(collection.name)}/${encodeURIComponent(existing.name)}`, q));
              if (res !== null) {
                dlg.close();
                location.reload();
              }
            },
          }
        : { label: 'Cancel' },
      {
        label: 'Save',
        class: 'primary',
        close: false,
        onClick: async () => {
          const body = build();
          const path = existing ? `/fields/${encodeURIComponent(collection.name)}/${encodeURIComponent(originalName)}` : `/fields/${encodeURIComponent(collection.name)}`;
          const res = await applyChange(`${existing ? 'Update' : 'Add'} ${collection.name}.${body.name}`, (q) => (existing ? api.patch(path, body, q) : api.post(path, body, q)));
          if (res) {
            dlg.close();
            location.reload();
          }
        },
      },
    ],
  });
}
