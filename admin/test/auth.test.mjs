// Sign-in screens and account security, executed on the fake DOM against a simulated server.
import assert from 'node:assert/strict';
import { test } from 'node:test';

import { FakeEl, findAll, fire, install } from './fakedom.mjs';

const body = install();
globalThis.location = { origin: 'https://cms.example.com', pathname: '/admin/', hash: '', search: '', reload() { this.reloaded = true; } };
globalThis.window = { confirm: () => true };
const realSetTimeout = globalThis.setTimeout;
globalThis.setTimeout = (fn, ms) => {
  const t = realSetTimeout(fn, ms);
  t.unref?.();
  return t;
};
const wait = (ms) => new Promise((r) => realSetTimeout(r, ms));

// ---- a tiny server -----------------------------------------------------------------------------------------------------------------
const server = {
  methods: { password_reset: false, invitations: false, providers: [], directory: false },
  otpRequired: false,
  me: { id: 'u1', email: 'ann@example.com', first_name: 'Ann', last_name: 'Lee', tfa_enabled: false, provider: null },
  tokenExpired: false,
  invite: { delivery: 'email', email: 'new@example.com', invite_url: 'https://cms.example.com/admin/#/invite?token=' + 'ab'.repeat(32) },
  accessCount: 0,
};
const calls = [];
const reply = (status, json) => ({ ok: status < 400, status, text: async () => (json === null ? '' : JSON.stringify(json)) });
const err = (status, code, message) => reply(status, { errors: [{ message, extensions: { code } }] });

globalThis.fetch = async (url, init = {}) => {
  const method = init.method || 'GET';
  const path = new URL(String(url), 'http://x').pathname;
  const payload = init.body ? JSON.parse(init.body) : undefined;
  calls.push({ method, path, body: payload, auth: init.headers?.Authorization });
  if (path === '/auth/methods') return reply(200, { data: server.methods });
  if (path === '/auth/login') {
    if (payload.password !== 'right-password') return err(401, 'UNAUTHORIZED', 'invalid credentials');
    if (server.otpRequired && !payload.otp) return err(401, 'OTP_REQUIRED', 'a two-factor code is required');
    if (server.otpRequired && payload.otp !== '123456') return err(401, 'INVALID_OTP', 'invalid two-factor code');
    return reply(200, { data: { access_token: 'access-1', refresh_token: 'refresh-1', expires: 900000 } });
  }
  if (path === '/auth/refresh') return reply(200, { data: { access_token: 'access-2', refresh_token: 'refresh-2', expires: 900000 } });
  if (path === '/auth/password/request') return reply(202, { data: { message: 'ok' } });
  if (path === '/auth/password/reset') return payload.token === 'good-token' ? reply(204, null) : err(401, 'UNAUTHORIZED', 'this link is invalid or has expired');
  if (path === '/auth/invite/accept') return payload.token === 'good-token' ? reply(200, { data: { access_token: 'access-i', refresh_token: 'refresh-i', expires: 900000 } }) : err(401, 'UNAUTHORIZED', 'this invitation is invalid or has expired');
  if (path === '/auth/oauth/exchange') return payload.code === 'good-code' ? reply(200, { data: { access_token: 'access-s', refresh_token: 'refresh-s', expires: 900000 } }) : err(401, 'UNAUTHORIZED', 'this sign-in code is invalid or has expired');
  if (path === '/users/me') return reply(200, { data: server.me });
  if (path === '/auth/tfa/generate') return reply(200, { data: { secret: 'JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP', otpauth_url: 'otpauth://totp/x', qr_svg: '<svg xmlns="http://www.w3.org/2000/svg"><rect width="1" height="1"/></svg>' } });
  if (path === '/auth/tfa/enable') {
    if (server.tokenExpired && init.headers?.Authorization === 'Bearer access-1') return err(401, 'UNAUTHORIZED', 'invalid token');
    if (payload.otp !== '654321') return err(401, 'INVALID_OTP', 'invalid two-factor code');
    server.me = { ...server.me, tfa_enabled: true };
    return reply(200, { data: { recovery_codes: ['aaaaa-bbbbb', 'ccccc-ddddd'] } });
  }
  if (path === '/auth/tfa/disable') {
    server.me = { ...server.me, tfa_enabled: false };
    return reply(204, null);
  }
  if (path === '/users') return reply(200, { data: [{ id: 'u1', email: 'a@b.c', status: 'active', provider: 'azure', tfa_enabled: true }, { id: 'u2', email: 'd@e.f', status: 'active', provider: null, tfa_enabled: false }] });
  if (path === '/roles') return reply(200, { data: [{ id: 'r1', name: 'Editor' }] });
  if (path === '/users/invite') return reply(200, { data: { id: 'u9', status: 'invited', ...server.invite } });
  if (path === '/users/u1/tfa') return reply(204, null);
  throw new Error(`unmocked ${method} ${path}`);
};

const api = await import('../js/api.js');
const { loginView } = await import('../js/views/login.js');
const { forgotView, inviteView, resetView, ssoView } = await import('../js/views/auth.js');
const { accountView } = await import('../js/views/account.js');
const { usersView } = await import('../js/views/users.js');

const submit = (root) => fire(findAll(root, (n) => n.tagName === 'FORM')[0], 'submit', { preventDefault() {} });
const input = (root, pred) => findAll(root, (n) => n.tagName === 'INPUT' && pred(n))[0];
const typeInto = async (el, value) => {
  el.value = value;
  await fire(el, 'input');
};
const byText = (root, tag, text) => findAll(root, (n) => n.tagName === tag && n.text === text)[0];
const links = (root) => findAll(root, (n) => n.tagName === 'A');

// ---- sign-in ------------------------------------------------------------------------------------------------------------------------------
test('a plain server shows just email and password', async () => {
  const root = new FakeEl('div');
  loginView(root, async () => {});
  await wait(10);
  assert.equal(findAll(root, (n) => n.tagName === 'INPUT').length, 3, 'email, password and the (hidden) code field');
  assert.equal(links(root).length, 0, 'no forgot link, no provider buttons');
  assert.equal(findAll(root, (n) => n.attrs['aria-label'] === 'Authentication code')[0].parent.attrs.hidden, '', 'the code row starts hidden');
});

test('the server decides what the sign-in page offers: forgot link and provider buttons', async () => {
  server.methods = { password_reset: true, invitations: true, providers: [{ name: 'azure', label: 'Sign in with Microsoft' }, { name: 'okta', label: 'Sign in with Okta' }], directory: false };
  const root = new FakeEl('div');
  loginView(root, async () => {});
  await wait(10);
  assert.ok(byText(root, 'A', 'Forgot your password?'));
  const sso = links(root).filter((a) => a.attrs['data-provider']);
  assert.deepEqual(sso.map((a) => a.text), ['Sign in with Microsoft', 'Sign in with Okta']);
  const href = new URL(sso[0].attrs.href, 'https://cms.example.com');
  assert.equal(href.pathname, '/auth/oauth/azure');
  assert.equal(href.searchParams.get('redirect'), 'https://cms.example.com/admin/#/sso', 'the provider sends the user back to the admin app');
});

test('a wrong password shows the server’s message and stays on the first step', async () => {
  const root = new FakeEl('div');
  loginView(root, async () => assert.fail('must not succeed'));
  await typeInto(input(root, (n) => n.attrs.type === 'email'), 'ann@example.com');
  await typeInto(input(root, (n) => n.attrs.type === 'password'), 'nope');
  await submit(root);
  const box = findAll(root, (n) => n.className === 'error-box')[0];
  assert.match(box.textContent ?? box.text, /invalid credentials/);
  assert.equal(findAll(root, (n) => n.attrs['aria-label'] === 'Authentication code')[0].parent.hidden, undefined, 'still hidden');
});

test('two-factor sign-in: the code field appears, the credentials are kept, a wrong code is reported', async () => {
  server.otpRequired = true;
  calls.length = 0;
  let signedIn = 0;
  const root = new FakeEl('div');
  loginView(root, async () => signedIn++);
  const email = input(root, (n) => n.attrs.type === 'email');
  const password = input(root, (n) => n.attrs.type === 'password');
  const code = input(root, (n) => n.attrs['aria-label'] === 'Authentication code');
  await typeInto(email, 'ann@example.com');
  await typeInto(password, 'right-password');
  await submit(root);
  assert.equal(signedIn, 0);
  assert.equal(code.parent.hidden, false, 'the code row is now shown');
  assert.ok(email.readOnly && password.readOnly, 'credentials are locked while the code is asked for');
  assert.equal(findAll(root, (n) => n.className === 'error-box')[0].hidden, true, 'asking for the code is not an error');
  await typeInto(code, '000000');
  await submit(root);
  assert.match(findAll(root, (n) => n.className === 'error-box')[0].textContent, /invalid two-factor code/);
  assert.equal(code.value, '', 'a wrong code is cleared');
  await typeInto(code, ' 123456 ');
  await submit(root);
  assert.equal(signedIn, 1);
  const logins = calls.filter((c) => c.path === '/auth/login').map((c) => c.body);
  assert.deepEqual(logins.map((b) => b.otp), [undefined, '000000', '123456'], 'the code is sent only once asked for, and trimmed');
  assert.ok(logins.every((b) => b.password === 'right-password'));
  server.otpRequired = false;
});

// ---- forgot / reset / invite / single sign-on ----------------------------------------------------------------------------------------------------
test('forgot password: asks for the email and never says whether it exists', async () => {
  calls.length = 0;
  const root = new FakeEl('div');
  let back = 0;
  forgotView(root, () => back++, 'prefilled@example.com');
  assert.equal(input(root, (n) => n.attrs.type === 'email').value, 'prefilled@example.com');
  await submit(root);
  assert.deepEqual(calls.find((c) => c.path === '/auth/password/request').body, { email: 'prefilled@example.com' });
  assert.match(root.text, /Check your email/);
  assert.match(root.text, /If that address belongs to an account/);
  await fire(byText(root, 'A', 'Back to sign in'), 'click', { preventDefault() {} });
  assert.equal(back, 1);
});

test('reset password: validates on the client, then sends the token from the link', async () => {
  calls.length = 0;
  const root = new FakeEl('div');
  let back = 0;
  resetView(root, 'good-token', () => back++);
  const [a, b] = findAll(root, (n) => n.tagName === 'INPUT');
  await typeInto(a, 'short');
  await typeInto(b, 'short');
  await submit(root);
  assert.match(findAll(root, (n) => n.className === 'error-box')[0].textContent, /at least 8/);
  await typeInto(a, 'long-enough-1');
  await typeInto(b, 'different-1');
  await submit(root);
  assert.match(findAll(root, (n) => n.className === 'error-box')[0].textContent, /not the same/);
  assert.equal(calls.filter((c) => c.path === '/auth/password/reset').length, 0, 'nothing is sent until the form is valid');
  await typeInto(b, 'long-enough-1');
  await submit(root);
  assert.deepEqual(calls.find((c) => c.path === '/auth/password/reset').body, { token: 'good-token', password: 'long-enough-1' });
  assert.match(root.text, /Password changed/);
  await fire(byText(root, 'A', 'Go to sign in'), 'click', { preventDefault() {} });
  assert.equal(back, 1);
});

test('reset password: an expired link is explained', async () => {
  const root = new FakeEl('div');
  resetView(root, 'stale-token', () => {});
  const [a, b] = findAll(root, (n) => n.tagName === 'INPUT');
  await typeInto(a, 'long-enough-1');
  await typeInto(b, 'long-enough-1');
  await submit(root);
  assert.match(findAll(root, (n) => n.className === 'error-box')[0].textContent, /invalid or has expired/);
});

test('invitation: choosing a password signs the user in', async () => {
  calls.length = 0;
  const root = new FakeEl('div');
  let signedIn = 0;
  inviteView(root, 'good-token', async () => signedIn++);
  const [a, b] = findAll(root, (n) => n.tagName === 'INPUT');
  await typeInto(a, 'welcome-pass-1');
  await typeInto(b, 'welcome-pass-1');
  await submit(root);
  assert.equal(signedIn, 1);
  assert.deepEqual(calls.find((c) => c.path === '/auth/invite/accept').body, { token: 'good-token', password: 'welcome-pass-1' });
  assert.equal(api.hasSession(), true, 'tokens were stored');
});

test('single sign-on return: a code is exchanged, an error is explained', async () => {
  calls.length = 0;
  let signedIn = 0;
  const root = new FakeEl('div');
  await ssoView(root, new URLSearchParams('code=good-code'), async () => signedIn++, () => {});
  assert.equal(signedIn, 1);
  assert.deepEqual(calls.find((c) => c.path === '/auth/oauth/exchange').body, { code: 'good-code' });

  for (const [query, pattern] of [['error=denied', /not allowed to sign in this way/], ['error=domain', /organisation/], ['error=state', /interrupted/], ['error=token', /could not be verified/], ['error=whatever', /could not be reached/], ['', /could not be reached/]]) {
    const r = new FakeEl('div');
    let back = 0;
    await ssoView(r, new URLSearchParams(query), async () => assert.fail('must not sign in'), () => back++);
    assert.match(r.text, /Sign-in failed/);
    assert.match(r.text, pattern, query);
    await fire(byText(r, 'A', 'Back to sign in'), 'click', { preventDefault() {} });
    assert.equal(back, 1);
  }
  const used = new FakeEl('div');
  await ssoView(used, new URLSearchParams('code=used-code'), async () => assert.fail('must not sign in'), () => {});
  assert.match(used.text, /invalid or has expired/, 'a spent code is reported');
});

// ---- account / two-factor --------------------------------------------------------------------------------------------------------------------------
test('account: set up two-factor, see the QR code and the recovery codes', async () => {
  server.me = { id: 'u1', email: 'ann@example.com', first_name: 'Ann', last_name: 'Lee', tfa_enabled: false, provider: null };
  await api.login('ann@example.com', 'right-password');
  calls.length = 0;
  const root = new FakeEl('div');
  await accountView(root);
  assert.match(root.text, /ann@example\.com/);
  assert.match(root.text, /sign in with a Ferro password/);
  await fire(byText(root, 'BUTTON', 'Set up two-factor authentication'), 'click');
  const pw = input(root, (n) => n.attrs.type === 'password');
  await typeInto(pw, 'right-password');
  await fire(byText(root, 'BUTTON', 'Continue'), 'click');
  await wait(10);
  assert.deepEqual(calls.find((c) => c.path === '/auth/tfa/generate').body, { password: 'right-password' });
  const qr = findAll(root, (n) => n.tagName === 'IMG' && n.className === 'qr')[0];
  assert.match(qr.attrs.src, /^data:image\/svg\+xml;charset=utf-8,%3Csvg/, 'the QR code is an image, never injected as markup');
  assert.match(root.text, /JBSW Y3DP EHPK 3PXP/, 'the secret is shown in groups for typing by hand');

  const code = input(root, (n) => n.attrs.autocomplete === 'one-time-code');
  await typeInto(code, '111111');
  await fire(byText(root, 'BUTTON', 'Turn on'), 'click');
  await wait(10);
  assert.ok(body.children.at(-1) !== undefined);
  assert.match(findAll(body, (n) => n.className?.includes('toast'))[0]?.text ?? '', /invalid two-factor code/, 'a wrong code is reported without leaving the step');
  await typeInto(code, '654321');
  await fire(byText(root, 'BUTTON', 'Turn on'), 'click');
  await wait(10);
  assert.deepEqual(calls.filter((c) => c.path === '/auth/tfa/enable').at(-1).body, { secret: 'JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP', otp: '654321' });
  assert.match(root.text, /Save your recovery codes/);
  assert.match(root.text, /aaaaa-bbbbb\s*ccccc-ddddd/);
  const download = links(root).find((a) => a.attrs.download);
  assert.equal(decodeURIComponent(download.attrs.href.split(',')[1]).trim(), 'aaaaa-bbbbb\nccccc-ddddd');
  await fire(byText(root, 'BUTTON', 'I have saved them'), 'click');
  await wait(10);
  assert.match(root.text, /On\./, 'back on the card, now showing two-factor as on');
});

test('account: turning two-factor off needs the code (and the password when there is one)', async () => {
  server.me = { ...server.me, tfa_enabled: true };
  calls.length = 0;
  const root = new FakeEl('div');
  await accountView(root);
  await fire(byText(root, 'BUTTON', 'Turn off…'), 'click');
  await typeInto(input(root, (n) => n.attrs.type === 'password'), 'right-password');
  await typeInto(input(root, (n) => n.attrs.autocomplete === 'one-time-code'), 'abcde-fghij');
  await fire(byText(root, 'BUTTON', 'Turn off'), 'click');
  await wait(10);
  assert.deepEqual(calls.find((c) => c.path === '/auth/tfa/disable').body, { otp: 'abcde-fghij', password: 'right-password' });
  assert.match(root.text, /Set up two-factor authentication/);
});

test('account: a user who signs in through the organisation is not asked for a password they do not have', async () => {
  server.me = { ...server.me, tfa_enabled: false, provider: 'azure' };
  const root = new FakeEl('div');
  await accountView(root);
  assert.match(root.text, /through your organisation \(azure\)/);
  calls.length = 0;
  await fire(byText(root, 'BUTTON', 'Set up two-factor authentication'), 'click');
  await fire(byText(root, 'BUTTON', 'Continue'), 'click');
  await wait(10);
  assert.deepEqual(calls.find((c) => c.path === '/auth/tfa/generate').body, {}, 'no password sent');
});

test('a wrong one-time code is not retried (it would count as a second failure), an expired token is', async () => {
  server.me = { ...server.me, tfa_enabled: false, provider: null };
  await api.login('ann@example.com', 'right-password');
  calls.length = 0;
  await assert.rejects(() => api.api.post('/auth/tfa/enable', { secret: 's', otp: '000000' }), (e) => e.code === 'INVALID_OTP');
  assert.equal(calls.filter((c) => c.path === '/auth/tfa/enable').length, 1);
  assert.equal(calls.filter((c) => c.path === '/auth/refresh').length, 0);

  server.tokenExpired = true;
  calls.length = 0;
  const ok = await api.api.post('/auth/tfa/enable', { secret: 's', otp: '654321' });
  assert.deepEqual(ok.data.recovery_codes.length, 2);
  assert.deepEqual(calls.map((c) => c.path), ['/auth/tfa/enable', '/auth/refresh', '/auth/tfa/enable'], 'refreshed once and retried');
  assert.equal(calls.at(-1).auth, 'Bearer access-2');
  server.tokenExpired = false;
});

// ---- users ------------------------------------------------------------------------------------------------------------------------------------------------------------
test('users: sign-in method and 2FA columns, invitations by email or by link', async () => {
  const root = new FakeEl('div');
  await usersView(root);
  assert.match(root.text, /Sign-in/);
  assert.match(root.text, /azure/);
  assert.match(root.text, /password/);

  calls.length = 0;
  await fire(byText(root, 'BUTTON', 'Invite by email'), 'click');
  const dlg = body.children.at(-1);
  await typeInto(input(dlg, (n) => n.attrs.type === 'email'), 'new@example.com');
  await fire(byText(dlg, 'BUTTON', 'Send the invitation'), 'click');
  await wait(10);
  assert.deepEqual(calls.find((c) => c.path === '/users/invite').body, { email: 'new@example.com', role: null });
  assert.equal(globalThis.location.reloaded, true, 'delivered by email: the list is reloaded');

  // no outgoing email on the server: the administrator gets the link
  server.invite = { delivery: 'link', email: 'new@example.com', invite_url: 'https://cms.example.com/admin/#/invite?token=' + 'ab'.repeat(32) };
  await fire(byText(root, 'BUTTON', 'Invite by email'), 'click');
  const dlg2 = body.children.at(-1);
  await typeInto(input(dlg2, (n) => n.attrs.type === 'email'), 'other@example.com');
  await fire(byText(dlg2, 'BUTTON', 'Send the invitation'), 'click');
  await wait(10);
  assert.match(body.children.at(-1).text, /could not send email/);
  assert.match(body.children.at(-1).text, /#\/invite\?token=abab/);

  // an invitation needs an address
  await fire(byText(root, 'BUTTON', 'Invite by email'), 'click');
  const dlg3 = body.children.at(-1);
  calls.length = 0;
  await fire(byText(dlg3, 'BUTTON', 'Send the invitation'), 'click');
  assert.equal(calls.filter((c) => c.path === '/users/invite').length, 0);
});

test('users: an administrator can remove somebody’s second factor', async () => {
  const root = new FakeEl('div');
  await usersView(root);
  calls.length = 0;
  const row = findAll(root, (n) => n.tagName === 'TR' && n.text.includes('a@b.c'))[0];
  await fire(row, 'click');
  const dlg = body.children.at(-1);
  await fire(byText(dlg, 'BUTTON', 'Reset two-factor authentication'), 'click');
  await wait(10);
  assert.ok(calls.find((c) => c.method === 'DELETE' && c.path === '/users/u1/tfa'));
  const other = findAll(root, (n) => n.tagName === 'TR' && n.text.includes('d@e.f'))[0];
  await fire(other, 'click');
  assert.equal(byText(body.children.at(-1), 'BUTTON', 'Reset two-factor authentication'), undefined, 'offered only when it is on');
});
