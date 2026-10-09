// Integration suite. Needs: a built binary (cargo build -p ferro-cli), PostgreSQL reachable at E2E_PG
// (default postgres://ferro@localhost:55432, user allowed to create databases) and `psql` in the PATH.
//
//   node tests/e2e/run.mjs            all suites
//   node tests/e2e/run.mjs sync flows only the suites whose file name contains one of the words
import { readdirSync } from 'node:fs';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';

import { Env, ROOT, check, cli, section, sleep, summary } from './lib.mjs';

const only = process.argv.slice(2);
const dir = join(ROOT, 'tests/e2e');
const suites = readdirSync(dir)
  .filter((f) => /^\d\d-.*\.mjs$/.test(f))
  .filter((f) => only.length === 0 || only.some((o) => f.includes(o)))
  .sort();

const a = new Env('a', 18055, 'ferro_e2e_a', { FERRO_LOGIN_LOCKOUT_SECS: '3', FERRO_FLOW_ENV_PUBLISH_HOOK_TOKEN: 'tok123', FERRO_FLOW_ALLOW_PRIVATE: 'true', FERRO_WEBHOOK_SECRET: 's3cret', FERRO_EXTENSIONS_DIR: join(ROOT, 'examples/site/extensions'), FERRO_ADMIN_DIR: join(ROOT, 'admin') });
const b = new Env('b', 18056, 'ferro_e2e_b', {});

let ok = false;
try {
  console.log('starting environments…');
  await Promise.all([a.start(), b.start()]);
  // both environments get the example project (model, roles, policies, flows)
  for (const env of [a, b]) {
    const r = env.cli(['schema', 'apply', '--from', join(ROOT, 'examples/site/schema'), '--to', env.name, '--yes']);
    if (r.code !== 0) throw new Error(`cannot apply the example schema to ${env.name}:\n${r.out}`);
  }
  for (const f of suites) {
    section(f);
    const mod = await import(pathToFileURL(join(dir, f)).href);
    try {
      await mod.default({ a, b, check, cli, sleep, section });
    } catch (err) {
      check(`${f} did not crash`, false, String(err?.stack || err).split('\n').slice(0, 4).join(' | '));
    }
  }
  ok = summary();
} finally {
  await Promise.all([a.stop(), b.stop()]);
}
process.exit(ok ? 0 : 1);
