// Promotion of content between two environments: M2M, natural keys, prune, strategies, bundles.
import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { cli, rnd } from './lib.mjs';

export default async function ({ a, b, check }) {
  const sh = (args) => cli(args, { FERRO_A_TOKEN: a.token, FERRO_B_TOKEN: b.token });
  const posts = `qs_posts_${rnd()}`, tags = `qs_tags_${rnd()}`, junction = `${posts}_tags`;

  // model on A: posts <-m2m-> tags, natural keys on both sides
  await a.rest('POST', '/collections', { name: posts, sync: { natural_key: ['slug'] }, fields: [{ name: 'title', type: 'string' }, { name: 'slug', type: 'string', unique: true }] });
  await a.rest('POST', '/collections', { name: tags, sync: { natural_key: ['name'] }, fields: [{ name: 'name', type: 'string', required: true, unique: true }] });
  await a.rest('POST', '/collections', {
    name: junction, system_fields: false,
    fields: [
      { name: 'posts_id', type: 'm2o', relation: { related: posts, on_delete: 'cascade' } },
      { name: 'tags_id', type: 'm2o', relation: { related: tags, on_delete: 'cascade' } },
    ],
  });
  const add = await a.rest('POST', `/fields/${posts}`, { name: 'tags', type: 'm2m', relation: { related: junction, foreign_field: 'posts_id', junction_field: 'tags_id' } });
  check('model with an M2M relation built on A', add.status === 200, add.error);

  // schema promotion A → B through the CLI
  let r = sh(['schema', 'diff', '--from', 'a', '--to', 'b', '--exit-code']);
  check('schema diff A→B finds the new collections (exit 2)', r.code === 2 && r.out.includes(`+ collection ${posts}`), r.out.split('\n').slice(0, 3).join('|'));
  r = sh(['schema', 'apply', '--from', 'a', '--to', 'b', '--yes']);
  check('schema apply A→B', r.code === 0, r.out.slice(-200));
  check('schemas are now identical', sh(['schema', 'diff', '--from', 'a', '--to', 'b', '--exit-code']).code === 0);

  // data on A
  const news = (await a.rest('POST', `/items/${tags}`, { name: 'news' })).data;
  const tech = (await a.rest('POST', `/items/${tags}`, { name: 'tech' })).data;
  const post = (await a.rest('POST', `/items/${posts}`, { title: 'First', slug: 'first', tags: [{ tags_id: news.id }, { tags_id: tech.id }] })).data;
  const sel = ['--select', `${posts}:{"slug":{"_eq":"first"}}`];

  r = sh(['data', 'diff', '--from', 'a', '--to', 'b', ...sel]);
  check('data diff (dry run) lists what would be created', r.out.includes('DRY RUN') && new RegExp(`${junction}\\s+2`).test(r.out) && new RegExp(`${tags}\\s+2`).test(r.out), r.out);
  check('…and wrote nothing', (await b.rest('GET', `/items/${posts}`)).data.length === 0);

  r = sh(['data', 'push', '--from', 'a', '--to', 'b', ...sel, '--yes']);
  check('push follows the M2M and brings tags and junction rows', r.code === 0 && r.out.includes('APPLIED'), r.out);
  const onB = (await b.rest('GET', `/items/${posts}?fields=id,title,tags.tags_id`)).data;
  check('same ids in both environments', onB.length === 1 && onB[0].id === post.id && onB[0].tags.length === 2);
  check('tags arrived with their ids', (await b.rest('GET', `/items/${tags}/${news.id}?fields=name`)).data?.name === 'news');
  check('second push changes nothing', sh(['data', 'diff', '--from', 'a', '--to', 'b', ...sel, '--exit-code']).code === 0);

  // natural key: B already has "extra" under another id
  const extraB = (await b.rest('POST', `/items/${tags}`, { name: 'extra' })).data;
  const extraA = (await a.rest('POST', `/items/${tags}`, { name: 'extra' })).data;
  const post2 = (await a.rest('POST', `/items/${posts}`, { title: 'Second', slug: 'second', tags: [{ tags_id: extraA.id }] })).data;
  check('premise: same business key, different ids', extraA.id !== extraB.id);
  r = sh(['data', 'push', '--from', 'a', '--to', 'b', '--select', `${posts}:{"slug":{"_eq":"second"}}`, '--yes']);
  check('push with a natural-key collision succeeds and reports the remap', r.code === 0 && /natural key/.test(r.out), r.out);
  const p2b = (await b.rest('GET', `/items/${posts}/${post2.id}?fields=tags.tags_id`)).data;
  check('the junction points to the id that B already had', p2b?.tags?.[0]?.tags_id === extraB.id, p2b);
  check('no duplicate tag was created on B', (await b.rest('GET', `/items/${tags}?filter=${encodeURIComponent(JSON.stringify({ name: { _eq: 'extra' } }))}`)).data.length === 1);

  // prune: remove a tag from the post on A
  await a.rest('PATCH', `/items/${posts}/${post.id}`, { tags: [{ tags_id: news.id }] });
  sh(['data', 'push', '--from', 'a', '--to', 'b', ...sel, '--yes']);
  // (an array without row ids replaces the rows on A: the new junction row has a new id, so B holds 2 old + 1 new)
  check('without --prune the junction rows removed on A stay on B', (await b.rest('GET', `/items/${posts}/${post.id}?fields=tags.tags_id`)).data.tags.length === 3);
  r = sh(['data', 'diff', '--from', 'a', '--to', 'b', ...sel]); // a diff always includes what --prune would delete
  check('diff with --prune announces the deletions of the stale rows', new RegExp(`${junction}\\s+\\d+\\s+\\d+\\s+2\\b`).test(r.out), r.out.split('\n').slice(0, 8).join('|'));
  r = sh(['data', 'push', '--from', 'a', '--to', 'b', ...sel, '--prune', '--yes']);
  check('--prune removes it', r.code === 0 && (await b.rest('GET', `/items/${posts}/${post.id}?fields=tags.tags_id`)).data.tags.length === 1, r.out);
  check('the tag itself is kept (only owned children are pruned)', (await b.rest('GET', `/items/${tags}/${tech.id}`)).status === 200);

  // strategies
  await a.rest('PATCH', `/items/${posts}/${post.id}`, { title: 'First (edited)' });
  r = sh(['data', 'push', '--from', 'a', '--to', 'b', ...sel, '--strategy', 'insert-only', '--yes']);
  check('insert-only leaves existing items alone', r.code === 0 && (await b.rest('GET', `/items/${posts}/${post.id}?fields=title`)).data.title === 'First', r.out);
  sh(['data', 'push', '--from', 'a', '--to', 'b', ...sel, '--yes']);
  check('overwrite updates it', (await b.rest('GET', `/items/${posts}/${post.id}?fields=title`)).data.title === 'First (edited)');
  await b.rest('PATCH', `/items/${posts}/${post.id}`, { title: 'Edited on B' });
  await a.rest('PATCH', `/items/${posts}/${post.id}`, { title: 'Edited on A' });
  r = sh(['data', 'push', '--from', 'a', '--to', 'b', ...sel, '--strategy', 'safe', '--yes']);
  check('safe strategy: a change made on B since the last sync is a conflict (exit 1)', r.code === 1 && /conflict/.test(r.out) && (await b.rest('GET', `/items/${posts}/${post.id}?fields=title`)).data.title === 'Edited on B', r.out.split('\n').slice(0, 6).join('|'));
  r = sh(['data', 'push', '--from', 'a', '--to', 'b', ...sel, '--strategy', 'safe', '--force', '--yes']);
  check('--force overwrites the conflicting item', r.code === 0 && (await b.rest('GET', `/items/${posts}/${post.id}?fields=title`)).data.title === 'Edited on A');

  // fields that exist only on A
  await a.rest('POST', `/fields/${posts}`, { name: 'subtitle', type: 'string' });
  await a.rest('PATCH', `/items/${posts}/${post.id}`, { subtitle: 'only on A' });
  r = sh(['data', 'push', '--from', 'a', '--to', 'b', ...sel, '--yes']);
  check('a field missing on the target stops the import with a clear error', r.code === 1 && /subtitle: missing in target/.test(r.out), r.out.split('\n').slice(-4).join('|'));
  r = sh(['data', 'push', '--from', 'a', '--to', 'b', ...sel, '--ignore-missing-fields', '--yes']);
  check('--ignore-missing-fields imports the rest', r.code === 0 && r.out.includes('ignored'), r.out);

  // bundle on disk: export, damage the target, import
  const dir = mkdtempSync(join(tmpdir(), 'ferro-bundle-'));
  r = sh(['data', 'export', '--from', 'a', ...sel, '--out', dir]);
  check('export to a directory', r.code === 0 && JSON.parse(readFileSync(join(dir, 'bundle.json'), 'utf8')).format === 'ferro-bundle/1');
  await b.rest('DELETE', `/items/${posts}/${post.id}`);
  r = sh(['data', 'import', '--to', 'b', '--bundle', dir, '--ignore-missing-fields', '--yes']);
  check('import from the directory restores the item', r.code === 0 && (await b.rest('GET', `/items/${posts}/${post.id}?fields=title`)).data?.title === 'Edited on A', r.out);
  const tampered = JSON.parse(readFileSync(join(dir, 'bundle.json'), 'utf8'));
  tampered.format = 'ferro-bundle/99';
  writeFileSync(join(dir, 'bundle.json'), JSON.stringify(tampered));
  r = sh(['data', 'import', '--to', 'b', '--bundle', dir, '--yes']);
  check('an unknown bundle format is refused', r.code !== 0, r.out.slice(-120));

  // protection and permissions
  await a.rest('PATCH', `/items/${posts}/${post.id}`, { title: 'needs confirmation' });
  r = cli(['data', 'push', '--from', 'a', '--to', 'b', ...sel, '--ignore-missing-fields'], { FERRO_A_TOKEN: a.token, FERRO_B_TOKEN: b.token });
  check('protected environment: refuses without --yes in a non-interactive shell', r.code !== 0 && /protected/.test(r.out), r.out.slice(-150));
  const ed = (await a.rest('POST', '/auth/login', { email: 'nobody@example.com', password: 'x' }, null)).status;
  check('anonymous sync endpoints are closed', ed === 401 && (await a.rest('POST', '/sync/export', { selections: [{ collection: posts }] }, null)).status === 403);

  // file binaries travel with their metadata
  const fd = new FormData();
  fd.append('file', new Blob(['binary-' + rnd()], { type: 'text/plain' }), 'doc.txt');
  const file = await (await fetch(`${a.url}/files`, { method: 'POST', headers: { Authorization: `Bearer ${a.token}` }, body: fd })).json();
  const pg = (await a.rest('POST', '/items/pages', { title: 'With cover', slug: `cover-${rnd()}`, cover: file.data.id })).data;
  r = sh(['data', 'push', '--from', 'a', '--to', 'b', '--select', `pages:id=${pg.id}`, '--yes']);
  check('a page with a file: push transfers the binary', r.code === 0 && /transferred 1 file/.test(r.out), r.out);
  const asset = await fetch(`${b.url}/assets/${file.data.id}`, { headers: { Authorization: `Bearer ${b.token}` } });
  check('the binary is served by the target', (await asset.text()).startsWith('binary-'));
}
