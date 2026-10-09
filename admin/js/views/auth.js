// Screens that appear before anybody is signed in: asking for a password reset, choosing a new password from the emailed
// link, accepting an invitation, and the return from a single sign-on provider.

import { acceptInvitation, exchangeSsoCode, requestPasswordReset, resetPassword } from '../api.js';
import { h, replace } from '../dom.js';

function card(root, ...content) {
  replace(root, h('div', { class: 'login-page' }, h('div', { class: 'login-card' }, h('h1', {}, 'Ferro'), ...content)));
}

function formCard(root, { title, intro, fields, submitLabel, onSubmit, footer }) {
  const error = h('div', { class: 'error-box', hidden: true });
  const submit = h('button', { type: 'submit', class: 'primary' }, submitLabel);
  const form = h(
    'form',
    {
      class: 'login-card',
      onSubmit: async (e) => {
        e.preventDefault();
        submit.disabled = true;
        error.hidden = true;
        try {
          await onSubmit();
        } catch (err) {
          error.textContent = err.message || 'Something went wrong';
          error.hidden = false;
        } finally {
          submit.disabled = false;
        }
      },
    },
    h('h1', {}, 'Ferro'),
    h('h2', {}, title),
    intro ? h('p', { class: 'muted' }, intro) : null,
    error,
    fields,
    submit,
    footer || null,
  );
  replace(root, h('div', { class: 'login-page' }, form));
}

/** "Forgot your password?": ask for the email. The answer never says whether the address has an account. */
export function forgotView(root, back, prefill = '') {
  const email = h('input', { type: 'email', required: true, autocomplete: 'username', value: prefill, autofocus: true });
  formCard(root, {
    title: 'Reset your password',
    intro: 'Enter the email address of your account. If it exists, we will send you a link to choose a new password.',
    fields: [h('label', {}, 'Email', email)],
    submitLabel: 'Send the link',
    footer: h('p', {}, h('a', { href: '#', onClick: (e) => (e.preventDefault(), back()) }, 'Back to sign in')),
    onSubmit: async () => {
      await requestPasswordReset(email.value.trim());
      card(
        root,
        h('h2', {}, 'Check your email'),
        h('p', {}, 'If that address belongs to an account, a message with a reset link is on its way. The link works once and expires in one hour.'),
        h('p', {}, h('a', { href: '#', onClick: (e) => (e.preventDefault(), back()) }, 'Back to sign in')),
      );
    },
  });
}

function passwordPair() {
  const a = h('input', { type: 'password', required: true, autocomplete: 'new-password', minlength: '8', autofocus: true });
  const b = h('input', { type: 'password', required: true, autocomplete: 'new-password', minlength: '8' });
  return {
    fields: [h('label', {}, 'New password', a, h('span', { class: 'muted small' }, 'At least 8 characters.')), h('label', {}, 'Repeat the password', b)],
    value() {
      if (a.value.length < 8) throw new Error('The password must be at least 8 characters long.');
      if (a.value !== b.value) throw new Error('The two passwords are not the same.');
      return a.value;
    },
  };
}

/** The emailed link: choose a new password. */
export function resetView(root, token, back) {
  const pw = passwordPair();
  formCard(root, {
    title: 'Choose a new password',
    fields: pw.fields,
    submitLabel: 'Change the password',
    onSubmit: async () => {
      await resetPassword(token, pw.value());
      card(root, h('h2', {}, 'Password changed'), h('p', {}, 'You have been signed out everywhere. Sign in with your new password.'), h('p', {}, h('a', { href: '#', onClick: (e) => (e.preventDefault(), back()) }, 'Go to sign in')));
    },
  });
}

/** The invitation link: choose a password; the account becomes active and the user is signed in. */
export function inviteView(root, token, onSuccess) {
  const pw = passwordPair();
  formCard(root, {
    title: 'Welcome',
    intro: 'You have been invited. Choose a password to activate your account.',
    fields: pw.fields,
    submitLabel: 'Activate and sign in',
    onSubmit: async () => {
      await acceptInvitation(token, pw.value());
      await onSuccess();
    },
  });
}

const SSO_ERRORS = {
  denied: 'You are not allowed to sign in this way. Ask an administrator for access.',
  domain: 'Your email address is not from an organisation that may sign in here.',
  email: 'The identity provider did not confirm your email address.',
  state: 'The sign-in was interrupted or took too long. Please try again.',
  token: 'The identity provider’s answer could not be verified. Please try again.',
  provider: 'The identity provider could not be reached or refused the request. Please try again.',
  server: 'Something went wrong on our side. Please try again.',
};

/** Return from a single sign-on provider: `#/sso?code=...` (success) or `#/sso?error=...`. */
export async function ssoView(root, params, onSuccess, back) {
  const failure = (text) =>
    card(root, h('h2', {}, 'Sign-in failed'), h('div', { class: 'error-box' }, text), h('p', {}, h('a', { href: '#', onClick: (e) => (e.preventDefault(), back()) }, 'Back to sign in')));
  const code = params.get('code');
  const err = params.get('error');
  if (err || !code) return failure(SSO_ERRORS[err] || SSO_ERRORS.provider);
  card(root, h('p', { class: 'muted' }, 'Signing you in…'));
  try {
    await exchangeSsoCode(code);
  } catch (e) {
    return failure(e.message || SSO_ERRORS.server);
  }
  await onSuccess();
}
