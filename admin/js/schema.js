// Client-side view of the data model. Pure functions over the collections
// loaded from GET /collections (same JSON as the snapshot files).

import { shortId, titleCase } from './util.js';

let collections = new Map();

const f = (name, type, extra = {}) => ({ name, type, ...extra });
const m2o = (name, related) => f(name, 'm2o', { relation: { related } });

// System collections exposed by the items API (see ferro-core/src/system.rs).
const SYSTEM = {
  ferro_users: {
    name: 'ferro_users',
    system_fields: false,
    meta: {},
    fields: [f('email', 'string'), f('first_name', 'string'), f('last_name', 'string'), f('role', 'uuid'), f('status', 'string'), f('last_access', 'datetime')],
  },
  ferro_folders: { name: 'ferro_folders', system_fields: false, meta: {}, fields: [f('name', 'string'), m2o('parent', 'ferro_folders')] },
  ferro_files: {
    name: 'ferro_files',
    system_fields: false,
    meta: {},
    fields: [
      f('filename_download', 'string'),
      f('title', 'string'),
      f('description', 'text'),
      f('type', 'string'),
      m2o('folder', 'ferro_folders'),
      f('filesize', 'bigint'),
      f('width', 'integer'),
      f('height', 'integer'),
      f('uploaded_on', 'datetime'),
      f('modified_on', 'datetime'),
    ],
  },
};

export const SYSTEM_FIELD_NAMES = ['date_created', 'user_created', 'date_updated', 'user_updated'];
export const SYSTEM_COLLECTIONS = Object.keys(SYSTEM);
export const FIELD_TYPES = ['string', 'text', 'integer', 'bigint', 'float', 'decimal', 'boolean', 'uuid', 'json', 'date', 'datetime', 'time', 'm2o', 'file', 'o2m', 'm2m', 'm2a', 'm2a_item'];

export function setCollections(list) {
  collections = new Map((list || []).map((c) => [c.name, c]));
}

export function getCollection(name) {
  return collections.get(name) || SYSTEM[name] || null;
}

export function userCollections() {
  return [...collections.values()].sort((a, b) => a.name.localeCompare(b.name));
}

export const isAlias = (field) => ['o2m', 'm2m', 'm2a'].includes(field.type);
export const isRelational = (field) => isAlias(field) || ['m2o', 'file', 'm2a_item'].includes(field.type);

export function collectionLabel(c) {
  return titleCase(typeof c === 'string' ? c : c.name);
}

export function fieldLabel(field) {
  return titleCase(field.name);
}

export function relatedCollection(field) {
  if (field.type === 'file') return 'ferro_files';
  return field.relation?.related || null;
}

export function m2aItemField(junctionName) {
  return getCollection(junctionName)?.fields?.find((x) => x.type === 'm2a_item') || null;
}

/** Declared fields that have a column. */
export function scalarFields(c) {
  return (c?.fields || []).filter((x) => !isAlias(x));
}

/** Fields shown by item listings and relation pickers. */
export function listFieldsParam(c, extra = []) {
  const names = ['id', ...scalarFields(c).filter((x) => x.type !== 'json').map((x) => x.name), ...extra];
  return [...new Set(names)];
}

export function labelOf(collectionName, item) {
  if (item === null || item === undefined) return '';
  if (typeof item !== 'object') return shortId(item);
  const c = getCollection(collectionName);
  const tpl = c?.meta?.display_template;
  if (tpl) {
    const s = tpl
      .replace(/\{\{\s*([\w.]+)\s*\}\}/g, (_, path) => {
        const v = path.split('.').reduce((a, k) => (a ? a[k] : undefined), item);
        return v === undefined || v === null ? '' : String(v);
      })
      .trim();
    if (s) return s;
  }
  for (const k of ['title', 'name', 'headline', 'slug', 'code', 'label', 'email', 'filename_download']) {
    if (item[k]) return String(item[k]);
  }
  return shortId(item.id);
}

/**
 * Dotted paths needed to load an item together with everything its form edits:
 * o2m / m2m children, m2a junction rows and the block items, recursively.
 * M2A targets use lenient reads on the server, so paths valid for only some
 * of the allowed collections are safe.
 */
export function relationPaths(collectionName, prefix = '', depth = 3) {
  const c = getCollection(collectionName);
  if (!c || depth <= 0) return [];
  const out = [];
  for (const field of c.fields || []) {
    const rel = field.relation || {};
    const p = prefix + field.name;
    if (field.type === 'file') {
      out.push(`${p}.*`); // file name and type for the preview
    } else if (field.type === 'o2m' || field.type === 'm2m') {
      out.push(`${p}.*`);
      if (field.type === 'o2m') out.push(...relationPaths(rel.related, `${p}.`, depth - 1));
    } else if (field.type === 'm2a') {
      out.push(`${p}.*`);
      const item = m2aItemField(rel.related);
      if (item) {
        out.push(`${p}.${item.name}.*`);
        for (const allowed of item.relation?.allowed || []) {
          out.push(...relationPaths(allowed, `${p}.${item.name}.`, depth - 1));
        }
      }
    }
  }
  return out;
}

export function editFieldsParam(collectionName) {
  return ['*', ...new Set(relationPaths(collectionName))].join(',');
}

/** m2m: collection on the far side of the junction. */
export function junctionTarget(field) {
  const junction = getCollection(field.relation?.related);
  const far = junction?.fields?.find((x) => x.name === field.relation?.junction_field);
  return far ? relatedCollection(far) : null;
}
