import { login } from '../api.js';
import { h, replace } from '../dom.js';

/** Render the sign-in form into `root`; `onSuccess` runs after a successful login. */
export function loginView(root, onSuccess, message) {
  const error = h('div', { class: 'error-box', hidden: true });
  const email = h('input', { type: 'email', required: true, autocomplete: 'username', autofocus: true });
  const password = h('input', { type: 'password', required: true, autocomplete: 'current-password' });
  const submit = h('button', { type: 'submit', class: 'primary' }, 'Sign in');

  const form = h(
    'form',
    {
      class: 'login-card',
      onSubmit: async (e) => {
        e.preventDefault();
        submit.disabled = true;
        error.hidden = true;
        try {
          await login(email.value.trim(), password.value);
          await onSuccess();
        } catch (err) {
          error.textContent = err.message || 'Sign-in failed';
          error.hidden = false;
        } finally {
          submit.disabled = false;
        }
      },
    },
    h('h1', {}, 'Ferro'),
    message ? h('p', { class: 'muted' }, message) : null,
    error,
    h('label', {}, 'Email', email),
    h('label', {}, 'Password', password),
    submit,
  );
  replace(root, h('div', { class: 'login-page' }, form));
}
