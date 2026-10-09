// Account security without an identity provider: two-factor authentication (TOTP + recovery codes), password reset by
// email, invitations. Uses a fake SMTP server and a dedicated environment with outgoing email configured.
import { spawnSync } from 'node:child_process';
import http from 'node:http';

import { BIN, Env, ROOT, rnd, sleep, uuid } from './lib.mjs';
import { FakeSmtp, linkIn, sql, tokenIn, totp } from './identity.mjs';

export default async function ({ a, check }) {
  const smtp = await new FakeSmtp().start();
  const PUBLIC = 'http://127.0.0.1:18058';
  const d = new Env('d', 18058, 'ferro_e2e_d', {
    FERRO_PUBLIC_URL: PUBLIC,
    FERRO_AUTH_REDIRECT_ALLOWLIST: 'https://app.example.com',
    FERRO_SMTP_HOST: '127.0.0.1',
    FERRO_SMTP_PORT: String(smtp.port),
    FERRO_SMTP_SECURE: 'none',
    FERRO_EMAIL_FROM: 'Ferro <no-reply@example.com>',
    FERRO_LOGIN_LOCKOUT_SECS: '4',
  });
  const login = (email, password, otp) => d.rest('POST', '/auth/login', { email, password, ...(otp !== undefined ? { otp } : {}) }, null);
  const mkUser = async (email, password = 'first-password-1', role) => (await d.rest('POST', '/users', { email, password, role })).data;
  const sqlD = (s) => sql('ferro_e2e_d', s);

  try {
    await d.start();
    const role = uuid();
    await d.rest('PUT', `/roles/${role}`, { name: `plain-${rnd()}`, policies: [] });

    // ================================================================================== two-factor authentication
    const email = `ann${rnd()}@example.com`;
    const ann = await mkUser(email, 'ann-password-1', role);
    let tokens = (await login(email, 'ann-password-1')).data;
    check('baseline: password login works', !!tokens?.access_token);
    const annToken = tokens.access_token;
    const asAnn = (method, path, body) => d.rest(method, path, body, annToken);

    check('enrolment needs a signed-in user', (await d.rest('POST', '/auth/tfa/generate', {}, null)).status === 401);
    const wrongPw = await asAnn('POST', '/auth/tfa/generate', { password: 'not-my-password' });
    check('…and the current password', wrongPw.status === 403 && !wrongPw.json?.data, wrongPw.status);
    const gen = await asAnn('POST', '/auth/tfa/generate', { password: 'ann-password-1' });
    const secret = gen.data?.secret;
    check('generate returns a secret, an otpauth address and a QR code', /^[A-Z2-7]{32}$/.test(secret ?? '') && gen.data.otpauth_url.startsWith('otpauth://totp/') && gen.data.otpauth_url.includes(`secret=${secret}`) && /<svg/.test(gen.data.qr_svg), gen.status);
    check('nothing is stored before the code is confirmed', (await asAnn('GET', '/users/me')).data.tfa_enabled === false);
    const badEnable = await asAnn('POST', '/auth/tfa/enable', { secret, otp: '000000' });
    check('enable with a wrong code is refused (INVALID_OTP)', badEnable.status === 401 && badEnable.code === 'INVALID_OTP', badEnable.json);
    const weakSecret = await asAnn('POST', '/auth/tfa/enable', { secret: 'AAAA', otp: totp('AAAA') });
    check('a short or made-up secret is refused', weakSecret.status === 400 || weakSecret.status === 401, weakSecret.status);
    const en = await asAnn('POST', '/auth/tfa/enable', { secret, otp: totp(secret) });
    const recovery = en.data?.recovery_codes ?? [];
    check('enable with a right code returns 10 distinct recovery codes', en.status === 200 && recovery.length === 10 && new Set(recovery).size === 10 && recovery.every((c) => /^[a-z2-7]{5}-[a-z2-7]{5}$/.test(c)), en.json);
    check('/users/me reports tfa_enabled', (await asAnn('GET', '/users/me')).data.tfa_enabled === true);
    check('enrolling again while enabled is a conflict', (await asAnn('POST', '/auth/tfa/generate', { password: 'ann-password-1' })).status === 409);
    check('…and enable cannot overwrite the secret', (await asAnn('POST', '/auth/tfa/enable', { secret: 'JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP', otp: totp('JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP') })).status === 409);

    // the secret never leaves the server and is encrypted at rest
    const raw = sqlD(`SELECT tfa_secret, tfa_recovery::text FROM ferro_users WHERE email = '${email}'`)[0];
    check('the secret is encrypted in the database (not the base32 seed)', raw[0] && !raw[0].includes(secret) && raw[0] !== secret, raw[0]?.slice(0, 12));
    check('recovery codes are stored hashed', recovery.every((c) => !raw[1].includes(c)) && raw[1].length > 200);
    const listed = await d.rest('GET', `/items/ferro_users/${ann.id}?fields=*`);
    check('the API never exposes tfa_secret / tfa_recovery', listed.status === 200 && !('tfa_secret' in listed.data) && !('tfa_recovery' in listed.data) && listed.data.tfa_enabled === true, Object.keys(listed.data ?? {}).join());
    check('…not even when asked for explicitly', (await d.rest('GET', `/items/ferro_users/${ann.id}?fields=tfa_secret`)).status >= 400);
    check('…or used as a filter', (await d.rest('GET', `/items/ferro_users?filter=${encodeURIComponent(JSON.stringify({ tfa_secret: { _nnull: true } }))}`)).status >= 400);

    // login
    const noOtp = await login(email, 'ann-password-1');
    check('login without a code asks for it (OTP_REQUIRED)', noOtp.status === 401 && noOtp.code === 'OTP_REQUIRED', noOtp.json);
    const badPwNoOtp = await login(email, 'wrong-password');
    check('a wrong password never reveals that a second factor exists', badPwNoOtp.status === 401 && badPwNoOtp.code === 'UNAUTHORIZED', badPwNoOtp.code);
    const ghost = await login(`ghost${rnd()}@example.com`, 'whatever-1');
    check('…same answer as an unknown account', ghost.status === 401 && ghost.code === badPwNoOtp.code);
    const wrongOtp = await login(email, 'ann-password-1', '123456');
    check('a wrong code is refused (INVALID_OTP)', wrongOtp.status === 401 && wrongOtp.code === 'INVALID_OTP');
    const letters = await login(email, 'ann-password-1', 'abcdef');
    check('a malformed code is refused', letters.status === 401);
    // the enrolment consumed the current step: the next one is still inside the accepted window
    const next = totp(secret, +1);
    const ok = await login(email, 'ann-password-1', next);
    check('a code from the next step (slightly fast clock) signs in', ok.status === 200 && !!ok.data?.access_token, ok.json);
    const replay = await login(email, 'ann-password-1', next);
    check('the same code cannot be used twice (replay)', replay.status === 401 && replay.code === 'INVALID_OTP');
    const old = await login(email, 'ann-password-1', totp(secret, -1));
    check('…nor an older one once a newer step was used', old.status === 401);
    const far = await login(email, 'ann-password-1', totp(secret, +5));
    check('a code far in the future is refused', far.status === 401);

    // recovery codes
    const rc = await login(email, 'ann-password-1', recovery[0]);
    check('a recovery code signs in', rc.status === 200 && !!rc.data?.access_token, rc.json);
    const rc2 = await login(email, 'ann-password-1', recovery[0]);
    check('…once only', rc2.status === 401 && rc2.code === 'INVALID_OTP');
    const rcUpper = await login(email, 'ann-password-1', ` ${recovery[1].toUpperCase()} `);
    check('recovery codes ignore case and surrounding spaces', rcUpper.status === 200);
    check('a made-up recovery code is refused', (await login(email, 'ann-password-1', 'abcde-abcde')).status === 401);
    const afterTwo = sqlD(`SELECT jsonb_array_length(tfa_recovery) FROM ferro_users WHERE email = '${email}'`)[0][0];
    check('spent recovery codes are removed from the database', afterTwo === '8', afterTwo);
    const concurrent = await Promise.all([login(email, 'ann-password-1', recovery[2]), login(email, 'ann-password-1', recovery[2]), login(email, 'ann-password-1', recovery[2])]);
    check('the same recovery code used by three requests at once works for exactly one', concurrent.filter((r) => r.status === 200).length === 1, concurrent.map((r) => r.status));

    // disabling and administrator reset
    check('disabling needs a code', (await asAnn('POST', '/auth/tfa/disable', { password: 'ann-password-1', otp: '000000' })).status === 401);
    check('…and the password', (await asAnn('POST', '/auth/tfa/disable', { password: 'nope', otp: recovery[3] })).status === 403);
    check('a failed disable left two-factor on', (await asAnn('GET', '/users/me')).data.tfa_enabled === true);
    const dis = await asAnn('POST', '/auth/tfa/disable', { password: 'ann-password-1', otp: recovery[3] });
    check('disable with password + recovery code', dis.status === 204, dis.status);
    check('after disabling, password alone signs in', (await login(email, 'ann-password-1')).status === 200);
    check('…and the stored secret and codes are gone', sqlD(`SELECT tfa_secret IS NULL AND tfa_recovery IS NULL FROM ferro_users WHERE email = '${email}'`)[0][0] === 't');
    check('disabling when it is off is a conflict', (await asAnn('POST', '/auth/tfa/disable', { password: 'ann-password-1', otp: '123456' })).status === 409);

    const bob = `bob${rnd()}@example.com`;
    const bobUser = await mkUser(bob, 'bob-password-1', role);
    const bobTok = (await login(bob, 'bob-password-1')).data.access_token;
    const bobSecret = (await d.rest('POST', '/auth/tfa/generate', { password: 'bob-password-1' }, bobTok)).data.secret;
    await d.rest('POST', '/auth/tfa/enable', { secret: bobSecret, otp: totp(bobSecret) }, bobTok);
    check('before the reset, bob is asked for a code', (await login(bob, 'bob-password-1')).code === 'OTP_REQUIRED');
    check('only an administrator can reset someone else’s second factor', (await d.rest('DELETE', `/users/${bobUser.id}/tfa`, undefined, annToken)).status === 403);
    check('an administrator resets it (204)', (await d.rest('DELETE', `/users/${bobUser.id}/tfa`)).status === 204);
    check('…and bob signs in with the password again', (await login(bob, 'bob-password-1')).status === 200);
    check('resetting a user that does not exist is 404', (await d.rest('DELETE', '/users/0199a000-0000-7000-8000-00000000dead/tfa')).status === 404);

    // brute force on the code
    const eve = `eve${rnd()}@example.com`;
    await mkUser(eve, 'eve-password-1', role);
    const eveTok = (await login(eve, 'eve-password-1')).data.access_token;
    const eveSecret = (await d.rest('POST', '/auth/tfa/generate', { password: 'eve-password-1' }, eveTok)).data.secret;
    await d.rest('POST', '/auth/tfa/enable', { secret: eveSecret, otp: totp(eveSecret) }, eveTok);
    let last;
    for (let i = 0; i < 12; i++) last = await login(eve, 'eve-password-1', String(100000 + i));
    check('guessing the code is throttled (429) like guessing the password', last.status === 429, last.status);
    check('…even for the right code while locked', (await login(eve, 'eve-password-1', totp(eveSecret, +1))).status === 429);
    await sleep(4500);
    check('…until the lockout expires', (await login(eve, 'eve-password-1', totp(eveSecret, +1))).status === 200);
    // a lost phone with no other administrator: the command line is the way back in
    const cliReset = spawnSync(BIN, ['user', 'reset-tfa', '--email', eve], { env: d.env, encoding: 'utf8', cwd: ROOT });
    check('`ferro user reset-tfa` removes the second factor from the command line', cliReset.status === 0 && (await login(eve, 'eve-password-1')).status === 200, cliReset.stderr);
    check('…and refuses an unknown user', spawnSync(BIN, ['user', 'reset-tfa', '--email', 'nobody@example.com'], { env: d.env, encoding: 'utf8', cwd: ROOT }).status !== 0);

    // ============================================================================================ password reset
    const reqReset = (body, headers = {}) => fetch(`${d.url}/auth/password/request`, { method: 'POST', headers: { 'Content-Type': 'application/json', ...headers }, body: JSON.stringify(body) });
    const methods = (await (await fetch(`${d.url}/auth/methods`)).json()).data;
    const methodsA = (await (await fetch(`${a.url}/auth/methods`)).json()).data;
    check('/auth/methods tells the sign-in page what is available', methods.password_reset === true && methods.invitations === true && methodsA.password_reset === false);
    check('without outgoing email the request is refused clearly (400)', (await fetch(`${a.url}/auth/password/request`, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ email: 'x@example.com' }) })).status === 400);

    const carol = `carol${rnd()}@example.com`;
    await mkUser(carol, 'carol-old-password', role);
    const carolSession = (await login(carol, 'carol-old-password')).data;
    const t0 = Date.now();
    const known = await reqReset({ email: carol });
    const knownBody = await known.text();
    const unknown = await reqReset({ email: `nobody${rnd()}@example.com` });
    const unknownBody = await unknown.text();
    check('a request answers 202 with the same body for known and unknown addresses', known.status === 202 && unknown.status === 202 && knownBody === unknownBody, [known.status, unknown.status]);
    const mail1 = await smtp.wait(carol, t0);
    check('an email arrives for the known address', !!mail1 && /reset/i.test(mail1.subject), mail1?.subject);
    check('…from the configured sender', /no-reply@example\.com/.test(mail1?.from ?? ''), mail1?.from);
    const link1 = linkIn(mail1 ?? { text: '' });
    check('…with a link on the public address (admin app, reset route)', link1?.startsWith(`${PUBLIC}/admin/#/reset?token=`), link1);
    check('…and an HTML alternative with the same link', (mail1?.html ?? '').includes(link1 ?? 'x'));
    await sleep(800);
    check('nothing is sent for an unknown address', smtp.messages.filter((m) => m.at >= t0).length === 1, smtp.messages.length);
    const token1 = tokenIn(mail1);
    check('the token is stored hashed', token1 && sqlD(`SELECT count(*) FROM ferro_auth_tokens WHERE token_hash = '${token1}'`)[0][0] === '0' && sqlD("SELECT count(*) FROM ferro_auth_tokens WHERE kind = 'reset'")[0][0] !== '0');

    // a Host header must not steer the link (password reset poisoning)
    const t1 = Date.now();
    await new Promise((resolve, reject) => {
      const req = http.request({ host: '127.0.0.1', port: 18058, path: '/auth/password/request', method: 'POST', headers: { Host: 'evil.example.net', 'X-Forwarded-Host': 'evil.example.net', 'Content-Type': 'application/json' } }, (res) => { res.resume(); res.on('end', resolve); });
      req.on('error', reject);
      req.end(JSON.stringify({ email: carol }));
    });
    const mail2 = await smtp.wait(carol, t1);
    check('a spoofed Host / X-Forwarded-Host header does not change the link', !!mail2 && linkIn(mail2)?.startsWith(PUBLIC) && !mail2.text.includes('evil.example.net'), linkIn(mail2 ?? { text: '' }));
    const token2 = tokenIn(mail2 ?? { text: '' });
    const stale = await d.rest('POST', '/auth/password/reset', { token: token1, password: 'carol-new-password' }, null);
    check('requesting again invalidates the previous link', stale.status === 401, stale.status);

    const evil = await reqReset({ email: carol, reset_url: 'https://evil.example.net/reset' });
    check('a reset_url on another origin is refused (400)', evil.status === 400);
    const sneaky = await reqReset({ email: carol, reset_url: 'https://app.example.com.evil.net/reset' });
    check('…also when it merely starts like an allowed one', sneaky.status === 400);
    const t2 = Date.now();
    const good = await reqReset({ email: carol, reset_url: 'https://app.example.com/new-password' });
    const mail3 = await smtp.wait(carol, t2);
    check('an allow-listed reset_url is used for the link', good.status === 202 && linkIn(mail3 ?? { text: '' })?.startsWith('https://app.example.com/new-password?token='), linkIn(mail3 ?? { text: '' }));
    const token3 = tokenIn(mail3 ?? { text: '' });

    check('a weak new password is refused and the link survives', (await d.rest('POST', '/auth/password/reset', { token: token3, password: 'short' }, null)).status === 400);
    check('a made-up token is refused', (await d.rest('POST', '/auth/password/reset', { token: 'f'.repeat(64), password: 'carol-new-password' }, null)).status === 401);
    const results = await Promise.all([1, 2, 3].map(() => d.rest('POST', '/auth/password/reset', { token: token3, password: 'carol-new-password' }, null)));
    check('three simultaneous uses of one link: exactly one succeeds', results.filter((r) => r.status === 204).length === 1 && results.filter((r) => r.status === 401).length === 2, results.map((r) => r.status));
    check('the new password works', (await login(carol, 'carol-new-password')).status === 200);
    check('the old password no longer does', (await login(carol, 'carol-old-password')).status === 401);
    check('the link cannot be used again', (await d.rest('POST', '/auth/password/reset', { token: token3, password: 'carol-third-password' }, null)).status === 401);
    const refreshed = await d.rest('POST', '/auth/refresh', { refresh_token: carolSession.refresh_token }, null);
    check('a reset ends every existing session', refreshed.status === 401, refreshed.status);

    // expiry, status and provider
    const dave = `dave${rnd()}@example.com`;
    const daveUser = await mkUser(dave, 'dave-old-password', role);
    const t3 = Date.now();
    await reqReset({ email: dave });
    const daveTok = tokenIn((await smtp.wait(dave, t3)) ?? { text: '' });
    sqlD(`UPDATE ferro_auth_tokens SET expires = now() - interval '1 minute' WHERE user_id = '${daveUser.id}'`);
    check('an expired link is refused', (await d.rest('POST', '/auth/password/reset', { token: daveTok, password: 'dave-new-password' }, null)).status === 401);
    await d.rest('PATCH', `/users/${daveUser.id}`, { status: 'suspended' });
    const t4 = Date.now();
    await reqReset({ email: dave });
    await sleep(900);
    check('a suspended account gets no reset email', smtp.to(dave, t4).length === 0);
    const sso = `sso${rnd()}@example.com`;
    const ssoUser = await mkUser(sso, 'sso-old-password', role);
    sqlD(`UPDATE ferro_users SET provider = 'azure', external_identifier = 'x-${rnd()}' WHERE id = '${ssoUser.id}'`);
    const t5 = Date.now();
    await reqReset({ email: sso });
    await sleep(900);
    check('…and neither does an account that signs in through an identity provider', smtp.to(sso, t5).length === 0);
    const flood = `flood${rnd()}@example.com`;
    let tooMany = 0;
    for (let i = 0; i < 8; i++) if ((await reqReset({ email: flood })).status === 429) tooMany++;
    check('flooding one address is throttled (429 after 5 requests)', tooMany === 3, tooMany);

    // ================================================================================================ invitations
    const invite = (body, token) => d.rest('POST', '/users/invite', body, token);
    const guest = `guest${rnd()}@example.com`;
    check('only administrators can invite', (await invite({ email: guest }, annToken)).status === 403);
    check('an invalid address is refused', (await invite({ email: 'not-an-email' })).status === 400);
    check('an unknown role is refused', (await invite({ email: guest, role: '0199a000-0000-7000-8000-00000000dead' })).status === 400);
    check('an invite_url on another origin is refused', (await invite({ email: guest, invite_url: 'https://evil.example.net/' })).status === 400);
    const t6 = Date.now();
    const inv = await invite({ email: guest, role });
    check('inviting creates an invited user and reports email delivery', inv.status === 200 && inv.data.status === 'invited' && inv.data.delivery === 'email' && !inv.data.invite_url, inv.json);
    const invMail = await smtp.wait(guest, t6);
    const invLink = linkIn(invMail ?? { text: '' });
    check('the invitation email carries a link to the admin app', /invited/i.test(invMail?.subject ?? '') && invLink?.startsWith(`${PUBLIC}/admin/#/invite?token=`), invLink);
    const status = (await d.rest('GET', `/items/ferro_users/${inv.data.id}?fields=status,role`)).data;
    check('the user is stored as invited with the chosen role, without a password', status.status === 'invited' && status.role === role && sqlD(`SELECT password_hash IS NULL FROM ferro_users WHERE id = '${inv.data.id}'`)[0][0] === 't');
    check('an invited user cannot sign in yet', (await login(guest, 'anything-at-all-1')).status === 401);
    check('inviting an address that is already registered is a conflict', (await invite({ email: email })).status === 409);
    const t7 = Date.now();
    const again = await invite({ email: guest });
    const invMail2 = await smtp.wait(guest, t7);
    const invTok1 = tokenIn(invMail), invTok2 = tokenIn(invMail2 ?? { text: '' });
    check('inviting a pending address again renews the invitation (old link dies)', again.status === 200 && again.data.created === false && !!invTok2 && invTok2 !== invTok1 && (await d.rest('POST', '/auth/invite/accept', { token: invTok1, password: 'guest-password-1' }, null)).status === 401);
    check('a weak password is refused and the invitation survives', (await d.rest('POST', '/auth/invite/accept', { token: invTok2, password: '1234' }, null)).status === 400);
    const accepted = await d.rest('POST', '/auth/invite/accept', { token: invTok2, password: 'guest-password-1' }, null);
    check('accepting sets the password, activates the user and signs them in', accepted.status === 200 && !!accepted.data?.access_token, accepted.json);
    const me = (await d.rest('GET', '/users/me', undefined, accepted.data.access_token)).data;
    check('…with the role they were invited with', me.email === guest && me.role === role && me.status === 'active', me);
    check('the invitation cannot be used twice', (await d.rest('POST', '/auth/invite/accept', { token: invTok2, password: 'guest-password-2' }, null)).status === 401);
    check('the new password works for a normal login', (await login(guest, 'guest-password-1')).status === 200);
    // tokens of one kind are useless for the other, even when both are valid
    const t9 = Date.now();
    await reqReset({ email: guest });
    const resetTok = tokenIn((await smtp.wait(guest, t9)) ?? { text: '' });
    const other = `second${rnd()}@example.com`;
    const t10 = Date.now();
    await invite({ email: other });
    const inviteTok = tokenIn((await smtp.wait(other, t10)) ?? { text: '' });
    check('a valid password-reset token is not accepted as an invitation', !!resetTok && (await d.rest('POST', '/auth/invite/accept', { token: resetTok, password: 'guest-password-3' }, null)).status === 401);
    check('a valid invitation token is not accepted as a password reset', !!inviteTok && (await d.rest('POST', '/auth/password/reset', { token: inviteTok, password: 'guest-password-3' }, null)).status === 401);
    check('…and each still works for its own purpose', (await d.rest('POST', '/auth/password/reset', { token: resetTok, password: 'guest-password-3' }, null)).status === 204 && (await d.rest('POST', '/auth/invite/accept', { token: inviteTok, password: 'second-password-1' }, null)).status === 200);

    // no email available: the link goes to the administrator
    await d.restart({ FERRO_SMTP_PORT: '1' });
    const t8 = Date.now();
    const offline = `offline${rnd()}@example.com`;
    const fb = await invite({ email: offline });
    check('if sending fails the invitation link is returned to the administrator', fb.status === 200 && fb.data.delivery === 'link' && fb.data.invite_url?.startsWith(`${PUBLIC}/admin/#/invite?token=`), fb.json);
    const fbAccept = await d.rest('POST', '/auth/invite/accept', { token: /token=([0-9a-f]{64})/.exec(fb.data.invite_url)[1], password: 'offline-password-1' }, null);
    check('…and that link works', fbAccept.status === 200);
    check('no email was sent', smtp.to(offline, t8).length === 0);

    // ====================================================================================== password policy everywhere
    const patched = await d.rest('PATCH', `/users/${ann.id}`, { password: 'short' });
    check('the password policy also applies to administrator changes', patched.status === 400);
    const huge = await d.rest('POST', '/users', { email: `huge${rnd()}@example.com`, password: 'x'.repeat(5000) });
    check('absurdly long passwords are refused (hashing cost)', huge.status === 400, huge.status);
  } finally {
    await d.stop();
    smtp.stop();
  }
}
