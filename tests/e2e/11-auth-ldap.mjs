// Directory (LDAP) login against a real LDAP server: the search-then-bind exchange, filter injection, anonymous-bind
// pitfalls, account binding, second factor, outages, TLS.
import { Env, rnd, sleep, uuid } from './lib.mjs';
import { sql, totp } from './identity.mjs';
import { BASE, MockLdap, SERVICE_DN, SERVICE_PASSWORD, selfSignedCert } from './mock-ldap.mjs';

export default async function ({ check }) {
  const dir = await new MockLdap(18389).start();
  const ROLE = uuid();
  const env = (extra = {}) => ({
    FERRO_LDAP_URL: 'ldap://127.0.0.1:18389',
    FERRO_LDAP_BIND_DN: SERVICE_DN,
    FERRO_LDAP_BIND_PASSWORD: SERVICE_PASSWORD,
    FERRO_LDAP_USER_BASE_DN: BASE,
    FERRO_LOGIN_LOCKOUT_SECS: '3',
    ...extra,
  });
  const F = new Env('f', 18060, 'ferro_e2e_f', env());
  const q = (s) => sql('ferro_e2e_f', s);
  const login = (who, password, otp) => F.rest('POST', '/auth/login', { email: who, password, ...(otp !== undefined ? { otp } : {}) }, null);
  const count = (email) => Number(q(`SELECT count(*) FROM ferro_users WHERE lower(email) = lower('${email}')`)[0][0]);
  const provision = (email, extra = '') => q(`INSERT INTO ferro_users (id, email, provider, status, role) VALUES (gen_random_uuid(), '${email}', 'ldap', 'active', '${ROLE}') ${extra}`);
  const cert = selfSignedCert();
  let tls = null;

  try {
    await F.start();
    await F.rest('PUT', `/roles/${ROLE}`, { name: 'directory-default', policies: [] });
    check('/auth/methods announces directory login', (await (await fetch(`${F.url}/auth/methods`)).json()).data.directory === true);

    // ================================================================================================ registration closed
    dir.log.length = 0;
    const stranger = await login('alice', 'alice-secret-1');
    check('a directory user with no Ferro account cannot sign in while registration is closed', stranger.status === 401 && count('alice@example.com') === 0, stranger.status);
    check('…the directory did verify the password (service search, then a bind as the person)', dir.log.some((l) => l.type === 'search' && l.bindDn.toLowerCase() === SERVICE_DN.toLowerCase()) && dir.userBinds().length === 1 && dir.userBinds()[0].dn === `uid=alice,${BASE}`, dir.log);

    provision('alice@example.com');
    const ok = await login('alice', 'alice-secret-1');
    check('once the account exists (created for the directory) signing in with the uid works', ok.status === 200 && !!ok.data?.access_token, ok.json);
    const me = (await F.rest('GET', '/users/me', undefined, ok.data.access_token)).data;
    check('…bound by the directory’s stable id (entryUUID), role kept', me.provider === 'ldap' && me.role === ROLE && q("SELECT external_identifier FROM ferro_users WHERE email = 'alice@example.com'")[0][0] === '11111111-1111-1111-1111-111111111111', me);
    check('the email works as the login too', (await login('alice@example.com', 'alice-secret-1')).status === 200);
    check('…and so does a different capitalisation of it', (await login('Alice@Example.com', 'alice-secret-1')).status === 200);
    check('no password hash is stored for a directory user', q("SELECT password_hash IS NULL FROM ferro_users WHERE email = 'alice@example.com'")[0][0] === 't');

    // ====================================================================================================== wrong input
    const wrong = await login('alice', 'wrong-password-1');
    const unknown = await login(`nobody${rnd()}`, 'wrong-password-1');
    check('wrong password and unknown login answer the same 401', wrong.status === 401 && unknown.status === 401 && wrong.text === unknown.text, [wrong.text, unknown.text]);
    dir.log.length = 0;
    const empty = await login('alice', '');
    check('an empty password is refused without asking the directory (an empty bind would succeed as anonymous)', empty.status === 401 && dir.userBinds().length === 0 && dir.log.length === 0, dir.log);
    check('…also one containing a NUL byte', (await login('alice', 'alice-secret-1\u0000')).status === 401 && dir.userBinds().length === 0);
    check('an absurdly long login is refused', (await login('a'.repeat(400), 'alice-secret-1')).status === 401);
    check('a login of only spaces is refused', (await login('   ', 'alice-secret-1')).status === 401);

    // ===================================================================================================== LDAP injection
    dir.log.length = 0;
    for (const evil of ['*', 'a*', 'alice)(uid=*', '*)(uid=*))(|(uid=*', 'alice)(|(uid=*', '\\2a', 'alice\u0000']) {
      const r = await login(evil, 'alice-secret-1');
      check(`filter injection ${JSON.stringify(evil)} is just a login nobody has (401)`, r.status === 401, r.status);
    }
    const filters = dir.log.filter((l) => l.type === 'search').map((l) => l.filter);
    check('…the directory received the characters escaped, never as wildcards or extra clauses', filters.length > 0 && filters.every((f) => (f.match(/\(/g) ?? []).length === 5), filters.slice(0, 3));
    check('…and nobody was bound as a result', dir.userBinds().length === 0, dir.userBinds());

    // ambiguous and incomplete entries
    provision('dup@example.com');
    check('a login that matches two directory entries names nobody (401)', (await login('dup@example.com', 'dup1-secret-1')).status === 401 && (await login('dup@example.com', 'dup2-secret-1')).status === 401);
    check('…but their uids are unambiguous', (await login('dup1', 'dup1-secret-1')).status !== 500);
    check('an entry without a mail attribute cannot become a user (401)', (await login('nomail', 'nomail-secret-1')).status === 401 && q("SELECT count(*) FROM ferro_users WHERE email LIKE 'nomail%'")[0][0] === '0');

    // ====================================================================================== automatic accounts and rules
    await F.restart(env({ FERRO_LDAP_ALLOW_PUBLIC_REGISTRATION: 'true', FERRO_LDAP_DEFAULT_ROLE: ROLE }));
    const carol = await login('carol', 'päss wörd ✓ 1');
    const carolMe = carol.data ? (await F.rest('GET', '/users/me', undefined, carol.data.access_token)).data : null;
    check('with registration open the account is created at the first sign-in (unicode password included)', carol.status === 200 && carolMe?.email === 'carol@example.com' && carolMe.first_name === 'Carol' && carolMe.last_name === 'Ünïcode' && carolMe.role === ROLE, carol.json);
    const twins = await Promise.all(Array.from({ length: 8 }, () => login('dave', 'dave-secret-1')));
    check('eight simultaneous first sign-ins create one account', twins.every((t) => t.status === 200) && count('dave@example.com') === 1, twins.map((t) => t.status));

    // a local account is never taken over, and the directory is not a fallback for it
    const local = (await F.rest('POST', '/users', { email: 'bob@example.com', password: 'bob-local-pass-1', role: ROLE })).data;
    check('a Ferro account with a password signs in with that password', (await login('bob@example.com', 'bob-local-pass-1')).status === 200);
    check('…and the directory password does not work for it (no fallthrough)', (await login('bob@example.com', 'bob-secret-1')).status === 401);
    check('…nor does signing in by uid let the directory take the account over', (await login('bob', 'bob-secret-1')).status === 401 && q(`SELECT provider IS NULL FROM ferro_users WHERE id = '${local.id}'`)[0][0] === 't');
    await F.restart(env({ FERRO_LDAP_LINK_EXISTING_USERS: 'true' }));
    check('with LINK_EXISTING_USERS the directory can claim it', (await login('bob', 'bob-secret-1')).status === 200 && q(`SELECT provider FROM ferro_users WHERE id = '${local.id}'`)[0][0] === 'ldap');
    check('…after which the old password is dead', (await login('bob@example.com', 'bob-local-pass-1')).status === 401);

    // suspended
    const aliceId = q("SELECT id FROM ferro_users WHERE email = 'alice@example.com'")[0][0];
    await F.rest('PATCH', `/users/${aliceId}`, { status: 'suspended' });
    check('a suspended user cannot sign in even with the right directory password', (await login('alice', 'alice-secret-1')).status === 401);
    await F.rest('PATCH', `/users/${aliceId}`, { status: 'active' });
    check('…and can again once reactivated', (await login('alice', 'alice-secret-1')).status === 200);

    // stable identity
    dir.entries.find((e) => e.attributes.uid === 'alice').attributes.mail = 'alice.new@example.com';
    const moved = await login('alice', 'alice-secret-1');
    check('if the email changes in the directory the person is still the same Ferro user', moved.status === 200 && count('alice.new@example.com') === 0 && q(`SELECT email FROM ferro_users WHERE id = '${aliceId}'`)[0][0] === 'alice@example.com');
    const entry = dir.entries.find((e) => e.attributes.uid === 'alice');
    entry.dn = `uid=alice,${BASE}`;
    entry.attributes.mail = 'alice@example.com';

    // ================================================================================================= second factor
    const aliceTok = (await login('alice', 'alice-secret-1')).data.access_token;
    const gen = await F.rest('POST', '/auth/tfa/generate', {}, aliceTok);
    check('a directory user (no Ferro password) can enrol a second factor with their session alone', gen.status === 200 && !!gen.data?.secret, gen.json);
    await F.rest('POST', '/auth/tfa/enable', { secret: gen.data.secret, otp: totp(gen.data.secret) }, aliceTok);
    const noOtp = await login('alice', 'alice-secret-1');
    check('once enrolled, the directory password alone is not enough (OTP_REQUIRED)', noOtp.status === 401 && noOtp.code === 'OTP_REQUIRED', noOtp.json);
    check('…a wrong code is refused', (await login('alice', 'alice-secret-1', '000000')).code === 'INVALID_OTP');
    check('…a wrong directory password never reaches the code check', (await login('alice', 'nope-nope-1', totp(gen.data.secret, +1))).code === 'UNAUTHORIZED');
    check('…the right password and code sign in', (await login('alice', 'alice-secret-1', totp(gen.data.secret, +1))).status === 200);

    // lockout
    let last;
    for (let i = 0; i < 12; i++) last = await login('carol', `wrong-${i}-password`);
    check('repeated wrong directory passwords lock the login (429), like local ones', last.status === 429, last.status);
    check('…even for the right password', (await login('carol', 'päss wörd ✓ 1')).status === 429);
    await sleep(3500);
    check('…until the lockout expires', (await login('carol', 'päss wörd ✓ 1')).status === 200);

    // ===================================================================================================== outages
    await F.restart(env({ FERRO_LDAP_BIND_PASSWORD: 'wrong-service-password' }));
    const bad = await login('carol', 'päss wörd ✓ 1');
    check('a refused service account is a 503 (not the user’s fault), not a 401', bad.status === 503 && bad.code === 'DIRECTORY_UNAVAILABLE', bad.json);
    dir.stop();
    await F.restart(env());
    const down = await login('carol', 'päss wörd ✓ 1');
    check('an unreachable directory is a 503 as well', down.status === 503 && down.code === 'DIRECTORY_UNAVAILABLE', down.json);
    check('…the server stays healthy', (await fetch(`${F.url}/server/health`)).ok);
    const localAdmin = await F.rest('GET', '/users/me');
    check('…and token-based access is unaffected', localAdmin.status === 200);
    check('a failed directory attempt is not counted as a wrong password', (await (async () => { for (let i = 0; i < 12; i++) await login('carol', 'päss wörd ✓ 1'); return login('carol', 'päss wörd ✓ 1'); })()).status === 503);

    // ========================================================================================================== TLS
    if (cert) {
      tls = await new MockLdap(18636, { tls: cert }).start();
      await F.restart(env({ FERRO_LDAP_URL: 'ldaps://127.0.0.1:18636' }));
      const strict = await login('carol', 'päss wörd ✓ 1');
      check('ldaps:// with a certificate that is not trusted is refused by default (503)', strict.status === 503, strict.status);
      await F.restart(env({ FERRO_LDAP_URL: 'ldaps://127.0.0.1:18636', FERRO_LDAP_TLS_VERIFY: 'false' }));
      const lax = await login('carol', 'päss wörd ✓ 1');
      check('…and works when verification is switched off for a test server', lax.status === 200, lax.json);
      check('…over the encrypted connection', tls.log.some((l) => l.type === 'search'));
    } else {
      console.log('SKIP  openssl not available: ldaps checks not run');
    }
  } finally {
    await F.stop();
    dir.stop();
    tls?.stop();
  }
}
