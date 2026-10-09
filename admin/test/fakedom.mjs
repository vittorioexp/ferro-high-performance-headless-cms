// Minimal DOM, just enough to execute the admin modules under Node.

export class FakeText {
  constructor(text) {
    this.nodeType = 3;
    this.textContent = text;
  }
}

export class FakeEl {
  constructor(tag) {
    this.nodeType = 1;
    this.tagName = tag.toUpperCase();
    this.children = [];
    this.parent = null;
    this.attrs = {};
    this.style = {};
    this.handlers = {};
    this.className = '';
    this.value = '';
  }
  append(...nodes) {
    for (let n of nodes) {
      // like the browser: anything that is not a Node becomes text (arrays -> "a,b", null -> "null")
      if (!n || typeof n !== 'object' || !n.nodeType) n = new FakeText(String(n));
      if (n.parent) n.remove();
      n.parent = this;
      this.children.push(n);
    }
  }
  replaceChildren(...nodes) {
    this.children.forEach((c) => (c.parent = null));
    this.children = [];
    this.append(...nodes);
  }
  replaceWith(node) {
    const p = this.parent;
    if (!p) return;
    p.children[p.children.indexOf(this)] = node;
    node.parent = p;
    this.parent = null;
  }
  remove() {
    if (this.parent) this.parent.children = this.parent.children.filter((c) => c !== this);
    this.parent = null;
  }
  setAttribute(k, v) {
    this.attrs[k] = v;
  }
  addEventListener(type, fn) {
    (this.handlers[type] ||= []).push(fn);
  }
  get text() {
    return this.children.map((c) => (c.nodeType === 3 ? c.textContent : c.text)).join('');
  }
}

export function install() {
  const body = new FakeEl('body');
  globalThis.document = {
    body,
    createElement: (t) => new FakeEl(t),
    createTextNode: (t) => new FakeText(t),
    getElementById: () => null,
  };
  return body;
}

export function walk(node, fn) {
  if (node.nodeType === 1) {
    fn(node);
    node.children.forEach((c) => walk(c, fn));
  }
}

export function findAll(root, pred) {
  const out = [];
  walk(root, (n) => pred(n) && out.push(n));
  return out;
}

export async function fire(el, type, extra = {}) {
  for (const fn of el.handlers[type] || []) await fn({ target: el, ...extra });
}
