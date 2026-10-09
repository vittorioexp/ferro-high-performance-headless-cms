// Single sign-on against a mock OpenID Connect / OAuth 2.0 provider: the happy paths, account-binding rules, and every
// way a callback or an ID token can be forged, replayed or broken.
import { Env, rnd, sleep, uuid } from './lib.mjs';
import { MockIdp } from './mock-idp.mjs';
import { sql } from './identity.mjs';

export default async function ({ check }) {
  const idp = await new MockIdp().start();
  idp.clients = { 'cid-mock': 's-mock', 'cid-closed': 's-closed', 'cid-linker': 's-linker', 'cid-loose': 's-loose', 'cid-plain': 's-plain', 'cid-liar': 's-liar' };
  const ROLE = '0199a000-0000-7000-8000-0000000050c0';
  const PUBLIC = 'http://127.0.0.1:18059';
  const provider = (name, extra = {}) =>
    Object.fromEntries(Object.entries({ DRIVER: 'openid', CLIENT_ID: `cid-${name}`, CLIENT_SECRET: `s-${name}`, ISSUER_URL: idp.issuer, ...extra }).map(([k, v]) => [`FERRO_AUTH_${name.toUpperCase()}_${k}`, v]));
  const E = new Env('e', 18059, 'ferro_e2e_e', {
    FERRO_PUBLIC_URL: PUBLIC,
    FERRO_AUTH_REDIRECT_ALLOWLIST: 'https://app.example.com',
    FERRO_AUTH_PROVIDERS: 'mock,closed,linker,loose,plain,down,liar',
    ...provider('mock', { LABEL: 'Sign in with Mock', ALLOW_PUBLIC_REGISTRATION: 'true', DEFAULT_ROLE: ROLE, ALLOWED_EMAIL_DOMAINS: 'example.com' }),
    ...provider('closed'),
    ...provider('linker', { LINK_EXISTING_USERS: 'true' }),
    ...provider('loose', { REQUIRE_VERIFIED_EMAIL: 'false', ALLOW_PUBLIC_REGISTRATION: 'true', DEFAULT_ROLE: ROLE }),
    ...provider('plain', { DRIVER: 'oauth2', ISSUER_URL: undefined, AUTHORIZE_URL: `${idp.issuer}/authorize`, TOKEN_URL: `${idp.issuer}/token`, PROFILE_URL: `${idp.issuer}/profile`, IDENTIFIER_KEY: 'id', FIRST_NAME_KEY: 'name', ALLOW_PUBLIC_REGISTRATION: 'true', DEFAULT_ROLE: ROLE }),
    ...provider('down', { CLIENT_ID: 'x', CLIENT_SECRET: 'x', ISSUER_URL: 'http://127.0.0.1:1' }),
    ...provider('liar'),
  });
  for (const k of Object.keys(E.extraEnv)) if (E.extraEnv[k] === undefined) delete E.extraEnv[k];
  const q = (s) => sql('ferro_e2e_e', s);
  const queryOf = (loc) => new URLSearchParams((loc ?? '').split('?').slice(1).join('?'));
  const ident = (email, extra = {}) => ({ sub: `sub-${rnd()}${rnd()}`, email, email_verified: true, given_name: 'Ann', family_name: 'Lee', ...extra });
  const users = (email) => Number(q(`SELECT count(*) FROM ferro_users WHERE lower(email) = lower('${email}')`)[0][0]);

  /** The whole browser dance, step by step. `back` lets a test alter the callback URL; `cookie` replaces the cookie. */
  async function flow(name, { identity, redirect, back, cookie: forced, skipProvider = false } = {}) {
    idp.identity = identity ?? null;
    const r1 = await fetch(`${E.url}/auth/oauth/${name}${redirect ? `?redirect=${encodeURIComponent(redirect)}` : ''}`, { redirect: 'manual' });
    const out = { r1, status: r1.status };
    if (r1.status !== 302) return out;
    const setCookie = r1.headers.get('set-cookie') ?? '';
    out.setCookie = setCookie;
    out.cookie = forced !== undefined ? forced : /ferro_oauth=[^;]*/.exec(setCookie)?.[0];
    out.authUrl = r1.headers.get('location');
    const r2 = await fetch(out.authUrl, { redirect: 'manual' });
    let cb = r2.headers.get('location');
    if (back) cb = back(cb);
    out.callback = cb;
    const r3 = await fetch(cb, { redirect: 'manual', headers: out.cookie ? { Cookie: out.cookie } : {} });
    out.r3 = r3;
    out.final = r3.headers.get('location');
    out.params = queryOf(out.final);
    out.clearedCookie = r3.headers.get('set-cookie') ?? '';
    return out;
  }
  const exchange = (code) => E.rest('POST', '/auth/oauth/exchange', { code }, null);
  const ok = async (name, identity, opts = {}) => {
    const f = await flow(name, { identity, ...opts });
    const t = f.params?.get('code') ? await exchange(f.params.get('code')) : null;
    return { f, tokens: t?.data, me: t?.data ? (await E.rest('GET', '/users/me', undefined, t.data.access_token)).data : null };
  };

  try {
    await E.start();
    await E.rest('PUT', `/roles/${ROLE}`, { name: 'sso-default', policies: [] });

    // ================================================================================== discovery of what is available
    const methods = (await (await fetch(`${E.url}/auth/methods`)).json()).data;
    check('/auth/methods lists the providers with their labels', methods.providers.map((p) => p.name).join() === 'closed,down,linker,liar,loose,mock,plain'.split(',').sort().join() && methods.providers.find((p) => p.name === 'mock').label === 'Sign in with Mock', methods.providers.map((p) => p.name));
    check('…and never their secrets', !JSON.stringify(methods).includes('s-mock') && !JSON.stringify(methods).includes('cid-'));
    check('an unknown provider is a 404', (await fetch(`${E.url}/auth/oauth/nope`, { redirect: 'manual' })).status === 404);
    for (const bad of ['https://evil.example.net/', '//evil.example.net', 'javascript:alert(1)', 'https://127.0.0.1.evil.net/', `${PUBLIC}.evil.net/x`, 'https://app.example.com@evil.net/']) {
      const r = await fetch(`${E.url}/auth/oauth/mock?redirect=${encodeURIComponent(bad)}`, { redirect: 'manual' });
      check(`open redirect refused (400): ${bad}`, r.status === 400, r.status);
    }

    // ============================================================================================ the request to the provider
    idp.identity = null;
    const start = await fetch(`${E.url}/auth/oauth/mock`, { redirect: 'manual' });
    const auth = new URL(start.headers.get('location'));
    const sc = start.headers.get('set-cookie') ?? '';
    check('the browser is redirected (302) to the provider’s authorize endpoint', start.status === 302 && auth.origin === idp.issuer && auth.pathname === '/authorize', auth.toString().slice(0, 80));
    const p = auth.searchParams;
    check('…as an authorization-code request with scope, state and nonce', p.get('response_type') === 'code' && p.get('client_id') === 'cid-mock' && p.get('scope') === 'openid profile email' && p.get('state')?.length === 64 && p.get('nonce')?.length === 64, Object.fromEntries(p));
    check('…whose redirect_uri is built from FERRO_PUBLIC_URL', p.get('redirect_uri') === `${PUBLIC}/auth/oauth/mock/callback`, p.get('redirect_uri'));
    check('…with PKCE (S256 challenge of 43 characters)', p.get('code_challenge_method') === 'S256' && p.get('code_challenge')?.length === 43);
    check('the state cookie is HttpOnly, SameSite=Lax, scoped to /auth/oauth', /ferro_oauth=/.test(sc) && /HttpOnly/i.test(sc) && /SameSite=Lax/i.test(sc) && /Path=\/auth\/oauth/.test(sc), sc.slice(0, 120));
    check('…and not Secure here (the public address is plain http)', !/;\s*Secure/i.test(sc));
    check('the verifier never appears in the redirect URL', !auth.toString().includes('verifier'));
    const second = await fetch(`${E.url}/auth/oauth/mock`, { redirect: 'manual' });
    check('every start uses fresh state, nonce and challenge', new URL(second.headers.get('location')).searchParams.get('state') !== p.get('state') && new URL(second.headers.get('location')).searchParams.get('code_challenge') !== p.get('code_challenge'));

    // ================================================================================================= first sign-in
    const email = `ann${rnd()}@example.com`;
    const who = ident(email);
    idp.log.length = 0;
    const first = await ok('mock', who);
    check('the callback sends the browser back to the admin app with a one-time code (never a token)', first.f.r3.status === 302 && new RegExp(`^${PUBLIC}/admin/#/sso\\?code=[0-9a-f]{64}$`).test(first.f.final) && !/access_token|refresh_token/.test(first.f.final), first.f.final);
    check('the state cookie is cleared by the callback', /ferro_oauth=;?/.test(first.f.clearedCookie) && /Max-Age=0/i.test(first.f.clearedCookie), first.f.clearedCookie);
    check('the code is exchanged for tokens', !!first.tokens?.access_token && !!first.tokens?.refresh_token);
    check('the account was created from the provider’s profile, with the default role', first.me?.email === email && first.me.first_name === 'Ann' && first.me.last_name === 'Lee' && first.me.role === ROLE && first.me.status === 'active', first.me);
    check('…bound to the provider (provider name; no password)', first.me?.provider === 'mock' && q(`SELECT password_hash IS NULL AND external_identifier = '${who.sub}' FROM ferro_users WHERE email = '${email}'`)[0][0] === 't');
    const tokenCall = idp.log.find((l) => l.type === 'token')?.params ?? {};
    check('the token request carried the PKCE verifier, the secret and the exact redirect_uri', (tokenCall.code_verifier ?? '').length >= 43 && tokenCall.client_secret === 's-mock' && tokenCall.redirect_uri === `${PUBLIC}/auth/oauth/mock/callback` && tokenCall.grant_type === 'authorization_code', tokenCall);
    check('the one-time code works exactly once', (await exchange(first.f.params.get('code'))).status === 401);
    check('a made-up code is refused', (await exchange('f'.repeat(64))).status === 401);
    check('the refresh token from SSO works', (await E.rest('POST', '/auth/refresh', { refresh_token: first.tokens.refresh_token }, null)).status === 200);
    const stored = q("SELECT token_hash FROM ferro_auth_tokens WHERE kind = 'sso' LIMIT 1")[0]?.[0];
    check('one-time codes are stored hashed and consumed', stored === undefined || !first.f.params.get('code').includes(stored));

    // returning user
    const again = await ok('mock', who);
    check('signing in again finds the same account (no duplicate)', again.me?.id === first.me.id && users(email) === 1);
    const moved = await ok('mock', { ...who, email: `renamed${rnd()}@example.com` });
    check('the provider’s identifier, not the email, identifies the person (an email change at the provider does not create a second account)', moved.me?.id === first.me.id && moved.me.email === email, moved.me?.email);
    check('…and the Ferro email is not silently overwritten', users(email) === 1);

    // concurrency: the same new person arriving through ten tabs at once
    const twin = ident(`twin${rnd()}@example.com`);
    const runs = await Promise.all(Array.from({ length: 10 }, () => ok('mock', twin)));
    check('ten simultaneous first sign-ins of one person create one account and all succeed', new Set(runs.map((r) => r.me?.id)).size === 1 && runs.every((r) => r.tokens) && users(twin.email) === 1, runs.map((r) => r.f.final?.slice(-20)));

    // ==================================================================================================== provider rules
    const outsider = ident(`mallory${rnd()}@evil.net`);
    const dom = await flow('mock', { identity: outsider });
    check('an email outside ALLOWED_EMAIL_DOMAINS is refused (error=domain) and nothing is created', dom.params.get('error') === 'domain' && !dom.params.get('code') && users(outsider.email) === 0, dom.final);
    const tricky = await flow('mock', { identity: ident(`x${rnd()}@example.com.evil.net`) });
    check('…a lookalike domain too', tricky.params.get('error') === 'domain');
    const unverified = ident(`unv${rnd()}@example.com`, { email_verified: false });
    const unv = await flow('mock', { identity: unverified });
    check('an email the provider does not vouch for is refused (error=email)', unv.params.get('error') === 'email' && users(unverified.email) === 0, unv.final);
    const missing = await flow('mock', { identity: ident(`miss${rnd()}@example.com`, { email_verified: 'absent' }) });
    check('…also when the claim is absent', missing.params.get('error') === 'email');
    const loose = ident(`loose${rnd()}@example.com`, { email_verified: false });
    const looseRun = await ok('loose', loose);
    check('a provider configured with REQUIRE_VERIFIED_EMAIL=false accepts it', looseRun.me?.email === loose.email);
    const closedNew = ident(`new${rnd()}@example.com`);
    const closedRun = await flow('closed', { identity: closedNew });
    check('registration closed: a stranger gets error=denied and no account', closedRun.params.get('error') === 'denied' && users(closedNew.email) === 0, closedRun.final);

    // =================================================================================================== account binding
    const localEmail = `local${rnd()}@example.com`;
    const local = (await E.rest('POST', '/users', { email: localEmail, password: 'local-password-1', role: ROLE })).data;
    const takeover = await flow('closed', { identity: ident(localEmail) });
    check('a provider cannot take over an existing password account by email (error=denied)', takeover.params.get('error') === 'denied', takeover.final);
    check('…which stays untouched', q(`SELECT provider IS NULL AND external_identifier IS NULL FROM ferro_users WHERE id = '${local.id}'`)[0][0] === 't' && (await E.rest('POST', '/auth/login', { email: localEmail, password: 'local-password-1' }, null)).status === 200);
    const link = await ok('linker', ident(localEmail.toUpperCase()));
    check('with LINK_EXISTING_USERS the account is bound to the provider (email compared case-insensitively)', link.me?.id === local.id && link.me.provider === 'linker', link.me);

    const preEmail = `pre${rnd()}@example.com`;
    q(`INSERT INTO ferro_users (id, email, provider, status, role) VALUES (gen_random_uuid(), '${preEmail}', 'closed', 'active', '${ROLE}')`);
    const pre = ident(preEmail);
    const preRun = await ok('closed', pre);
    check('an account pre-created for this provider is bound at the first sign-in, even though registration is closed', preRun.me?.email === preEmail && q(`SELECT external_identifier FROM ferro_users WHERE email = '${preEmail}'`)[0][0] === pre.sub, preRun.f.final);
    const other = await flow('closed', { identity: { ...ident(preEmail), sub: 'someone-else' } });
    check('…and afterwards another person with the same email is refused', other.params.get('error') === 'denied', other.final);

    const invEmail = `inv${rnd()}@example.com`;
    q(`INSERT INTO ferro_users (id, email, status, role) VALUES (gen_random_uuid(), '${invEmail}', 'invited', '${ROLE}')`);
    const inv = await ok('closed', ident(invEmail));
    check('an invited user can accept the invitation by signing in with the provider', inv.me?.status === 'active' && inv.me.provider === 'closed', inv.me);

    const bound = ident(`bound${rnd()}@example.com`);
    await ok('mock', bound);
    const hijack = await flow('closed', { identity: { ...bound, sub: 'different-sub' } });
    check('an account bound to one provider cannot be reached through another with the same email', hijack.params.get('error') === 'denied', hijack.final);
    const sameSub = await ok('loose', { ...ident(`samesub${rnd()}@example.com`), sub: bound.sub });
    check('identifiers are per provider: the same sub at another provider is a different person', sameSub.me?.email !== bound.email && !!sameSub.me);

    const susp = ident(`susp${rnd()}@example.com`);
    const suspRun = await ok('mock', susp);
    await E.rest('PATCH', `/users/${suspRun.me.id}`, { status: 'suspended' });
    const suspAgain = await flow('mock', { identity: susp });
    check('a suspended user cannot sign in with the provider', suspAgain.params.get('error') === 'denied' && !suspAgain.params.get('code'));
    check('…and their refresh token died with the suspension', (await E.rest('POST', '/auth/refresh', { refresh_token: suspRun.tokens.refresh_token }, null)).status === 401);

    // ============================================================================== forged and broken callbacks
    const attack = async (name, mutate) => {
      idp.reset();
      mutate();
      const who2 = ident(`atk${rnd()}@example.com`);
      const r = await flow('mock', { identity: who2 });
      idp.reset();
      return { r, who2 };
    };
    for (const [label, mutate] of [
      ['an ID token with alg=none', () => (idp.alg = 'none')],
      ['an HS256 token keyed with the public RSA key (algorithm confusion)', () => (idp.alg = 'HS256')],
      ['an ID token signed with a key the provider never published', () => (idp.alg = 'wrong-key')],
      ['an ID token for another client (aud)', () => (idp.claims = { aud: 'cid-other' })],
      ['an ID token from another issuer', () => (idp.claims = { iss: 'https://evil.example.net' })],
      ['an expired ID token', () => (idp.claims = { exp: Math.floor(Date.now() / 1000) - 3600 })],
      ['an ID token with the wrong nonce (replay)', () => (idp.claims = { nonce: 'f'.repeat(64) })],
      ['an ID token with no nonce', () => (idp.claims = { nonce: undefined })],
      ['an ID token with no subject', () => (idp.claims = { sub: undefined })],
      ['an ID token issued to another authorised party (azp)', () => (idp.claims = { azp: 'cid-other' })],
      ['no ID token at all', () => (idp.omitIdToken = true)],
    ]) {
      const { r, who2 } = await attack(label, mutate);
      check(`${label} is refused (error=token) and creates nothing`, r.params.get('error') === 'token' && !r.params.get('code') && users(who2.email) === 0, r.final);
    }
    const { r: audList } = await attack('aud list', () => (idp.claims = { aud: ['cid-mock', 'cid-other'], azp: 'cid-mock' }));
    check('an audience list that includes this client (with azp) is accepted', !!audList.params.get('code'), audList.final);
    const { r: noEmail, who2: noEmailWho } = await attack('userinfo', () => (idp.omitEmailInIdToken = true));
    check('when the ID token has no email, the userinfo endpoint supplies it', !!noEmail.params.get('code') && users(noEmailWho.email) === 1, noEmail.final);
    const { r: wrongSub, who2: wrongSubWho } = await attack('userinfo sub', () => { idp.omitEmailInIdToken = true; idp.userinfoSub = 'someone-else'; });
    check('…but userinfo for a different subject is refused', wrongSub.params.get('error') === 'token' && users(wrongSubWho.email) === 0, wrongSub.final);

    // CSRF / replay of the callback
    const victim = ident(`csrf${rnd()}@example.com`);
    const forgedState = await flow('mock', { identity: victim, back: (cb) => cb.replace(/state=[^&]+/, 'state=' + 'a'.repeat(64)) });
    check('a callback whose state differs from the cookie is refused (error=state)', forgedState.params.get('error') === 'state' && users(victim.email) === 0, forgedState.final);
    const noCookie = await flow('mock', { identity: victim, cookie: '' });
    check('a callback without the state cookie is refused with a plain 400 (nowhere safe to redirect)', noCookie.r3.status === 400 && (await noCookie.r3.json()).errors[0].extensions.code === 'SSO_FAILED' && users(victim.email) === 0);
    const garbage = await flow('mock', { identity: victim, cookie: 'ferro_oauth=not.a.jwt' });
    check('…nor with a forged cookie', garbage.r3.status === 400);
    const stolen = await flow('mock', { identity: victim });
    const replayCallback = await fetch(stolen.callback, { redirect: 'manual', headers: { Cookie: stolen.cookie } });
    check('replaying a used callback (same code) fails at the provider (error=provider)', queryOf(replayCallback.headers.get('location')).get('error') === 'provider', replayCallback.headers.get('location'));
    const crossProvider = await flow('closed', { identity: victim });
    const crossBack = crossProvider.callback.replace('/closed/', '/mock/');
    const mixed = await fetch(crossBack, { redirect: 'manual', headers: { Cookie: crossProvider.cookie } });
    check('a cookie started for one provider is useless on another provider’s callback', mixed.status === 400 || queryOf(mixed.headers.get('location')).get('error') === 'state', mixed.status);
    const decliner = ident(`decline${rnd()}@example.com`);
    idp.denyNext = true;
    const denied = await flow('mock', { identity: decliner });
    check('the user declining at the provider ends in error=denied', denied.params.get('error') === 'denied' && users(decliner.email) === 0, denied.final);
    const direct = await fetch(`${E.url}/auth/oauth/mock/callback?code=abc&state=def`, { redirect: 'manual' });
    check('a callback hit directly (no flow) is refused', direct.status === 400);

    // provider trouble
    idp.tokenStatus = 500;
    const outage = await flow('mock', { identity: ident(`out${rnd()}@example.com`) });
    idp.reset();
    check('a failing token endpoint ends in error=provider, not a crash', outage.params.get('error') === 'provider', outage.final);
    const down = await fetch(`${E.url}/auth/oauth/down`, { redirect: 'manual' });
    check('an unreachable provider at the start is a 502, not a hang', down.status === 502, down.status);
    idp.discoveryIssuer = 'https://evil.example.net';
    const liar = await fetch(`${E.url}/auth/oauth/liar`, { redirect: 'manual' });
    idp.reset();
    check('a discovery document that names another issuer is rejected (502)', liar.status === 502, liar.status);

    // return addresses
    const custom = await flow('mock', { identity: ident(`app${rnd()}@example.com`), redirect: 'https://app.example.com/after-login?x=1' });
    check('an allow-listed redirect receives the code (appended to its query)', /^https:\/\/app\.example\.com\/after-login\?x=1&code=[0-9a-f]{64}$/.test(custom.final), custom.final);
    const customErr = await flow('mock', { identity: outsider, redirect: 'https://app.example.com/after-login' });
    check('…and errors', customErr.final === 'https://app.example.com/after-login?error=domain', customErr.final);

    // code lifetime
    const fresh = await flow('mock', { identity: ident(`ttl${rnd()}@example.com`) });
    q("UPDATE ferro_auth_tokens SET expires = now() - interval '1 minute' WHERE kind = 'sso'");
    check('an expired one-time code is refused', (await exchange(fresh.params.get('code'))).status === 401);

    // key rotation
    idp.rotate();
    const stale = await flow('mock', { identity: ident(`rot${rnd()}@example.com`) });
    await sleep(5500);
    const rotated = await flow('mock', { identity: ident(`rot${rnd()}@example.com`) });
    check('a provider key rotation is picked up (JWKS refetched on an unknown key id)', !!rotated.params.get('code'), [stale.params.get('error'), rotated.final]);

    // plain OAuth 2.0
    const o2 = ident(`oauth2${rnd()}@example.com`, { given_name: 'Olga' });
    const o2run = await ok('plain', o2);
    check('an OAuth 2.0 provider (no ID token): the profile endpoint supplies the identity', o2run.me?.email === o2.email && o2run.me.first_name === 'Olga' && o2run.me.provider === 'plain', o2run.me);
    check('…the numeric id is the identifier', q(`SELECT external_identifier FROM ferro_users WHERE email = '${o2.email}'`)[0][0] === '1234');
    const plainAuth = idp.log.filter((l) => l.type === 'userinfo').at(-1);
    check('…fetched with the access token as a bearer', plainAuth?.authorized === true);
  } finally {
    await E.stop();
    idp.stop();
  }
}
