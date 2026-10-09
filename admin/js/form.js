// Item form generated from the data model.
//
//   const form = renderForm('pages', item);     // item = null for a new one
//   container.append(form.el);
//   const payload = form.collect();             // throws FormError when invalid
//
// Relations are edited inline and written back with the nested-write format of
// the API: arrays replace the whole relation (items without `id` are created,
// items with `id` are updated, omitted ones are removed or detached).

import { api, thumbUrl } from './api.js';
import { button, fail, h, modal, replace } from './dom.js';
import * as S from './schema.js';
import { registry } from './registry.js';
import { isoToLocalInput, localInputToIso, parseJson, shortId, truncate } from './util.js';

export class FormError extends Error {}

const MAX_DEPTH = 3;
const optionCache = new Map();

export function clearOptionCache() {
  optionCache.clear();
}

function loadOptions(collection) {
  if (!optionCache.has(collection)) {
    const c = S.getCollection(collection);
    const p = api
      .get(`/items/${collection}`, { fields: S.listFieldsParam(c).join(','), limit: 500 })
      .then((r) => (Array.isArray(r.data) ? r.data : r.data ? [r.data] : []))
      .catch((e) => {
        optionCache.delete(collection);
        throw e;
      });
    optionCache.set(collection, p);
  }
  return optionCache.get(collection);
}

// ---------------------------------------------------------------------------
// form
// ---------------------------------------------------------------------------

export function renderForm(collectionName, item, opts = {}) {
  const c = S.getCollection(collectionName);
  if (!c) throw new Error(`Unknown collection ${collectionName}`);
  const depth = opts.depth || 0;
  const hide = new Set(opts.hide || []);
  const isNew = !item || !item.id;
  const widgets = [];
  const el = h('div', { class: 'form' });

  for (const field of c.fields || []) {
    if (hide.has(field.name) || field.meta?.hidden) continue;
    const present = isNew || (item && field.name in item);
    const value = item ? item[field.name] : undefined;
    const w = fieldWidget(c, field, value, { isNew, depth, present });
    widgets.push({ field, get: w.get, present });
    el.append(
      h(
        'div',
        { class: `field w-${field.meta?.width === 'half' ? 'half' : 'full'}` },
        h('label', {}, S.fieldLabel(field), field.required ? h('span', { class: 'req' }, ' *') : null),
        w.el,
        field.meta?.note ? h('div', { class: 'note' }, field.meta.note) : null,
      ),
    );
  }

  function collect() {
    const out = {};
    const missing = [];
    for (const w of widgets) {
      if (w.field.meta?.readonly || !w.present) continue;
      const v = w.get();
      if (v === undefined) continue;
      if (w.field.required && (v === null || v === '')) missing.push(S.fieldLabel(w.field));
      out[w.field.name] = v;
    }
    if (missing.length) throw new FormError(`Required: ${missing.join(', ')}`);
    return out;
  }

  return { el, collect };
}

// ---------------------------------------------------------------------------
// widgets
// ---------------------------------------------------------------------------

function fieldWidget(c, field, value, ctx) {
  const readonly = !!field.meta?.readonly || !ctx.present;
  const initial = ctx.isNew && value === undefined && field.default !== '$NOW' ? field.default : value;
  // interfaces contributed by extensions take precedence over the built-in widgets
  const custom = field.meta?.interface ? registry.interfaces[field.meta.interface] : null;
  if (custom) {
    try {
      const w = custom(field, initial, { collection: c.name, isNew: ctx.isNew, readonly, depth: ctx.depth });
      if (w && w.el && typeof w.get === 'function') return w;
      console.warn(`Interface "${field.meta.interface}" must return { el, get }`);
    } catch (err) {
      console.error(`Interface "${field.meta.interface}" failed:`, err);
    }
  }
  switch (field.type) {
    case 'string': {
      const choices = field.meta?.options?.choices;
      if (Array.isArray(choices) && choices.length) return choiceSelect(choices, initial, field.required, readonly);
      return textInput(field, initial, 'text', readonly);
    }
    case 'text':
      return textArea(initial, 6, readonly);
    case 'integer':
    case 'bigint':
      return numberInput(initial, '1', false, readonly);
    case 'float':
      return numberInput(initial, 'any', false, readonly);
    case 'decimal':
      return numberInput(initial, 'any', true, readonly);
    case 'boolean': {
      const input = h('input', { type: 'checkbox', checked: initial === true || initial === 'true', disabled: readonly });
      return { el: input, get: () => !!input.checked };
    }
    case 'json': {
      const text = initial === undefined || initial === null ? '' : JSON.stringify(initial, null, 2);
      const area = h('textarea', { rows: 6, class: 'mono', disabled: readonly }, text);
      area.value = text;
      return { el: area, get: () => parseJson(area.value, S.fieldLabel(field)) };
    }
    case 'date': {
      const input = h('input', { type: 'date', value: initial ? String(initial).slice(0, 10) : '', disabled: readonly });
      return { el: input, get: () => input.value || null };
    }
    case 'datetime': {
      const input = h('input', { type: 'datetime-local', value: isoToLocalInput(initial), disabled: readonly });
      return { el: input, get: () => localInputToIso(input.value) };
    }
    case 'time': {
      const input = h('input', { type: 'time', step: '1', value: initial ?? '', disabled: readonly });
      return { el: input, get: () => input.value || null };
    }
    case 'm2o':
      return relationSelect(S.relatedCollection(field), objectId(initial), field.required, readonly);
    case 'file':
      return filePicker(initial, readonly);
    case 'o2m':
      return ctx.depth >= MAX_DEPTH ? tooDeep() : childrenEditor(field, initial, ctx.depth, readonly);
    case 'm2m':
      return ctx.depth >= MAX_DEPTH ? tooDeep() : manyToMany(field, initial, readonly);
    case 'm2a':
      return ctx.depth >= MAX_DEPTH ? tooDeep() : blocksEditor(field, initial, ctx.depth, readonly);
    default:
      // uuid, m2a_item (raw), anything unknown
      return textInput(field, initial, 'text', readonly);
  }
}

const objectId = (v) => (v && typeof v === 'object' ? v.id : v);

function tooDeep() {
  return { el: h('div', { class: 'muted' }, 'Nested too deeply: edit this relation from its own collection.'), get: () => undefined };
}

function textInput(field, value, type, readonly) {
  const input = h('input', { type, value: value ?? '', maxlength: field.max_length || null, disabled: readonly });
  return { el: input, get: () => (input.value === '' ? null : input.value) };
}

function textArea(value, rows, readonly) {
  const area = h('textarea', { rows, disabled: readonly }, value ?? '');
  area.value = value ?? '';
  return { el: area, get: () => (area.value === '' ? null : area.value) };
}

function numberInput(value, step, asString, readonly) {
  const input = h('input', { type: 'number', step, value: value ?? '', disabled: readonly });
  return {
    el: input,
    get: () => {
      if (input.value === '') return null;
      return asString ? String(input.value) : Number(input.value);
    },
  };
}

function choiceSelect(choices, value, required, readonly) {
  const opts = choices.map((ch) => (typeof ch === 'string' ? { value: ch, text: ch } : { value: ch.value, text: ch.text ?? ch.value }));
  const select = h(
    'select',
    { disabled: readonly, value: value ?? '' },
    required ? null : h('option', { value: '' }, '—'),
    opts.map((o) => h('option', { value: o.value }, o.text)),
  );
  select.value = value ?? (required ? opts[0]?.value : '') ?? '';
  return { el: select, get: () => (select.value === '' ? null : select.value) };
}

/** <select> filled with the items of another collection. */
function relationSelect(related, value, required, readonly) {
  const empty = () => h('option', { value: '' }, required ? '— select —' : '— none —');
  const select = h('select', { disabled: readonly }, empty(), value ? h('option', { value }, `${shortId(value)}…`) : null);
  select.value = value || '';
  if (related) {
    loadOptions(related)
      .then((items) => {
        const keep = select.value || value || '';
        const known = items.some((i) => i.id === keep);
        replace(select, 
          empty(),
          items.map((it) => h('option', { value: it.id }, S.labelOf(related, it) || shortId(it.id))),
          keep && !known ? h('option', { value: keep }, `${shortId(keep)}…`) : null,
        );
        select.value = keep;
      })
      .catch((e) => console.warn(`Cannot load ${related}:`, e.message));
  }
  return { el: select, get: () => select.value || null };
}

// ---- files -------------------------------------------------------------------

function fileThumb(id, meta) {
  const name = meta?.filename_download || shortId(id);
  if (meta?.type && !String(meta.type).startsWith('image/')) return h('span', { class: 'file-chip' }, name);
  return h('img', {
    src: thumbUrl(id, meta?.type, 160),
    alt: name,
    class: 'thumb',
    onError: (e) => e.target.replaceWith(h('span', { class: 'file-chip' }, name)),
  });
}

export function pickFile(onPick) {
  const grid = h('div', { class: 'file-grid' });
  const search = h('input', { type: 'search', placeholder: 'Search files…' });
  const load = async () => {
    const r = await api.get('/files', { fields: 'id,title,filename_download,type', limit: 60, sort: '-uploaded_on', search: search.value });
    replace(grid, 
      ...r.data.map((f) =>
        h(
          'button',
          {
            type: 'button',
            class: 'file-card',
            onClick: () => {
              onPick(f);
              dlg.close();
            },
          },
          fileThumb(f.id, f),
          h('span', { class: 'file-name' }, truncate(f.title || f.filename_download, 28)),
        ),
      ),
    );
  };
  search.addEventListener('input', () => load().catch(fail));
  const dlg = modal({ title: 'Choose a file', wide: true, body: h('div', {}, search, grid), buttons: [{ label: 'Close' }] });
  load().catch(fail);
}

function filePicker(value, readonly) {
  let current = objectId(value) || null;
  let meta = value && typeof value === 'object' ? value : null;
  const preview = h('div', { class: 'file-preview' });
  const draw = () => replace(preview, current ? fileThumb(current, meta) : h('span', { class: 'muted' }, 'No file'));
  draw();
  const picker = h('input', {
    type: 'file',
    style: { display: 'none' },
    onChange: async (e) => {
      const file = e.target.files[0];
      if (!file) return;
      try {
        const fd = new FormData();
        fd.append('file', file);
        const r = await api.upload('/files', fd);
        current = r.data.id;
        meta = r.data;
        draw();
      } catch (err) {
        fail(err);
      }
    },
  });
  const controls = readonly
    ? null
    : h(
        'div',
        { class: 'row-actions' },
        button('Upload', () => picker.click(), 'sm'),
        button(
          'Choose',
          () =>
            pickFile((f) => {
              current = f.id;
              meta = f;
              draw();
            }),
          'sm',
        ),
        button(
          'Clear',
          () => {
            current = null;
            meta = null;
            draw();
          },
          'sm',
        ),
        picker,
      );
  return { el: h('div', { class: 'file-field' }, preview, controls), get: () => current };
}

// ---- one-to-many ----------------------------------------------------------------

function rowCard(title, body, actions) {
  return h('div', { class: 'row-card' }, h('div', { class: 'row-head' }, h('strong', {}, title), h('span', { class: 'spacer' }), actions), body);
}

function moveButtons(rows, i, redraw) {
  const move = (d) => {
    const j = i + d;
    if (j < 0 || j >= rows.length) return;
    [rows[i], rows[j]] = [rows[j], rows[i]];
    redraw();
  };
  return [button('↑', () => move(-1), 'icon'), button('↓', () => move(1), 'icon')];
}

function listEditor({ rows, makeCard, addControls, readonly }) {
  const list = h('div', { class: 'rows' });
  const draw = () => replace(list, ...rows.map((r, i) => makeCard(r, i, draw)));
  draw();
  return { el: h('div', { class: 'relation-editor' }, list, readonly ? null : addControls(draw)), redraw: draw };
}

function childrenEditor(field, value, depth, readonly) {
  const rel = field.relation;
  const child = rel.related;
  const ff = rel.foreign_field;
  const sortField = rel.sort_field;
  const hide = [ff, sortField].filter(Boolean);
  const makeRow = (item) => {
    if (typeof item === 'string') return { opaque: true, id: item };
    return { id: item?.id, item, form: renderForm(child, item, { depth: depth + 1, hide }) };
  };
  const rows = (Array.isArray(value) ? value : []).map(makeRow);
  const editor = listEditor({
    rows,
    readonly,
    makeCard: (r, i, redraw) =>
      rowCard(
        r.opaque ? shortId(r.id) : r.item ? S.labelOf(child, r.item) : 'New',
        r.opaque ? h('div', { class: 'muted' }, 'Not readable with your permissions') : r.form.el,
        readonly
          ? null
          : [
              sortField ? moveButtons(rows, i, redraw) : null,
              button(
                '✕',
                () => {
                  rows.splice(i, 1);
                  redraw();
                },
                'icon danger',
              ),
            ],
      ),
    addControls: (redraw) =>
      button(
        `+ Add ${S.collectionLabel(child)}`,
        () => {
          rows.push(makeRow(null));
          redraw();
        },
        'sm',
      ),
  });
  return {
    el: editor.el,
    get: () =>
      readonly
        ? undefined
        : rows.map((r, i) => {
            if (r.opaque) return r.id;
            const o = r.form.collect();
            if (r.id) o.id = r.id;
            if (sortField) o[sortField] = i + 1;
            return o;
          }),
  };
}

// ---- many-to-many ---------------------------------------------------------------

function manyToMany(field, value, readonly) {
  const rel = field.relation;
  const sortField = rel.sort_field;
  const far = S.junctionTarget(field);
  const makeRow = (item) => {
    const farValue = item && typeof item === 'object' ? objectId(item[rel.junction_field]) : null;
    return { id: item && typeof item === 'object' ? item.id : undefined, select: relationSelect(far, farValue, true, readonly) };
  };
  const rows = (Array.isArray(value) ? value : []).map(makeRow);
  const editor = listEditor({
    rows,
    readonly,
    makeCard: (r, i, redraw) =>
      h(
        'div',
        { class: 'row-inline' },
        r.select.el,
        readonly
          ? null
          : [
              sortField ? moveButtons(rows, i, redraw) : null,
              button(
                '✕',
                () => {
                  rows.splice(i, 1);
                  redraw();
                },
                'icon danger',
              ),
            ],
      ),
    addControls: (redraw) =>
      button(
        `+ Add ${S.collectionLabel(far || 'item')}`,
        () => {
          rows.push(makeRow(null));
          redraw();
        },
        'sm',
      ),
  });
  return {
    el: editor.el,
    get: () => {
      if (readonly) return undefined;
      return rows
        .map((r, i) => {
          const id = r.select.get();
          if (!id) return null;
          const o = { [rel.junction_field]: id };
          if (r.id) o.id = r.id;
          if (sortField) o[sortField] = i + 1;
          return o;
        })
        .filter(Boolean);
    },
  };
}

// ---- many-to-any (page-builder blocks) ----------------------------------------

function blocksEditor(field, value, depth, readonly) {
  const rel = field.relation;
  const junction = rel.related;
  const itemField = S.m2aItemField(junction);
  if (!itemField) return { el: h('div', { class: 'muted' }, 'Junction without m2a_item field'), get: () => undefined };
  const cf = itemField.relation.collection_field;
  const allowed = itemField.relation.allowed || [];
  const sortField = rel.sort_field;

  const makeRow = (junctionRow, coll) => {
    if (!junctionRow) return { coll, form: renderForm(coll, null, { depth: depth + 1 }) };
    const target = junctionRow[itemField.name];
    const collection = junctionRow[cf];
    if (target && typeof target === 'object') {
      return { junctionId: junctionRow.id, coll: collection, itemId: target.id, item: target, form: renderForm(collection, target, { depth: depth + 1 }) };
    }
    // not expanded (no permission, or target removed): keep untouched
    return { junctionId: junctionRow.id, coll: collection, itemId: target, opaque: true };
  };
  const rows = (Array.isArray(value) ? value : []).map((j) => makeRow(j));

  let chosen = allowed[0];
  const editor = listEditor({
    rows,
    readonly,
    makeCard: (r, i, redraw) =>
      rowCard(
        `${S.collectionLabel(r.coll || '?')}${r.item ? ` · ${truncate(S.labelOf(r.coll, r.item), 40)}` : ''}`,
        r.opaque ? h('div', { class: 'muted' }, 'Content not available') : r.form.el,
        readonly
          ? null
          : [
              moveButtons(rows, i, redraw),
              button(
                '✕',
                () => {
                  rows.splice(i, 1);
                  redraw();
                },
                'icon danger',
              ),
            ],
      ),
    addControls: (redraw) =>
      h(
        'div',
        { class: 'row-actions' },
        h(
          'select',
          { onChange: (e) => (chosen = e.target.value), value: chosen },
          allowed.map((a) => h('option', { value: a }, S.collectionLabel(a))),
        ),
        button(
          '+ Add block',
          () => {
            if (!chosen) return;
            rows.push(makeRow(null, chosen));
            redraw();
          },
          'sm',
        ),
      ),
  });
  return {
    el: editor.el,
    get: () => {
      if (readonly) return undefined;
      return rows.map((r, i) => {
        const o = {};
        if (r.junctionId) o.id = r.junctionId;
        o[cf] = r.coll;
        if (r.opaque) o[itemField.name] = r.itemId;
        else {
          const payload = r.form.collect();
          if (r.itemId) payload.id = r.itemId;
          o[itemField.name] = payload;
        }
        if (sortField) o[sortField] = i + 1;
        return o;
      });
    },
  };
}
