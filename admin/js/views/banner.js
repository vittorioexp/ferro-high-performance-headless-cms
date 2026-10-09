// "Something changed" notice shown by views when a realtime event arrives.

import { button, h } from '../dom.js';

/** Hidden by default. `show(text)` reveals it; the button runs `onRefresh` and hides it. */
export function changeBanner(label, onRefresh) {
  const text = h('span', {}, '');
  const el = h('div', { class: 'banner', hidden: true }, text, button(label, () => {
    el.hidden = true;
    return onRefresh();
  }, 'sm'));
  return {
    el,
    show(message) {
      text.textContent = message;
      el.hidden = false;
    },
    hide() {
      el.hidden = true;
    },
  };
}
