// Tiny DOM toolkit. Text is always inserted as text nodes, never as HTML.

export function append(el, children) {
  for (const child of children.flat(Infinity)) {
    if (child === null || child === undefined || child === false) continue;
    el.append(child.nodeType ? child : document.createTextNode(String(child)));
  }
  return el;
}

/** Append children to `el`; null, false and nested arrays are accepted (the native append would print "null"). */
export function add(el, ...children) {
  return append(el, children);
}

/** Replace the children of `el`. Unlike the native replaceChildren, accepts arrays, null and strings. */
export function replace(el, ...children) {
  el.replaceChildren();
  return append(el, children);
}

export function h(tag, props, ...children) {
  const el = document.createElement(tag);
  let deferredValue;
  for (const [k, v] of Object.entries(props || {})) {
    if (v === undefined || v === null || v === false) continue;
    if (k === 'class') el.className = v;
    else if (k === 'style' && typeof v === 'object') Object.assign(el.style, v);
    else if (k === 'value') deferredValue = v; // after children: <select> needs its options first
    else if (['checked', 'disabled', 'selected', 'multiple', 'readOnly'].includes(k)) el[k] = v;
    else if (k.startsWith('on') && typeof v === 'function') el.addEventListener(k.slice(2).toLowerCase(), v);
    else el.setAttribute(k, v === true ? '' : String(v));
  }
  append(el, children);
  if (deferredValue !== undefined) el.value = deferredValue;
  return el;
}

export function toast(message, kind = 'info') {
  let box = document.getElementById('toasts');
  if (!box) {
    box = h('div', { id: 'toasts' });
    document.body.append(box);
  }
  const t = h('div', { class: `toast ${kind}` }, message);
  box.append(t);
  setTimeout(() => t.remove(), kind === 'error' ? 8000 : 3500);
}

/** Show an error from an async handler. */
export function fail(err) {
  console.error(err);
  toast(err?.message || String(err), 'error');
}

// Open dialogs, topmost last: Escape closes the topmost one.
const openModals = [];
let escapeBound = false;

function onEscape(e) {
  if (e.key === 'Escape' && openModals.length) openModals[openModals.length - 1]();
}

export function modal({ title, body, buttons = [], wide = false, onClose }) {
  const overlay = h('div', { class: 'overlay' });
  let closed = false;
  const close = () => {
    if (closed) return;
    closed = true;
    const i = openModals.indexOf(close);
    if (i >= 0) openModals.splice(i, 1);
    overlay.remove();
    if (onClose) onClose();
  };
  openModals.push(close);
  if (!escapeBound && typeof document.addEventListener === 'function') {
    document.addEventListener('keydown', onEscape);
    escapeBound = true;
  }
  const foot = buttons.map((b) =>
    h(
      'button',
      {
        type: 'button',
        class: b.class || '',
        onClick: async (e) => {
          try {
            if (b.onClick) {
              const r = await b.onClick(e);
              if (r === false) return;
            }
            if (b.close !== false) close();
          } catch (err) {
            fail(err);
          }
        },
      },
      b.label,
    ),
  );
  const box = h(
    'div',
    { class: `modal${wide ? ' wide' : ''}` },
    h('div', { class: 'modal-head' }, h('h3', {}, title), h('button', { type: 'button', class: 'icon', 'aria-label': 'Close', onClick: close }, '×')),
    h('div', { class: 'modal-body' }, body),
    foot.length ? h('div', { class: 'modal-foot' }, foot) : null,
  );
  overlay.append(box);
  document.body.append(overlay);
  return { close, el: box };
}

export function confirmDialog(message, { label = 'Confirm', danger = false } = {}) {
  return new Promise((resolve) => {
    let answered = false;
    const answer = (v) => {
      if (!answered) {
        answered = true;
        resolve(v);
      }
    };
    modal({
      title: 'Please confirm',
      body: h('p', {}, message),
      onClose: () => answer(false),
      buttons: [{ label: 'Cancel' }, { label, class: danger ? 'danger' : 'primary', onClick: () => answer(true) }],
    });
  });
}

// ---- form controls ---------------------------------------------------------

export function labeled(label, control, note) {
  return h('div', { class: 'field' }, h('label', {}, label), control, note ? h('div', { class: 'note' }, note) : null);
}

export function inputEl(value, onInput, attrs = {}) {
  return h('input', { type: 'text', value: value ?? '', ...attrs, onInput: (e) => onInput && onInput(e.target.value) });
}

export function checkbox(label, checked, onChange) {
  return h('label', { class: 'check' }, h('input', { type: 'checkbox', checked: !!checked, onChange: (e) => onChange && onChange(e.target.checked) }), ' ', label);
}

/** options: strings or {value,label}. */
export function selectEl(options, value, onChange, attrs = {}) {
  const opts = options.map((o) => (typeof o === 'string' ? { value: o, label: o } : o));
  return h(
    'select',
    { ...attrs, value: value ?? '', onChange: (e) => onChange && onChange(e.target.value) },
    opts.map((o) => h('option', { value: o.value }, o.label)),
  );
}

/** Button whose (async) handler errors are shown as toasts instead of being lost. */
export function button(label, onClick, cls = '') {
  const handler = (e) => {
    try {
      const r = onClick(e);
      if (r && typeof r.catch === 'function') r.catch(fail);
    } catch (err) {
      fail(err);
    }
  };
  return h('button', { type: 'button', class: cls, onClick: handler }, label);
}

/** Disable a button while an async action runs. */
export async function busy(btn, fn) {
  btn.disabled = true;
  try {
    return await fn();
  } finally {
    btn.disabled = false;
  }
}

/**
 * Show the plan of a schema change (dry run), then apply it on confirmation.
 * `run(query)` performs the request; it must resolve to the server response
 * ({ meta: { changes, sql } }).
 */
export async function applyWithPreview(title, run) {
  const dry = await run({ dry_run: true, allow_destructive: true });
  const changes = dry.meta?.changes || [];
  if (changes.length === 0) {
    toast('No changes to apply');
    return null;
  }
  const destructive = changes.some((c) => c.includes('[destructive]'));
  return new Promise((resolve) => {
    modal({
      title,
      wide: true,
      onClose: () => resolve(null),
      body: h(
        'div',
        {},
        h('ul', { class: 'changes' }, changes.map((c) => h('li', { class: c.includes('[destructive]') ? 'bad' : c.includes('[risky]') ? 'warn' : '' }, c))),
        destructive ? h('p', { class: 'bad' }, 'This change permanently deletes data.') : null,
        h('details', {}, h('summary', {}, 'SQL'), h('pre', {}, (dry.meta?.sql || []).map((s) => `${s};`).join('\n'))),
      ),
      buttons: [
        { label: 'Cancel' },
        {
          label: destructive ? 'Apply (destructive)' : 'Apply',
          class: destructive ? 'danger' : 'primary',
          onClick: async () => {
            resolve(await run({ allow_destructive: destructive }));
          },
        },
      ],
    });
  });
}
