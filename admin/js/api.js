// HTTP client for the Ferro API: bearer auth, transparent refresh on 401.

export class ApiError extends Error {
  constructor(message, status, code) {
    super(message);
    this.status = status;
    this.code = code;
  }
}

const storage = typeof localStorage !== 'undefined' ? localStorage : null;

const state = {
  base: '',
  access: null,
  refresh: storage ? storage.getItem('ferro_refresh') : null,
  onAuthLost: null,
};

let refreshing = null;

export function configure({ base = '', onAuthLost = null } = {}) {
  state.base = base.replace(/\/$/, '');
  state.onAuthLost = onAuthLost;
}

export const baseUrl = () => state.base;
export const currentToken = () => state.access;
export const hasSession = () => !!state.refresh || !!state.access;

function qs(query) {
  const p = new URLSearchParams();
  for (const [k, v] of Object.entries(query || {})) {
    if (v === undefined || v === null || v === '') continue;
    p.set(k, typeof v === 'object' ? JSON.stringify(v) : String(v));
  }
  const s = p.toString();
  return s ? `?${s}` : '';
}

function setTokens(tokens) {
  state.access = tokens ? tokens.access_token : null;
  state.refresh = tokens ? tokens.refresh_token : null;
  if (storage) {
    if (tokens) storage.setItem('ferro_refresh', tokens.refresh_token);
    else storage.removeItem('ferro_refresh');
  }
}

async function parse(res) {
  if (res.status === 204) return null;
  const text = await res.text();
  let json = null;
  try {
    json = text ? JSON.parse(text) : null;
  } catch {
    /* not JSON */
  }
  if (!res.ok) {
    const e = json?.errors?.[0];
    throw new ApiError(e?.message || `HTTP ${res.status}`, res.status, e?.extensions?.code);
  }
  return json;
}

export async function refreshSession() {
  if (!state.refresh) return false;
  if (!refreshing) {
    refreshing = (async () => {
      try {
        const res = await fetch(`${state.base}/auth/refresh`, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ refresh_token: state.refresh }),
        });
        const json = await parse(res);
        setTokens(json.data);
        return true;
      } catch {
        setTokens(null);
        return false;
      } finally {
        refreshing = null;
      }
    })();
  }
  return refreshing;
}

async function request(method, path, { query, body, form } = {}, retry = true) {
  const headers = {};
  if (state.access) headers.Authorization = `Bearer ${state.access}`;
  let payload;
  if (form) payload = form;
  else if (body !== undefined) {
    headers['Content-Type'] = 'application/json';
    payload = JSON.stringify(body);
  }
  const res = await fetch(`${state.base}${path}${qs(query)}`, { method, headers, body: payload });
  if (res.status === 401 && retry && !path.startsWith('/auth/') && state.refresh) {
    if (await refreshSession()) return request(method, path, { query, body, form }, false);
    if (state.onAuthLost) state.onAuthLost();
  }
  return parse(res);
}

export const api = {
  get: (path, query) => request('GET', path, { query }),
  post: (path, body, query) => request('POST', path, { body, query }),
  put: (path, body, query) => request('PUT', path, { body, query }),
  patch: (path, body, query) => request('PATCH', path, { body, query }),
  del: (path, query, body) => request('DELETE', path, { query, body }),
  upload: (path, form, query) => request('POST', path, { form, query }),
};

export async function login(email, password) {
  const res = await fetch(`${state.base}/auth/login`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ email, password }),
  });
  const json = await parse(res);
  setTokens(json.data);
}

/** Try to resume a session from the stored refresh token. */
export async function restoreSession() {
  return refreshSession();
}

export async function logout() {
  const token = state.refresh;
  setTokens(null);
  if (token) {
    try {
      await fetch(`${state.base}/auth/logout`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ refresh_token: token }),
      });
    } catch {
      /* best effort */
    }
  }
}

/** URL of a file's binary, usable in <img src> and links (token in the query). */
export function assetUrl(id, extra = '') {
  return `${state.base}/assets/${id}?access_token=${encodeURIComponent(state.access || '')}${extra}`;
}

/** Raster types the server can resize (SVG and the like are served as they are). */
const RESIZABLE = /^image\/(jpeg|png|gif|webp|bmp|tiff)$/;

/**
 * URL of a square thumbnail (`size` px, cropped to fill) for list views and previews, so a grid of large photos
 * does not download every original. Falls back to the original when the type is unknown or not resizable.
 */
export function thumbUrl(id, type, size = 240) {
  return RESIZABLE.test(String(type || '')) ? assetUrl(id, `&width=${size}&height=${size}&fit=cover`) : assetUrl(id);
}

/** Download an authenticated resource as a file. */
export async function download(path, filename, query) {
  const headers = state.access ? { Authorization: `Bearer ${state.access}` } : {};
  const res = await fetch(`${state.base}${path}${qs(query)}`, { headers });
  if (!res.ok) throw new ApiError(`HTTP ${res.status}`, res.status);
  const blob = await res.blob();
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = filename;
  document.body.append(a);
  a.click();
  a.remove();
  URL.revokeObjectURL(url);
}
