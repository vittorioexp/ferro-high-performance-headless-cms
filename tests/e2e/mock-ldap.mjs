// A real LDAP server (ldapjs) with a tiny directory, for the directory-login tests. Everything the client does is logged so
// tests can assert what Ferro sent (filters, which DNs were bound and with what).
import { spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import ldap from 'ldapjs';

export const BASE = 'ou=people,dc=example,dc=com';
export const SERVICE_DN = 'cn=ferro,ou=services,dc=example,dc=com';
export const SERVICE_PASSWORD = 'svc-secret';

const lower = (attrs) => Object.fromEntries(Object.entries(attrs).map(([k, v]) => [k.toLowerCase(), v]));
const person = (uid, extra) => ({ dn: `uid=${uid},${BASE}`, password: `${uid}-secret-1`, attributes: lower({ objectClass: ['inetOrgPerson', 'person'], uid, ...extra }) });

export function defaultDirectory() {
  return [
    person('alice', { mail: 'alice@example.com', givenName: 'Alice', sn: 'Liddell', entryUUID: '11111111-1111-1111-1111-111111111111' }),
    person('bob', { mail: 'bob@example.com', givenName: 'Bob', sn: 'Builder', entryUUID: '22222222-2222-2222-2222-222222222222' }),
    { ...person('carol', { mail: 'carol@example.com', givenName: 'Carol', sn: 'Ünïcode', entryUUID: '33333333-3333-3333-3333-333333333333' }), password: 'päss wörd ✓ 1' },
    person('dave', { mail: 'dave@example.com', givenName: 'Dave', sn: 'Davies', entryUUID: '44444444-4444-4444-4444-444444444444' }),
    person('dup1', { mail: 'dup@example.com', entryUUID: '55555555-5555-5555-5555-555555555551' }),
    person('dup2', { mail: 'dup@example.com', entryUUID: '55555555-5555-5555-5555-555555555552' }),
    person('nomail', { entryUUID: '66666666-6666-6666-6666-666666666666' }),
  ];
}

/** A throw-away self-signed certificate for 127.0.0.1 (needs the openssl binary); null if it is not available. */
export function selfSignedCert() {
  const dir = mkdtempSync(join(tmpdir(), 'ferro-ldap-'));
  const r = spawnSync('openssl', ['req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-keyout', join(dir, 'key.pem'), '-out', join(dir, 'cert.pem'), '-subj', '/CN=127.0.0.1', '-days', '2', '-addext', 'subjectAltName=IP:127.0.0.1'], { encoding: 'utf8' });
  if (r.status !== 0) return null;
  return { key: readFileSync(join(dir, 'key.pem'), 'utf8'), cert: readFileSync(join(dir, 'cert.pem'), 'utf8') };
}

export class MockLdap {
  constructor(port, { tls } = {}) {
    this.port = port;
    this.entries = defaultDirectory();
    this.log = [];
    this.server = ldap.createServer(tls ? { certificate: tls.cert, key: tls.key } : {});
    const bound = (req) => req.connection.ldap.bindDN.toString();

    // bind: the service account, any person with their password; an empty password is the "unauthenticated bind" that
    // real servers accept as anonymous, so it is logged loudly
    this.server.bind('', (req, res, next) => {
      const dn = req.dn.toString().toLowerCase();
      const password = req.credentials;
      this.log.push({ type: 'bind', dn: req.dn.toString(), empty: password === '' });
      if (dn === SERVICE_DN.toLowerCase()) {
        if (password !== SERVICE_PASSWORD) return next(new ldap.InvalidCredentialsError());
        res.end();
        return next();
      }
      const entry = this.entries.find((e) => e.dn.toLowerCase() === dn);
      if (!entry || password === '' || entry.password !== password) return next(new ldap.InvalidCredentialsError());
      res.end();
      return next();
    });

    this.server.search(BASE, (req, res, next) => {
      this.log.push({ type: 'search', filter: req.filter.toString(), bindDn: bound(req) });
      if (bound(req).toLowerCase() !== SERVICE_DN.toLowerCase()) return next(new ldap.InsufficientAccessRightsError());
      // matching is case-insensitive (caseIgnoreMatch on mail and uid), so compare lower-cased attributes with a lower-cased filter
      const filter = ldap.parseFilter(req.filter.toString().toLowerCase());
      const fold = (a) => Object.fromEntries(Object.entries(a).map(([k, v]) => [k, [].concat(v).map((x) => String(x).toLowerCase())]));
      // ldapjs would drop camelCase attributes the client asked for (it compares them case-sensitively): ask for everything
      res.attributes = ['*'];
      for (const e of this.entries) if (filter.matches(fold(e.attributes))) res.send({ dn: e.dn, attributes: e.attributes }, true);
      res.end();
      return next();
    });
  }

  async start() {
    await new Promise((resolve, reject) => {
      this.server.on('error', reject);
      this.server.listen(this.port, '127.0.0.1', resolve);
    });
    return this;
  }

  stop() {
    this.server.close();
  }

  /** Binds as a person (not the service account), i.e. password checks. */
  userBinds() {
    return this.log.filter((l) => l.type === 'bind' && l.dn.toLowerCase() !== SERVICE_DN.toLowerCase() && l.dn !== '');
  }
}
