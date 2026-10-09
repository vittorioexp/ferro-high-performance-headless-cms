// "My account": who I am and two-factor authentication (set up, recovery codes, turn off).

import { api } from '../api.js';
import { add, button, fail, h, inputEl, labeled, replace, toast } from '../dom.js';

const spaced = (secret) => secret.replace(/(.{4})/g, '$1 ').trim();

export async function accountView(root) {
  const holder = h('div', { id: 'tfa-holder' });
  const head = h('div', { class: 'card' });
  add(root, h('div', { class: 'page-head' }, h('h1', {}, 'My account')), head, holder);

  const draw = async () => {
    const me = (await api.get('/users/me')).data;
    replace(
      head,
      h('p', {}, h('strong', {}, me.email), me.first_name || me.last_name ? ` — ${[me.first_name, me.last_name].filter(Boolean).join(' ')}` : ''),
      h('p', { class: 'muted' }, me.provider ? `You sign in through your organisation (${me.provider}).` : 'You sign in with a Ferro password.'),
    );
    replace(holder, me.tfa_enabled ? enabledCard(draw) : setupCard(draw));
  };
  await draw();
}

function setupCard(refresh) {
  const box = h('div', { class: 'card' });
  const intro = () =>
    replace(
      box,
      h('h2', {}, 'Two-factor authentication'),
      h('p', { class: 'muted' }, 'Off. Add a second step to your sign-in: a code from an authenticator app (Microsoft Authenticator, Google Authenticator, 1Password, …).'),
      button('Set up two-factor authentication', () => askPassword(), 'primary'),
    );

  const askPassword = () => {
    let password = '';
    replace(
      box,
      h('h2', {}, 'Confirm it is you'),
      labeled('Current password', inputEl('', (v) => (password = v), { type: 'password', autocomplete: 'current-password' }), 'Leave empty if you sign in through your organisation.'),
      h('div', { class: 'row' }, button('Continue', () => start(password).catch(fail), 'primary'), button('Cancel', intro)),
    );
  };

  const start = async (password) => {
    const r = (await api.post('/auth/tfa/generate', password ? { password } : {})).data;
    let otp = '';
    replace(
      box,
      h('h2', {}, 'Scan the code'),
      h('p', {}, 'Open your authenticator app, add an account and scan this QR code.'),
      h('img', { class: 'qr', alt: 'QR code to scan with your authenticator app', width: '220', height: '220', src: `data:image/svg+xml;charset=utf-8,${encodeURIComponent(r.qr_svg)}` }),
      h('p', { class: 'muted small' }, 'Cannot scan it? Type this key into the app instead:'),
      h('code', { class: 'secret' }, spaced(r.secret)),
      labeled('Code shown by the app', inputEl('', (v) => (otp = v.trim()), { inputmode: 'numeric', autocomplete: 'one-time-code' }), 'Six digits. This proves the app is set up correctly.'),
      h('div', { class: 'row' }, button('Turn on', () => finish(r.secret, otp).catch(fail), 'primary'), button('Cancel', intro)),
    );
  };

  const finish = async (secret, otp) => {
    const r = (await api.post('/auth/tfa/enable', { secret, otp })).data;
    const text = r.recovery_codes.join('\n');
    replace(
      box,
      h('h2', {}, 'Save your recovery codes'),
      h('p', {}, 'If you lose your phone, each of these codes lets you sign in once. They are shown only now: keep them somewhere safe.'),
      h('pre', { class: 'token recovery' }, text),
      h('div', { class: 'row' },
        button('Copy', async () => {
          try {
            await navigator.clipboard.writeText(text);
            toast('Copied', 'ok');
          } catch {
            toast('Select the codes and copy them by hand', 'error');
          }
        }, 'sm'),
        h('a', { class: 'button-link', download: 'ferro-recovery-codes.txt', href: `data:text/plain;charset=utf-8,${encodeURIComponent(`${text}\n`)}` }, 'Download'),
      ),
      h('div', { class: 'row' }, button('I have saved them', () => refresh().catch(fail), 'primary')),
    );
  };

  intro();
  return box;
}

function enabledCard(refresh) {
  const box = h('div', { class: 'card' });
  const intro = () =>
    replace(
      box,
      h('h2', {}, 'Two-factor authentication'),
      h('p', {}, h('strong', { class: 'ok' }, 'On.'), ' Signing in asks for a code from your authenticator app.'),
      button('Turn off…', () => confirm(), 'sm'),
    );
  const confirm = () => {
    let password = '';
    let otp = '';
    replace(
      box,
      h('h2', {}, 'Turn off two-factor authentication'),
      labeled('Current password', inputEl('', (v) => (password = v), { type: 'password', autocomplete: 'current-password' }), 'Leave empty if you sign in through your organisation.'),
      labeled('Code from the app (or a recovery code)', inputEl('', (v) => (otp = v.trim()), { autocomplete: 'one-time-code' })),
      h('div', { class: 'row' },
        button('Turn off', async () => {
          try {
            await api.post('/auth/tfa/disable', { otp, ...(password ? { password } : {}) });
            toast('Two-factor authentication is off', 'ok');
            await refresh();
          } catch (e) {
            fail(e);
          }
        }, 'danger'),
        button('Cancel', intro),
      ),
    );
  };
  intro();
  return box;
}
