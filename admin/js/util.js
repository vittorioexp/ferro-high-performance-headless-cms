// Pure helpers (no DOM, no network): unit-tested with `node --test`.

export const shortId = (id) => (id ? String(id).slice(0, 8) : '');

export function truncate(value, max = 80) {
  const s = String(value ?? '');
  return s.length > max ? s.slice(0, max - 1) + '…' : s;
}

const pad = (n) => String(n).padStart(2, '0');

/** ISO timestamp -> value for <input type="datetime-local"> (local time). */
export function isoToLocalInput(iso) {
  if (!iso) return '';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return '';
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** Value of <input type="datetime-local"> -> ISO timestamp (UTC). */
export function localInputToIso(value) {
  if (!value) return null;
  const d = new Date(value);
  return Number.isNaN(d.getTime()) ? null : d.toISOString();
}

export function parseJson(text, what = 'JSON') {
  const t = (text ?? '').trim();
  if (!t) return null;
  try {
    return JSON.parse(t);
  } catch (e) {
    throw new Error(`${what}: ${e.message}`);
  }
}

export function newId() {
  return globalThis.crypto.randomUUID();
}

export function titleCase(name) {
  return String(name ?? '')
    .replace(/^ferro_/, '')
    .replace(/_/g, ' ')
    .replace(/\b\w/g, (c) => c.toUpperCase());
}

/** Match `/content/pages/123` against patterns such as `/content/:collection/:id`. */
export function matchRoute(path, patterns) {
  const segs = path.split('/').filter(Boolean);
  for (const pattern of patterns) {
    const ps = pattern.split('/').filter(Boolean);
    if (ps.length !== segs.length) continue;
    const params = {};
    let ok = true;
    for (let i = 0; i < ps.length; i++) {
      if (ps[i].startsWith(':')) params[ps[i].slice(1)] = decodeURIComponent(segs[i]);
      else if (ps[i] !== segs[i]) {
        ok = false;
        break;
      }
    }
    if (ok) return { pattern, params };
  }
  return null;
}

export const FILTER_OPS = [
  ['_eq', 'equals'],
  ['_neq', 'not equals'],
  ['_icontains', 'contains'],
  ['_istarts_with', 'starts with'],
  ['_gt', '>'],
  ['_gte', '≥'],
  ['_lt', '<'],
  ['_lte', '≤'],
  ['_in', 'in (a,b,c)'],
  ['_null', 'is empty'],
  ['_nnull', 'is not empty'],
];

/** [{field, op, value}] -> Ferro filter object (or null). */
export function buildFilter(conditions) {
  const parts = [];
  for (const c of conditions || []) {
    if (!c.field || !c.op) continue;
    let operand;
    if (c.op === '_null' || c.op === '_nnull') operand = true;
    else if (c.op === '_in') operand = String(c.value ?? '').split(',').map((s) => s.trim()).filter(Boolean);
    else if (c.value === '' || c.value === undefined) continue;
    else operand = c.value;
    parts.push({ [c.field]: { [c.op]: operand } });
  }
  if (parts.length === 0) return null;
  return parts.length === 1 ? parts[0] : { _and: parts };
}

export function formatBytes(n) {
  const v = Number(n);
  if (!Number.isFinite(v)) return '';
  if (v < 1024) return `${v} B`;
  if (v < 1024 * 1024) return `${(v / 1024).toFixed(1)} KB`;
  return `${(v / 1024 / 1024).toFixed(1)} MB`;
}
