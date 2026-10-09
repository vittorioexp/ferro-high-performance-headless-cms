// Test helpers for the identity suites: a fake SMTP server, TOTP codes, SQL access to the environment databases.
import { spawnSync } from 'node:child_process';
import { createHmac } from 'node:crypto';
import net from 'node:net';

import { PG } from './lib.mjs';

// ---- TOTP (RFC 6238) ---------------------------------------------------------------------------------------------------
const B32 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ234567';
export function base32Decode(s) {
  let bits = 0, value = 0;
  const out = [];
  for (const c of s.replace(/=+$/, '').toUpperCase()) {
    const v = B32.indexOf(c);
    if (v < 0) throw new Error(`bad base32 character ${c}`);
    value = (value << 5) | v;
    bits += 5;
    if (bits >= 8) {
      out.push((value >>> (bits - 8)) & 255);
      bits -= 8;
    }
  }
  return Buffer.from(out);
}
/** The 6-digit code for a base32 secret at a step (default: now). `offset` moves by whole 30-second steps. */
export function totp(secret, offset = 0, nowMs = Date.now()) {
  const step = Math.floor(nowMs / 1000 / 30) + offset;
  const counter = Buffer.alloc(8);
  counter.writeBigUInt64BE(BigInt(step));
  const h = createHmac('sha1', base32Decode(secret)).update(counter).digest();
  const o = h[19] & 15;
  const n = ((h[o] & 0x7f) << 24) | (h[o + 1] << 16) | (h[o + 2] << 8) | h[o + 3];
  return String(n % 1_000_000).padStart(6, '0');
}

// ---- SQL ---------------------------------------------------------------------------------------------------------------------
/** Run SQL on an environment database; returns the rows as arrays of strings (psql -At). */
export function sql(db, statement) {
  const r = spawnSync('psql', ['-At', '-F', '\t', '-c', statement, `${PG}/${db}`], { encoding: 'utf8' });
  if (r.status !== 0) throw new Error(`psql failed: ${r.stderr}`);
  return r.stdout.trim() === '' ? [] : r.stdout.trim().split('\n').map((l) => l.split('\t'));
}

// ---- fake SMTP -----------------------------------------------------------------------------------------------------------------
function decodeBody(headers, body) {
  const enc = (headers['content-transfer-encoding'] || '7bit').toLowerCase();
  if (enc === 'base64') return Buffer.from(body.replace(/\s+/g, ''), 'base64').toString('utf8');
  if (enc === 'quoted-printable') {
    return Buffer.from(body.replace(/=\r?\n/g, '').replace(/=([0-9A-F]{2})/gi, (_, h) => String.fromCharCode(parseInt(h, 16))), 'latin1').toString('utf8');
  }
  return body;
}
function parseHeaders(block) {
  const headers = {};
  for (const line of block.replace(/\r?\n[ \t]+/g, ' ').split(/\r?\n/)) {
    const i = line.indexOf(':');
    if (i > 0) headers[line.slice(0, i).toLowerCase()] = line.slice(i + 1).trim();
  }
  return headers;
}
/** { subject, from, to, text, html } from a raw RFC 5322 message (multipart/alternative with text and html). */
export function parseMail(raw) {
  const [head, ...rest] = raw.split(/\r?\n\r?\n/);
  const headers = parseHeaders(head);
  const body = rest.join('\n\n');
  const out = { subject: headers.subject, from: headers.from, to: headers.to, text: '', html: '' };
  const m = /boundary="?([^";]+)"?/i.exec(headers['content-type'] || '');
  if (!m) {
    out.text = decodeBody(headers, body);
    return out;
  }
  for (const part of body.split(`--${m[1]}`)) {
    const [ph, ...pb] = part.replace(/^\r?\n/, '').split(/\r?\n\r?\n/);
    const h = parseHeaders(ph);
    const type = (h['content-type'] || '').toLowerCase();
    const content = decodeBody(h, pb.join('\n\n').replace(/\r?\n$/, ''));
    if (type.startsWith('text/plain')) out.text = content;
    else if (type.startsWith('text/html')) out.html = content;
  }
  return out;
}

export class FakeSmtp {
  constructor() {
    this.messages = [];
    this.server = net.createServer((sock) => this.#session(sock));
  }
  async start(port = 0) {
    await new Promise((r) => this.server.listen(port, '127.0.0.1', r));
    this.port = this.server.address().port;
    return this;
  }
  stop() {
    this.server.close();
    for (const s of this.sockets ?? []) s.destroy();
  }
  #session(sock) {
    (this.sockets ??= new Set()).add(sock);
    sock.on('close', () => this.sockets.delete(sock));
    sock.on('error', () => {});
    let buf = '', data = null, from = '', rcpt = [];
    const say = (line) => sock.write(`${line}\r\n`);
    say('220 fake.smtp ESMTP');
    sock.on('data', (chunk) => {
      buf += chunk.toString('latin1');
      for (;;) {
        if (data !== null) {
          const end = buf.indexOf('\r\n.\r\n');
          if (end < 0) return;
          data = buf.slice(0, end);
          buf = buf.slice(end + 5);
          this.messages.push({ at: Date.now(), from, rcpt: [...rcpt], raw: data, ...parseMail(data.replace(/^\.\./gm, '.')) });
          data = null;
          say('250 queued');
          continue;
        }
        const nl = buf.indexOf('\r\n');
        if (nl < 0) return;
        const line = buf.slice(0, nl);
        buf = buf.slice(nl + 2);
        const cmd = line.slice(0, 4).toUpperCase();
        if (cmd === 'EHLO' || cmd === 'HELO') say('250 fake.smtp');
        else if (cmd === 'MAIL') { from = line.slice(10).replace(/[<>]/g, '').trim(); rcpt = []; say('250 ok'); }
        else if (cmd === 'RCPT') { rcpt.push(line.slice(8).replace(/[<>]/g, '').trim().toLowerCase()); say('250 ok'); }
        else if (cmd === 'DATA') { data = ''; say('354 go'); }
        else if (cmd === 'QUIT') { say('221 bye'); sock.end(); return; }
        else if (cmd === 'RSET' || cmd === 'NOOP') say('250 ok');
        else say('250 ok');
      }
    });
  }
  /** Messages for an address received since `since` (ms epoch). */
  to(address, since = 0) {
    return this.messages.filter((m) => m.rcpt.includes(address.toLowerCase()) && m.at >= since);
  }
  /** Wait for a message to `address`; null after `timeoutMs`. */
  async wait(address, since = 0, timeoutMs = 8000) {
    const end = Date.now() + timeoutMs;
    while (Date.now() < end) {
      const m = this.to(address, since).at(-1);
      if (m) return m;
      await new Promise((r) => setTimeout(r, 50));
    }
    return null;
  }
}

/** The `token=` value of the first link in a message. */
export const tokenIn = (mail) => /[?&]token=([0-9a-f]{64})/.exec(mail.text)?.[1] ?? null;
export const linkIn = (mail) => /(https?:\/\/\S+token=[0-9a-f]{64})/.exec(mail.text)?.[1] ?? null;
