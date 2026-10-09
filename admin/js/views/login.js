import { authMethods, login, ssoUrl } from '../api.js';
import { h, replace } from '../dom.js';
import { forgotView } from './auth.js';

/**
 * Render the sign-in form into `root`; `onSuccess` runs after a successful login.
 *
 * Besides email and password it offers, depending on what the server is configured for: "Forgot your password?",
 * one button per single sign-on provider, and — for an account with two-factor authentication — a second step asking
 * for the code from the authenticator app (or a recovery code).
 */
export function loginView(root, onSuccess, message) {
  const error = h('div', { class: 'error-box', hidden: true });
  const email = h('input', { type: 'email', required: true, autocomplete: 'username', autofocus: true });
  const password = h('input', { type: 'password', required: true, autocomplete: 'current-password' });
  const otp = h('input', { type: 'text', inputmode: 'numeric', autocomplete: 'one-time-code', 'aria-label': 'Authentication code', placeholder: '123456 or a recovery code' });
  const otpRow = h('label', { class: 'otp-row', hidden: true }, 'Authentication code', otp, h('span', { class: 'muted small' }, 'From your authenticator app, or one of your recovery codes.'));
  const submit = h('button', { type: 'submit', class: 'primary' }, 'Sign in');
  const extras = h('div', { class: 'login-extras' });
  let needOtp = false;

  const form = h(
    'form',
    {
      class: 'login-card',
      onSubmit: async (e) => {
        e.preventDefault();
        submit.disabled = true;
        error.hidden = true;
        try {
          await login(email.value.trim(), password.value, needOtp ? otp.value.trim() : undefined);
          await onSuccess();
        } catch (err) {
          if (err.code === 'OTP_REQUIRED') {
            needOtp = true;
            otpRow.hidden = false;
            email.readOnly = true;
            password.readOnly = true;
            if (otp.focus) otp.focus();
          } else {
            error.textContent = err.message || 'Sign-in failed';
            error.hidden = false;
            if (err.code === 'INVALID_OTP') otp.value = '';
          }
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
    otpRow,
    submit,
    extras,
  );
  replace(root, h('div', { class: 'login-page' }, form));

  // what else the server offers (never blocks the form)
  authMethods().then((m) => {
    const providers = m.providers || [];
    replace(
      extras,
      m.password_reset ? h('a', { href: '#/forgot', class: 'forgot-link', onClick: (e) => (e.preventDefault(), forgotView(root, () => loginView(root, onSuccess, message), email.value.trim())) }, 'Forgot your password?') : null,
      providers.length ? h('div', { class: 'sso-divider muted small' }, 'or') : null,
      providers.map((p) => h('a', { class: 'button-link sso-button', href: ssoUrl(p.name), 'data-provider': p.name }, p.label || `Sign in with ${p.name}`)),
    );
  });
}
