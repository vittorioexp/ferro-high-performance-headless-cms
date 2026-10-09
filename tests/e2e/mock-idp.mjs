// A small OpenID Connect / OAuth 2.0 identity provider for tests: discovery, authorize (auto-approves the identity set by
// the test), token (client secret and PKCE enforced, codes single use), JWKS and userinfo. Every knob that lets a test
// forge or break something is explicit.
import { createHash, createHmac, generateKeyPairSync, randomBytes, sign } from 'node:crypto';
import http from 'node:http';

const b64u = (b) => Buffer.from(b).toString('base64url');
const json = (o) => b64u(JSON.stringify(o));

export class MockIdp {
  constructor(port = 18070) {
    this.port = port;
    this.issuer = `http://127.0.0.1:${port}`;
    /** client id -> secret */
    this.clients = {};
    /** Who signs in next: { sub, email, email_verified, given_name, family_name, ... } */
    this.identity = null;
    this.keys = [];
    this.rotate();
    this.codes = new Map();
    this.log = [];
    this.reset();
  }

  /** Restore every knob to its default (call between tests). */
  reset() {
    this.claims = {}; // merged into the ID token ({ key: undefined } removes the claim)
    this.alg = 'RS256'; // RS256 | none | HS256 | wrong-key
    this.omitEmailInIdToken = false;
    this.userinfoSub = null; // override the subject returned by userinfo
    this.tokenStatus = 200;
    this.denyNext = false;
    this.omitIdToken = false;
    this.discoveryIssuer = this.issuer;
  }

  /** New signing key (the old one disappears from the JWKS, like a rotation). */
  rotate() {
    const { publicKey, privateKey } = generateKeyPairSync('rsa', { modulusLength: 2048 });
    const kid = `k${randomBytes(4).toString('hex')}`;
    this.keys = [{ kid, privateKey, publicKey, jwk: { ...publicKey.export({ format: 'jwk' }), kid, alg: 'RS256', use: 'sig' } }];
    this.other = generateKeyPairSync('rsa', { modulusLength: 2048 }); // a key the server was never told about
  }

  async start() {
    this.server = http.createServer((req, res) => this.#handle(req, res).catch((e) => { res.writeHead(500); res.end(String(e)); }));
    await new Promise((r) => this.server.listen(this.port, '127.0.0.1', r));
    return this;
  }
  stop() {
    this.server?.closeAllConnections?.();
    this.server?.close();
  }

  idToken({ aud, nonce, sub, email }) {
    const now = Math.floor(Date.now() / 1000);
    const id = this.identity;
    const claims = {
      iss: this.issuer, aud, sub: sub ?? id.sub, iat: now, exp: now + 300, nonce,
      ...(this.omitEmailInIdToken ? {} : { email: email ?? id.email, email_verified: id.email_verified === 'absent' ? undefined : (id.email_verified ?? true), given_name: id.given_name, family_name: id.family_name }),
      ...this.claims,
    };
    for (const k of Object.keys(claims)) if (claims[k] === undefined) delete claims[k];
    const key = this.keys[0];
    if (this.alg === 'none') return `${json({ alg: 'none', typ: 'JWT' })}.${json(claims)}.`;
    if (this.alg === 'HS256') {
      // the classic key-confusion attack: HMAC keyed with the (public) RSA key
      const input = `${json({ alg: 'HS256', typ: 'JWT', kid: key.kid })}.${json(claims)}`;
      return `${input}.${createHmac('sha256', key.publicKey.export({ type: 'spki', format: 'pem' })).update(input).digest('base64url')}`;
    }
    const signer = this.alg === 'wrong-key' ? this.other.privateKey : key.privateKey;
    const input = `${json({ alg: 'RS256', typ: 'JWT', kid: key.kid })}.${json(claims)}`;
    return `${input}.${sign('sha256', Buffer.from(input), signer).toString('base64url')}`;
  }

  async #handle(req, res) {
    const url = new URL(req.url, this.issuer);
    const send = (status, body, headers = {}) => {
      res.writeHead(status, { 'Content-Type': 'application/json', ...headers });
      res.end(typeof body === 'string' ? body : JSON.stringify(body));
    };
    if (url.pathname === '/.well-known/openid-configuration') {
      return send(200, {
        issuer: this.discoveryIssuer, authorization_endpoint: `${this.issuer}/authorize`, token_endpoint: `${this.issuer}/token`,
        jwks_uri: `${this.issuer}/jwks`, userinfo_endpoint: `${this.issuer}/userinfo`, id_token_signing_alg_values_supported: ['RS256'],
      });
    }
    if (url.pathname === '/jwks') {
      this.log.push({ type: 'jwks' });
      return send(200, { keys: this.keys.map((k) => k.jwk) });
    }
    if (url.pathname === '/authorize') {
      const q = url.searchParams;
      this.log.push({ type: 'authorize', params: Object.fromEntries(q) });
      const secret = this.clients[q.get('client_id')];
      if (!secret) return send(400, { error: 'unauthorized_client' });
      const redirect = new URL(q.get('redirect_uri'));
      if (q.get('response_type') !== 'code' || q.get('code_challenge_method') !== 'S256' || !q.get('code_challenge') || !q.get('state')) {
        redirect.searchParams.set('error', 'invalid_request');
        redirect.searchParams.set('state', q.get('state') ?? '');
        res.writeHead(302, { Location: redirect.toString() });
        return res.end();
      }
      if (this.denyNext || !this.identity) {
        this.denyNext = false;
        redirect.searchParams.set('error', 'access_denied');
      } else {
        const code = randomBytes(16).toString('hex');
        this.codes.set(code, { client: q.get('client_id'), redirect_uri: q.get('redirect_uri'), challenge: q.get('code_challenge'), nonce: q.get('nonce'), identity: this.identity, used: false });
        redirect.searchParams.set('code', code);
      }
      redirect.searchParams.set('state', q.get('state'));
      res.writeHead(302, { Location: redirect.toString() });
      return res.end();
    }
    if (url.pathname === '/token' && req.method === 'POST') {
      let body = '';
      for await (const c of req) body += c;
      const f = Object.fromEntries(new URLSearchParams(body));
      this.log.push({ type: 'token', params: f });
      if (this.tokenStatus !== 200) return send(this.tokenStatus, { error: 'server_error' });
      const entry = this.codes.get(f.code);
      if (f.client_secret !== this.clients[f.client_id]) return send(401, { error: 'invalid_client' });
      if (!entry || entry.used || entry.client !== f.client_id || entry.redirect_uri !== f.redirect_uri) return send(400, { error: 'invalid_grant' });
      entry.used = true;
      const challenge = createHash('sha256').update(f.code_verifier ?? '').digest('base64url');
      if (challenge !== entry.challenge) return send(400, { error: 'invalid_grant', error_description: 'PKCE verification failed' });
      const accessToken = `at-${randomBytes(8).toString('hex')}`;
      this.accessTokens = { ...(this.accessTokens ?? {}), [accessToken]: entry.identity };
      const saved = this.identity;
      this.identity = entry.identity;
      const out = { access_token: accessToken, token_type: 'Bearer', expires_in: 300 };
      if (!this.omitIdToken) out.id_token = this.idToken({ aud: f.client_id, nonce: entry.nonce });
      this.identity = saved;
      return send(200, out);
    }
    if (url.pathname === '/userinfo' || url.pathname === '/profile') {
      const id = this.accessTokens?.[(req.headers.authorization ?? '').replace(/^Bearer /, '')];
      this.log.push({ type: 'userinfo', authorized: !!id });
      if (!id) return send(401, { error: 'invalid_token' });
      if (url.pathname === '/profile') return send(200, { id: id.numericId ?? 1234, email: id.email, name: id.given_name });
      return send(200, { sub: this.userinfoSub ?? id.sub, email: id.email, email_verified: id.email_verified ?? true, given_name: id.given_name, family_name: id.family_name });
    }
    send(404, { error: 'not_found' });
  }
}
