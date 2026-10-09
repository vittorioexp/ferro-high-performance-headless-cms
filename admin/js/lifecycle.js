// Cleanup hooks of the current view: subscriptions, timers... run when the user navigates away.

const hooks = [];

/** Register a cleanup for the view being rendered. */
export function onLeave(fn) {
  hooks.push(fn);
}

export function runLeave() {
  while (hooks.length) {
    try {
      hooks.pop()();
    } catch (err) {
      console.error('view cleanup failed:', err);
    }
  }
}
