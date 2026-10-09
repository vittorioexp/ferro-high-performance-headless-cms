// Object storage: S3-compatible bucket next to the local disk (several locations), signed redirects,
// replace/delete semantics, migration between locations, promotion of files from an object store to a disk.
//
// Needs an S3-compatible server; skipped (with a notice) unless E2E_S3_ENDPOINT is set:
//   E2E_S3_ENDPOINT=http://127.0.0.1:19000  E2E_S3_BUCKET=ferro-test  E2E_S3_KEY=...  E2E_S3_SECRET=...
//   E2E_S3_FILER=http://127.0.0.1:8888      (optional, SeaweedFS only: lets the suite look into the bucket independently)
import { spawnSync } from 'node:child_process';
import { join } from 'node:path';
import { createHash } from 'node:crypto';

import { Env, BIN, ROOT, cli, rnd } from './lib.mjs';
import { decodePng, quadrants } from './png.mjs';

const sha = (buf) => createHash('sha256').update(buf).digest('hex');

export default async function ({ b, check }) {
  const endpoint = process.env.E2E_S3_ENDPOINT;
  if (!endpoint) {
    console.log('SKIP  no E2E_S3_ENDPOINT: object-storage suite not run');
    return;
  }
  const bucket = process.env.E2E_S3_BUCKET || 'ferro-test';
  const prefix = `e2e-${rnd()}/`;
  const filer = process.env.E2E_S3_FILER;
  const s3env = {
    FERRO_STORAGE_LOCATIONS: 'local,s3',
    FERRO_STORAGE_DEFAULT: 's3',
    FERRO_STORAGE_S3_DRIVER: 's3',
    FERRO_STORAGE_S3_BUCKET: bucket,
    FERRO_STORAGE_S3_PREFIX: prefix,
    FERRO_STORAGE_S3_ENDPOINT: endpoint,
    FERRO_STORAGE_S3_ACCESS_KEY_ID: process.env.E2E_S3_KEY || 'ferrokey',
    FERRO_STORAGE_S3_SECRET_ACCESS_KEY: process.env.E2E_S3_SECRET || 'ferrosecret123',
  };
  const c = new Env('c', 18057, 'ferro_e2e_c', s3env);
  const sh = (args) => cli(args, { FERRO_B_TOKEN: b.token, FERRO_C_TOKEN: c.token });
  const tool = (args, extra = {}) => {
    const r = spawnSync(BIN, ['storage', ...args], { env: { ...c.env, ...extra }, encoding: 'utf8', cwd: ROOT });
    return { code: r.status, out: `${r.stdout || ''}${r.stderr || ''}` };
  };
  const upload = async (content, name, type = 'text/plain', query = '') => {
    const fd = new FormData();
    fd.append('file', new Blob([content], { type }), name);
    const res = await fetch(`${c.url}/files${query}`, { method: 'POST', headers: { Authorization: `Bearer ${c.token}` }, body: fd });
    return { status: res.status, json: await res.json() };
  };
  const asset = (id, extra = {}) => fetch(`${c.url}/assets/${id}`, { redirect: 'manual', headers: { Authorization: `Bearer ${c.token}` }, ...extra });
  const inBucket = async (id) => (filer ? (await fetch(`${filer}/buckets/${bucket}/${prefix}${id}`, { method: 'HEAD' })).status === 200 : null);

  // the example project lets the public policy read files: switch that on/off to test permissions
  const setPublicFiles = async (on) => {
    const policy = (await c.rest('GET', '/policies')).data.find((p) => p.public);
    const perms = (policy.permissions || []).filter((p) => !(p.collection === 'ferro_files' && p.action === 'read'));
    if (on) perms.push({ collection: 'ferro_files', action: 'read' });
    const res = await c.rest('PUT', `/policies/${policy.id}`, { ...policy, permissions: perms });
    if (res.status !== 200) throw new Error(`cannot change the public policy: ${res.text}`);
  };

  try {
    // ---- startup ------------------------------------------------------------------------------------
    await c.start();
    const apply = cli(['schema', 'apply', '--from', join(ROOT, 'examples/site/schema'), '--to', 'c', '--yes'], { FERRO_C_TOKEN: c.token });
    check('example schema applied to the S3 environment', apply.code === 0, apply.out.slice(-200));
    const info = (await c.rest('GET', '/server/info')).data;
    check('/server/info lists the locations and the default', info.storage?.default === 's3' && info.storage.locations.join() === 'local,s3', info.storage);

    // ---- upload / download ------------------------------------------------------------------------
    const first = await upload('hello object store', 'hello.txt');
    const id = first.json.data?.id;
    check('upload goes to the default location (s3)', first.status === 200 && first.json.data.storage === 's3' && first.json.data.filesize === 18, first.json);
    const down = await asset(id);
    check('download streams the same bytes with type and etag', down.status === 200 && (await down.text()) === 'hello object store' && down.headers.get('content-type').startsWith('text/plain') && !!down.headers.get('etag'));
    if (filer) check('the object really is in the bucket (independent look, under the prefix)', await inBucket(id));

    // ---- replace --------------------------------------------------------------------------------------
    const rep = await upload('second version!', 'hello.txt', 'text/plain', `?replace=${id}`);
    check('replace keeps the id and updates the row', rep.json.data?.id === id && rep.json.data.filesize === 15 && rep.json.data.storage === 's3', rep.json);
    check('replace serves the new content', (await (await asset(id)).text()) === 'second version!');

    // ---- permissions come before any redirect / bytes -----------------------------------------------------------
    // a user whose role has no policy at all (the example project makes files readable by the public policy,
    // so anonymous access is legitimate there)
    const role = await c.rest('POST', '/roles', { name: `nobody-${rnd()}` });
    const email = `nobody${rnd()}@example.com`;
    await c.rest('POST', '/users', { email, password: 'nobody-pass-123', role: role.data?.id });
    const nobody = (await c.rest('POST', '/auth/login', { email, password: 'nobody-pass-123' }, null)).data?.access_token;
    const pub = await fetch(`${c.url}/assets/${id}`, { redirect: 'manual' });
    check('the public policy of the example project serves public files', pub.status === 200, pub.status);
    await setPublicFiles(false);
    const denied = await fetch(`${c.url}/assets/${id}`, { redirect: 'manual', headers: { Authorization: `Bearer ${nobody}` } });
    const deniedAnon = await fetch(`${c.url}/assets/${id}`, { redirect: 'manual' });
    check('without read permission on files: 403 for users and anonymous (nothing leaks from the bucket)', !!nobody && denied.status === 403 && [401, 403].includes(deniedAnon.status), `${!!nobody} ${denied.status} ${deniedAnon.status}`);
    await setPublicFiles(true);

    // ---- large file ---------------------------------------------------------------------------------------
    const big = Buffer.alloc(12 * 1024 * 1024);
    for (let i = 0; i < big.length; i += 4099) big[i] = (i / 4099) & 0xff;
    const bigUp = await upload(big, 'big.bin', 'application/octet-stream');
    const bigDown = Buffer.from(await (await asset(bigUp.json.data.id)).arrayBuffer());
    check('12 MB file round-trips byte for byte', bigUp.json.data?.filesize === big.length && sha(bigDown) === sha(big) && bigUp.json.data.checksum === sha(big));

    // ---- delete: the binary goes after the commit ------------------------------------------------------------------
    const gone = await upload('to be deleted', 'gone.txt');
    const goneId = gone.json.data.id;
    const del = await c.rest('DELETE', `/files/${goneId}`);
    check('delete answers 204', del.status === 204, del.status);
    check('deleted file: asset is 404', (await asset(goneId)).status === 404);
    if (filer) check('deleted file: the object is gone from the bucket', (await inBucket(goneId)) === false);
    const stillThere = await c.rest('DELETE', `/files/${id}`, undefined, 'bogus-token');
    check('delete with a bad token changes nothing', stillThere.status === 401 && (await asset(id)).status === 200);

    // ---- a failed upload must not leave an orphan blob --------------------------------------------------------------
    const before = tool(['list']).out;
    const objects = async () => (filer ? ((await (await fetch(`${filer}/buckets/${bucket}/${prefix}`, { headers: { Accept: 'application/json' } })).json()).Entries || []).length : null);
    const n0 = await objects();
    const bad = new FormData();
    bad.append('file', new Blob(['orphan?'], { type: 'text/plain' }), 'ok.txt');
    bad.append('folder', 'not-a-uuid'); // parsed after the file was already written: the request fails with 400
    const res = await fetch(`${c.url}/files`, { method: 'POST', headers: { Authorization: `Bearer ${c.token}` }, body: bad });
    check('a request that fails after the blob was written answers 400', res.status === 400, res.status);
    if (filer) check('…and the blob it wrote is removed again (no orphan in the bucket)', (await objects()) === n0, `${n0} -> ${await objects()}`);
    const check1 = tool(['check', '--exit-code']);
    check('`ferro storage check`: every row has its binary', check1.code === 0 && /0 missing/.test(check1.out), check1.out.slice(-200));
    check('`ferro storage list` shows both locations', /^local\s+local/m.test(before) && /^s3\s+s3.*\(default\)/m.test(before), before);

    // ---- signed redirect --------------------------------------------------------------------------------------------
    await c.restart({ FERRO_STORAGE_S3_PRESIGN: '120' });
    const red = await asset(id);
    const loc = red.headers.get('location') || '';
    check('with PRESIGN the asset answers 307 to a signed URL', red.status === 307 && /X-Amz-Signature=/.test(loc) && loc.startsWith(endpoint), `${red.status} ${loc.slice(0, 120)}`);
    check('the redirect is not cacheable', red.headers.get('cache-control') === 'no-store');
    const viaUrl = await fetch(loc);
    check('the signed URL serves the bytes (inline, with the right filename)', viaUrl.status === 200 && (await viaUrl.text()) === 'second version!' && /filename="hello\.txt"/.test(viaUrl.headers.get('content-disposition') || ''));
    const tampered = await fetch(loc.replace(/X-Amz-Signature=[0-9a-f]{6}/, 'X-Amz-Signature=000000'));
    check('a tampered signature is refused by the store', tampered.status === 403, tampered.status);
    const otherKey = await fetch(loc.replace(id, goneId));
    check('the signature is bound to the object key', otherKey.status === 403, otherKey.status);
    await setPublicFiles(false);
    const denied2 = await fetch(`${c.url}/assets/${id}`, { redirect: 'manual', headers: { Authorization: `Bearer ${nobody}` } });
    const deniedAnon2 = await fetch(`${c.url}/assets/${id}`, { redirect: 'manual' });
    check('presign never skips the permission check (no signed URL for someone without access)', denied2.status === 403 && !denied2.headers.get('location') && [401, 403].includes(deniedAnon2.status) && !deniedAnon2.headers.get('location'), `${denied2.status} ${deniedAnon2.status}`);
    await setPublicFiles(true);
    const pub2 = await fetch(`${c.url}/assets/${id}`, { redirect: 'manual' });
    check('public files redirect to the signed URL too', pub2.status === 307, pub2.status);
    // transformations are produced by the server from the object, never by redirecting to the bucket
    const pic = await upload(quadrants(200, 100), 'pic.png', 'image/png');
    const small = await asset(pic.json.data.id + '?width=50');
    const smallImg = small.status === 200 ? decodePng(Buffer.from(await small.arrayBuffer())) : null;
    check('a transformed image from the bucket is served by Ferro (200, resized), not redirected', !!smallImg && smallImg.width === 50 && smallImg.height === 25 && !small.headers.get('location'), small.status);
    check('…while the original of the same image still redirects', (await asset(pic.json.data.id)).status === 307);
    await c.rest('DELETE', `/files/${pic.json.data.id}`); // keep the file counts the later checks rely on
    const dl = await fetch(`${c.url}/assets/${id}?download=1`, { redirect: 'manual', headers: { Authorization: `Bearer ${c.token}` } });
    const dlRes = await fetch(dl.headers.get('location') || endpoint);
    check('?download makes the signed URL an attachment', dl.status === 307 && /^attachment/.test(dlRes.headers.get('content-disposition') || ''), dlRes.headers.get('content-disposition'));

    // ---- several locations: new default is the disk, old files still come from the bucket ------------------------------
    await c.restart({ FERRO_STORAGE_S3_PRESIGN: '', FERRO_STORAGE_DEFAULT: 'local' });
    const loc2 = await upload('on the disk', 'disk.txt');
    check('after switching the default, new files go to the disk', loc2.json.data?.storage === 'local', loc2.json.data?.storage);
    check('old files are still served from the bucket', (await (await asset(id)).text()) === 'second version!');
    const lst = tool(['list']).out;
    check('`storage list` counts files per location', /^local\s+local\s+1 files/m.test(lst) && /^s3\s+s3\s+2 files/m.test(lst), lst);

    // ---- migration -----------------------------------------------------------------------------------------------------------
    const dry = tool(['migrate', '--from', 's3', '--to', 'local', '--dry-run']);
    check('migrate --dry-run changes nothing', dry.code === 0 && /2 file\(s\) in `s3`/.test(dry.out) && /^s3\s+s3\s+2 files/m.test(tool(['list']).out), dry.out);
    const mig = tool(['migrate', '--from', 's3', '--to', 'local', '--delete-source']);
    check('migrate s3 → local moves every file', mig.code === 0 && /moved 2, failed 0/.test(mig.out), mig.out);
    check('…rows now say local and the bytes are identical', (await c.rest('GET', `/files/${id}`)).data.storage === 'local' && (await (await asset(id)).text()) === 'second version!');
    const bigAfter = Buffer.from(await (await asset(bigUp.json.data.id)).arrayBuffer());
    check('…including the 12 MB file (checksum verified)', sha(bigAfter) === sha(big));
    if (filer) check('…and the source objects were deleted', (await inBucket(id)) === false);
    check('migrating a location onto itself is refused', tool(['migrate', '--from', 'local', '--to', 'local']).code !== 0);
    check('migrating from an unknown location is refused', tool(['migrate', '--from', 'nope', '--to', 'local']).code !== 0);
    const chk = tool(['check', '--exit-code']);
    check('`storage check` after the migration: nothing missing', chk.code === 0, chk.out.slice(-150));

    // ---- missing binary is reported ------------------------------------------------------------------------------------------
    await c.restart({ FERRO_STORAGE_DEFAULT: 's3' });
    const lost = await upload('about to be lost', 'lost.txt');
    if (filer) {
      await fetch(`${filer}/buckets/${bucket}/${prefix}${lost.json.data.id}`, { method: 'DELETE' });
      check('a binary removed behind our back: asset is 404 (not 500)', (await asset(lost.json.data.id)).status === 404);
      const chk2 = tool(['check', '--exit-code']);
      check('…and `storage check` reports it (exit 2)', chk2.code === 2 && chk2.out.includes(`MISSING ${lost.json.data.id}`), chk2.out.slice(-200));
    }

    // ---- bucket configuration errors stop the server at startup -------------------------------------------------------------
    const bad1 = tool(['check'], { FERRO_STORAGE_S3_BUCKET: 'no-such-bucket-xyz' });
    check('unknown bucket: refused at startup with a clear message', bad1.code !== 0 && /storage location `s3` is not usable/.test(bad1.out), bad1.out.slice(-200));
    const bad2 = tool(['check'], { FERRO_STORAGE_S3_SECRET_ACCESS_KEY: 'wrong-secret' });
    check('wrong credentials: refused at startup', bad2.code !== 0 && /not usable/.test(bad2.out), bad2.out.slice(-200));
    const bad3 = tool(['check'], { FERRO_STORAGE_DEFAULT: 'ghost' });
    check('default location not in the list: refused', bad3.code !== 0 && /FERRO_STORAGE_DEFAULT/.test(bad3.out), bad3.out.slice(-200));

    // ---- promotion between environments with different storage ------------------------------------------------------------
    const f2 = await upload('cover from the bucket ' + rnd(), 'cover.txt');
    const page = (await c.rest('POST', '/items/pages', { title: 'Cover', slug: `cv-${rnd()}`, cover: f2.json.data.id })).data;
    const push = sh(['data', 'push', '--from', 'c', '--to', 'b', '--select', `pages:id=${page.id}`, '--yes', '--ignore-missing-fields']);
    check('push from the S3 environment to the disk one transfers the binary', push.code === 0 && /transferred 1 file/.test(push.out), push.out.slice(-250));
    const onB = await fetch(`${b.url}/assets/${f2.json.data.id}`, { headers: { Authorization: `Bearer ${b.token}` } });
    check('…and the target serves it (row says local there)', onB.status === 200 && (await onB.text()).startsWith('cover from the bucket') && (await b.rest('GET', `/files/${f2.json.data.id}`)).data.storage === 'local');
    const pushBack = sh(['data', 'push', '--from', 'b', '--to', 'c', '--select', `pages:id=${page.id}`, '--yes', '--ignore-missing-fields']);
    check('and the other way, local → bucket (idempotent)', pushBack.code === 0, pushBack.out.slice(-200));
    check('…the file in the S3 environment is still readable', (await (await asset(f2.json.data.id)).text()).startsWith('cover from the bucket'));
  } finally {
    await c.stop();
  }
}
