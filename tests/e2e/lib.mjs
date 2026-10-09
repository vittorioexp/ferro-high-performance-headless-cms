// Helpers for the integration suite: environments (a server + its database), REST client, CLI wrapper, checks.
import { spawn, spawnSync } from 'node:child_process';
import { mkdirSync, openSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

export const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
export const BIN = process.env.FERRO_BIN || join(ROOT, 'target/debug', process.platform === 'win32' ? 'ferro.exe' : 'ferro');
export const PG = process.env.E2E_PG || 'postgres://ferro@localhost:55432';
export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ---- checks -------------------------------------------------------------------------------
let passed = 0;
const failures = [];
export function section(title) {
  console.log(`\n## ${title}`);
}
export function check(name, cond, extra = '') {
  if (cond) passed++;
  else failures.push(name);
  console.log(`${cond ? 'PASS' : 'FAIL'}  ${name}${extra !== '' ? `  [${typeof extra === 'string' ? extra : JSON.stringify(extra)}]` : ''}`);
  return !!cond;
}
export function summary() {
  console.log(`\n${passed} passed, ${failures.length} failed`);
  for (const f of failures) console.log(`  FAILED: ${f}`);
  return failures.length === 0;
}

// ---- an environment ---------------------------------------------------------------------------
export class Env {
  /** opts.reuseDb + opts.token: a second instance on the database (and the admin token) of another environment. */
  constructor(name, port, db, extraEnv = {}, opts = {}) {
    this.opts = opts;
    this.name = name;
    this.port = port;
    this.db = db;
    this.url = `http://127.0.0.1:${port}`;
    this.ws = `ws://127.0.0.1:${port}/websocket`;
    this.extraEnv = extraEnv;
    this.token = null;
    this.proc = null;
  }

  get env() {
    return {
      ...process.env,
      FERRO_DATABASE_URL: `${PG}/${this.db}`,
      FERRO_SECRET: `e2e-secret-${this.name}-e2e-secret-${this.name}-e2e-secret`,
      FERRO_BIND: `127.0.0.1:${this.port}`,
      FERRO_ENV: this.name,
      FERRO_ADMIN_EMAIL: 'admin@example.com',
      FERRO_ADMIN_PASSWORD: 'e2e-password-123',
      FERRO_STORAGE_LOCAL_ROOT: join(ROOT, 'target/e2e', `uploads-${this.name}`),
      FERRO_LOG: 'info,sqlx=warn',
      ...this.extraEnv,
    };
  }

  /** Fresh database, running server, admin token. */
  async start() {
    for (const sql of this.opts.reuseDb ? [] : [`DROP DATABASE IF EXISTS ${this.db}`, `CREATE DATABASE ${this.db}`]) {
      const r = spawnSync('psql', ['-q', '-c', sql, `${PG}/postgres`], { encoding: 'utf8' });
      if (r.status !== 0) throw new Error(`psql failed: ${r.stderr || r.error}`);
    }
    mkdirSync(join(ROOT, 'target/e2e'), { recursive: true });
    const log = openSync(join(ROOT, 'target/e2e', `${this.name}.log`), 'w');
    this.proc = spawn(BIN, ['serve'], { env: this.env, stdio: ['ignore', log, log], cwd: ROOT });
    for (let i = 0; i < 80; i++) {
      await sleep(250);
      try {
        const r = await fetch(`${this.url}/server/health`);
        if (r.ok) break;
      } catch {
        /* not yet */
      }
      if (i === 79) throw new Error(`${this.name}: server did not start (see target/e2e/${this.name}.log)`);
    }
    if (this.opts.token) {
      this.token = this.opts.token;
      return this;
    }
    const t = spawnSync(BIN, ['user', 'token', '--email', 'admin@example.com'], { env: this.env, encoding: 'utf8', cwd: ROOT });
    this.token = t.stdout.trim();
    if (!this.token.startsWith('ferro_')) throw new Error(`${this.name}: cannot create a token: ${t.stderr}`);
    return this;
  }

  async stop() {
    if (this.proc) this.proc.kill();
    await sleep(300);
  }

  /** Restart the server keeping the database (used to change its configuration). */
  async restart(extraEnv) {
    await this.stop();
    this.extraEnv = { ...this.extraEnv, ...extraEnv };
    const log = openSync(join(ROOT, 'target/e2e', `${this.name}.log`), 'a');
    this.proc = spawn(BIN, ['serve'], { env: this.env, stdio: ['ignore', log, log], cwd: ROOT });
    for (let i = 0; i < 80; i++) {
      await sleep(250);
      try {
        if ((await fetch(`${this.url}/server/health`)).ok) return;
      } catch {
        /* not yet */
      }
    }
    throw new Error(`${this.name}: restart failed`);
  }

  /** REST call. Returns { status, json, text, headers }. */
  async rest(method, path, body, token = this.token) {
    const headers = { ...(token ? { Authorization: `Bearer ${token}` } : {}) };
    let payload;
    if (body !== undefined) {
      headers['Content-Type'] = 'application/json';
      payload = typeof body === 'string' ? body : JSON.stringify(body);
    }
    const res = await fetch(this.url + path, { method, headers, body: payload });
    const text = await res.text();
    let json = null;
    try {
      json = text ? JSON.parse(text) : null;
    } catch {
      /* not JSON */
    }
    return { status: res.status, json, text, headers: res.headers, data: json?.data, error: json?.errors?.[0]?.message, code: json?.errors?.[0]?.extensions?.code };
  }

  graphql(query, variables, token = this.token) {
    return this.rest('POST', '/graphql', { query, variables }, token);
  }

  /** `ferro` CLI with this environment's tokens available as FERRO_<NAME>_TOKEN. */
  cli(args, extraEnv = {}) {
    return cli(args, { [`FERRO_${this.name.toUpperCase()}_TOKEN`]: this.token, ...extraEnv });
  }
}

/** Run the CLI with the e2e project file. */
export function cli(args, env = {}) {
  const r = spawnSync(BIN, ['--project', join(ROOT, 'tests/e2e/ferro.toml'), ...args], { env: { ...process.env, ...env }, encoding: 'utf8', cwd: ROOT, timeout: 120000 });
  return { code: r.status, out: `${r.stdout || ''}${r.stderr || ''}`, stdout: r.stdout || '' };
}

export const uuid = () => globalThis.crypto.randomUUID();
export const rnd = () => Math.random().toString(36).slice(2, 8);
