// Image transformations on /assets/:id: resize, fit modes, crop, formats, quality, validation, permissions,
// HTTP caching (ETag / 304), limits against abuse, concurrency.
import { quadrants, makePng, bombPng, decodePng, jpegSize, webpSize, near } from './png.mjs';
import { rnd } from './lib.mjs';

const RED = [255, 0, 0], GREEN = [0, 255, 0], BLUE = [0, 0, 255], YELLOW = [255, 255, 0];

export default async function ({ a, check }) {
  const upload = async (content, name, type) => {
    const fd = new FormData();
    fd.append('file', new Blob([content], { type }), name);
    const res = await fetch(`${a.url}/files`, { method: 'POST', headers: { Authorization: `Bearer ${a.token}` }, body: fd });
    return (await res.json()).data;
  };
  const get = (id, query = '', headers = {}, token = a.token) =>
    fetch(`${a.url}/assets/${id}${query}`, { headers: { ...(token ? { Authorization: `Bearer ${token}` } : {}), ...headers } });
  const bytes = async (res) => Buffer.from(await res.arrayBuffer());
  const png = async (id, query) => {
    const res = await get(id, query);
    return { res, img: res.status === 200 ? decodePng(await bytes(res)) : null };
  };

  // ---- metadata recorded at upload ---------------------------------------------------------------------------------
  const src = quadrants(400, 200);
  const file = await upload(src, 'quadrants.png', 'image/png');
  check('upload of an image records its width and height', file.width === 400 && file.height === 200, [file.width, file.height]);
  const txt = await upload('just text', 'note.txt', 'text/plain');
  const svg = await upload('<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"/>', 'logo.svg', 'image/svg+xml');
  check('non-images (text, SVG) have no dimensions', txt.width === null && svg.width === null);
  const original = await bytes(await get(file.id));
  check('without parameters the original bytes are served untouched', original.equals(src));

  // ---- resize --------------------------------------------------------------------------------------------------------
  let r = await png(file.id, '?width=100');
  check('width only: ratio kept (100x50), PNG stays PNG', r.res.status === 200 && r.res.headers.get('content-type') === 'image/png' && r.img.width === 100 && r.img.height === 50, r.res.status);
  check('…and the pixels are the right colours', near(r.img.at(10, 10), RED) && near(r.img.at(90, 10), GREEN) && near(r.img.at(10, 40), BLUE) && near(r.img.at(90, 40), YELLOW));
  r = await png(file.id, '?height=20');
  check('height only', r.img.width === 40 && r.img.height === 20);
  for (const [fit, w, h] of [['cover', 100, 100], ['contain', 100, 100], ['fill', 100, 100], ['inside', 100, 50], ['outside', 200, 100]]) {
    r = await png(file.id, `?width=100&height=100&fit=${fit}`);
    check(`fit=${fit} → ${w}x${h}`, r.img && r.img.width === w && r.img.height === h, r.img && [r.img.width, r.img.height]);
  }
  r = await png(file.id, '?width=30&height=60');
  check('both sizes without fit means cover', r.img.width === 30 && r.img.height === 60);
  r = await png(file.id, '?width=800');
  check('enlarging is allowed by default', r.img.width === 800 && r.img.height === 400);
  r = await png(file.id, '?width=800&withoutEnlargement=true');
  check('withoutEnlargement keeps the original size', r.img.width === 400 && r.img.height === 200, r.img && [r.img.width, r.img.height]);
  r = await png(file.id, '?width=800&without_enlargement=1');
  check('…also spelled without_enlargement', r.img.width === 400);

  // ---- cover position / crop ---------------------------------------------------------------------------------------
  r = await png(file.id, '?width=100&height=100&position=left');
  check('cover, position=left keeps the left side (red over blue)', near(r.img.at(10, 10), RED) && near(r.img.at(10, 90), BLUE));
  r = await png(file.id, '?width=100&height=100&position=right');
  check('cover, position=right keeps the right side (green over yellow)', near(r.img.at(90, 10), GREEN) && near(r.img.at(90, 90), YELLOW));
  r = await png(file.id, '?crop=200,100,200,100');
  check('explicit crop of the yellow quadrant', r.img.width === 200 && r.img.height === 100 && near(r.img.at(5, 5), YELLOW) && near(r.img.at(195, 95), YELLOW));
  r = await png(file.id, '?crop=0,0,200,100&width=50');
  check('crop first, then resize', r.img.width === 50 && r.img.height === 25 && near(r.img.at(25, 12), RED));
  r = await png(file.id, '?width=100&height=100&fit=contain&background=000000');
  check('contain pads with the background colour', near(r.img.at(50, 3), [0, 0, 0], 5) && near(r.img.at(10, 40), RED));
  r = await png(file.id, '?width=100&height=100&fit=contain');
  check('contain on a PNG pads with transparency by default', r.img.channels === 4 && r.img.at(50, 3)[3] === 0);

  // ---- formats and quality -----------------------------------------------------------------------------------------
  let res = await get(file.id, '?width=100&format=jpg');
  let buf = await bytes(res);
  check('format=jpg: JPEG bytes, content type, size', res.headers.get('content-type') === 'image/jpeg' && jpegSize(buf).width === 100 && jpegSize(buf).height === 50);
  check('…the file name follows the format', /filename="quadrants\.jpg"/.test(res.headers.get('content-disposition')), res.headers.get('content-disposition'));
  const jpegFile = await upload(buf, 'derived.jpg', 'image/jpeg');
  check('the generated JPEG is a valid source (dimensions recorded)', jpegFile.width === 100 && jpegFile.height === 50);
  res = await get(jpegFile.id, '?width=20');
  check('a JPEG stays a JPEG when only resized', res.headers.get('content-type') === 'image/jpeg' && jpegSize(await bytes(res)).width === 20);
  res = await get(file.id, '?width=120&format=webp');
  buf = await bytes(res);
  check('format=webp', res.headers.get('content-type') === 'image/webp' && webpSize(buf).width === 120 && webpSize(buf).height === 60);
  const webpFile = await upload(buf, 'derived.webp', 'image/webp');
  res = await get(webpFile.id, '?width=40&format=png');
  r = decodePng(await bytes(res));
  check('WebP → PNG round trip keeps the colours', r.width === 40 && near(r.at(5, 5), RED) && near(r.at(35, 15), YELLOW) && near(r.at(35, 5), GREEN));
  const photo = await upload(makePng(300, 300, (x, y) => [(x * 7 + y * 3) & 255, (x * y) & 255, (x ^ y) & 255, 255]), 'noise.png', 'image/png');
  const hi = await bytes(await get(photo.id, '?format=jpg&quality=95'));
  const lo = await bytes(await get(photo.id, '?format=jpg&quality=10'));
  check('quality changes the JPEG size', hi.length > lo.length * 2, [hi.length, lo.length]);
  const semi = await upload(quadrants(40, 40, 128), 'glass.png', 'image/png');
  res = await get(semi.id, '?format=jpg');
  check('transparent PNG → JPEG is flattened onto white', res.status === 200 && jpegSize(await bytes(res)).width === 40);
  const asDownload = await get(file.id, '?width=50&format=jpg&download=1');
  check('?download with a transformation: attachment with the new extension', /^attachment; filename="quadrants\.jpg"/.test(asDownload.headers.get('content-disposition')), asDownload.headers.get('content-disposition'));

  // ---- invalid requests ------------------------------------------------------------------------------------------------
  for (const [query, why] of [
    ['?width=0', 'width 0'], ['?width=-3', 'negative width'], ['?width=abc', 'non-numeric width'], ['?width=99999', 'width above the limit'],
    ['?height=1.5', 'fractional height'], ['?width=10&fit=stretch', 'unknown fit'], ['?fit=cover', 'fit without a size'],
    ['?width=10&format=avif', 'unsupported format'], ['?width=10&quality=0', 'quality 0'], ['?format=jpg&quality=101', 'quality 101'],
    ['?crop=1,2,3', 'crop with 3 numbers'], ['?crop=0,0,0,10', 'empty crop'], ['?crop=300,0,200,10', 'crop outside the image'],
    ['?crop=4294967295,0,10,10', 'crop overflowing an integer'], ['?width=10&background=red', 'background that is not hex'],
    ['?width=10&position=middle', 'unknown position'], ['?width=10&position=top', 'position needs both sizes'],
  ]) {
    const bad = await get(file.id, query);
    const body = await bad.json().catch(() => null);
    check(`rejected with 400: ${why}`, bad.status === 400 && !!body?.errors?.[0]?.message, `${bad.status} ${body?.errors?.[0]?.message ?? ''}`);
  }
  for (const [what, id] of [['a text file', txt.id], ['an SVG', svg.id]]) {
    const bad = await get(id, '?width=10');
    check(`transforming ${what} is refused (400)`, bad.status === 400, bad.status);
    check(`…but ${what} is still served as is`, (await get(id)).status === 200);
  }
  const corrupt = await upload(quadrants(60, 60).subarray(0, 120), 'cut.png', 'image/png');
  const cut = await get(corrupt.id, '?width=10');
  check('a truncated image gives 400, not a crash', cut.status === 400, cut.status);
  const bomb = await upload(bombPng(30000, 30000), 'bomb.png', 'image/png');
  const t0 = Date.now();
  const bombRes = await get(bomb.id, '?width=10');
  const bombBody = await bombRes.json().catch(() => null);
  check('a decompression bomb (30000x30000 header) is refused from the header', bombRes.status === 400 && /too large/.test(bombBody?.errors?.[0]?.message ?? '') && Date.now() - t0 < 3000, `${bombRes.status} ${bombBody?.errors?.[0]?.message} ${Date.now() - t0}ms`);
  const needle = await upload(makePng(1, 3000, () => [10, 20, 30, 255]), 'needle.png', 'image/png');
  const huge = await get(needle.id, '?width=4096');
  const hugeBody = await huge.json().catch(() => null);
  check('an extreme aspect ratio cannot blow up memory: width=4096 on 1x3000 is refused up front (400)', huge.status === 400 && /pixels per side/.test(hugeBody?.errors?.[0]?.message ?? ''), `${huge.status} ${hugeBody?.errors?.[0]?.message}`);
  r = await png(needle.id, '?width=50&height=50');
  check('…while a normal request on the same thin image works (cover, 50x50)', r.img && r.img.width === 50 && r.img.height === 50, r.res.status);
  check('the server is still healthy afterwards', (await fetch(`${a.url}/server/health`)).ok);
  check('unknown file: 404 even with parameters', (await get('0199a000-0000-7000-8000-00000000dead', '?width=10')).status === 404);

  // ---- permissions ---------------------------------------------------------------------------------------------------------
  const policy = (await a.rest('GET', '/policies')).data.find((p) => p.public);
  const putPolicy = (permissions) => a.rest('PUT', `/policies/${policy.id}`, { ...policy, permissions });
  const without = (policy.permissions || []).filter((p) => !(p.collection === 'ferro_files' && p.action === 'read'));
  try {
    check('anonymous can transform public files (example public policy)', (await get(file.id, '?width=20', {}, null)).status === 200);
    await putPolicy(without);
    const anon = await get(file.id, '?width=20', {}, null);
    check('without read permission a transformation is refused too (403) and reveals nothing', anon.status === 403 && !(await bytes(anon)).includes(Buffer.from('PNG')), anon.status);
    check('…also with an ETag the caller could have seen before', (await get(file.id, '?width=20', { 'If-None-Match': '"x"' }, null)).status === 403);
  } finally {
    await putPolicy(policy.permissions);
  }

  // ---- HTTP caching -----------------------------------------------------------------------------------------------------------
  res = await get(file.id, '?width=77&format=jpg');
  const etag = res.headers.get('etag');
  check('a transformation has its own ETag, different from the original', !!etag && etag !== (await get(file.id)).headers.get('etag'), etag);
  const again = await get(file.id, '?width=77&format=jpg', { 'If-None-Match': etag });
  check('If-None-Match → 304 without a body', again.status === 304 && (await bytes(again)).length === 0 && again.headers.get('etag') === etag);
  const weak = await get(file.id, '?width=77&format=jpg', { 'If-None-Match': `W/${etag}, "other"` });
  check('weak and multiple validators are understood', weak.status === 304);
  const same = await get(file.id, '?format=JPEG&width=77');
  check('equivalent spellings share the ETag (normalised parameters)', same.headers.get('etag') === etag, [same.headers.get('etag'), etag]);
  const other = await get(file.id, '?width=78&format=jpg');
  check('different parameters, different ETag', other.headers.get('etag') !== etag);
  const orig = await get(file.id);
  const origTag = orig.headers.get('etag');
  check('the original supports 304 as well', (await get(file.id, '', { 'If-None-Match': origTag })).status === 304);
  const rep = new FormData();
  rep.append('file', new Blob([quadrants(400, 200, 200)], { type: 'image/png' }), 'quadrants.png');
  await fetch(`${a.url}/files?replace=${file.id}`, { method: 'POST', headers: { Authorization: `Bearer ${a.token}` }, body: rep });
  const stale = await get(file.id, '?width=77&format=jpg', { 'If-None-Match': etag });
  check('after the binary is replaced the old ETag no longer matches (fresh image)', stale.status === 200 && stale.headers.get('etag') !== etag);
  const a1 = await bytes(await get(file.id, '?width=64')), a2 = await bytes(await get(file.id, '?width=64'));
  check('repeated requests return identical bytes (cache or not)', a1.equals(a2));

  // ---- load: concurrent distinct transformations -------------------------------------------------------------------------------------
  const big = await upload(makePng(900, 600, (x, y) => [(x * 255 / 900) | 0, (y * 255 / 600) | 0, (x + y) & 255, 255]), `big-${rnd()}.png`, 'image/png');
  const t1 = Date.now();
  const many = await Promise.all(Array.from({ length: 24 }, (_, i) => get(big.id, `?width=${100 + i * 7}&format=${i % 2 ? 'jpg' : 'webp'}`)));
  const okAll = many.every((m) => m.status === 200);
  const sizes = await Promise.all(many.map(async (m, i) => (i % 2 ? jpegSize(await bytes(m)).width : webpSize(await bytes(m)).width)));
  check('24 concurrent distinct transformations all succeed with the right sizes', okAll && sizes.every((w, i) => w === 100 + i * 7), `${Date.now() - t1}ms`);
  check('the server is responsive after the burst', (await fetch(`${a.url}/server/health`)).ok);
}
