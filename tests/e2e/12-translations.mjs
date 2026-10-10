// The dedicated `translations` field type: guided set-up, the database guarantee (one translation per language and item),
// writing by language code, reading with `lang` and a fallback chain, permissions, and promotion between environments.
import { cli, rnd, uuid } from './lib.mjs';
import { sql } from './identity.mjs';

export default async function ({ a, b, check }) {
  const tag = rnd();
  const art = `tr_art_${tag}`, langs = `tr_lang_${tag}`, trc = `${art}_translations`;
  const parentField = `${art}_id`, langField = `${langs}_id`;
  const sh = (args) => cli(args, { FERRO_A_TOKEN: a.token, FERRO_B_TOKEN: b.token });
  const q = (s) => sql('ferro_e2e_a', s);
  const scaffold = (body, query = '', token) => a.rest('POST', `/collections/${art}/translations${query}`, body, token);
  const translatable = [{ name: 'title', type: 'string' }, { name: 'body', type: 'text' }, { name: 'summary', type: 'text' }];
  const spec = { fields: translatable, languages_collection: langs, languages: [{ code: 'it-IT', name: 'Italiano' }, { code: 'en-US', name: 'English' }, { code: 'fr-FR', name: 'Français' }] };

  // ============================================================================================= guided set-up
  await a.rest('POST', '/collections', { name: art, sync: { natural_key: ['slug'] }, fields: [{ name: 'slug', type: 'string', required: true, unique: true }, { name: 'status', type: 'string' }] });

  const empty = await scaffold({ ...spec, fields: [] });
  check('a translatable-fields list is required (400)', empty.status === 400 && /at least one/.test(empty.error), empty.error);
  const ghost = await a.rest('POST', `/collections/nope_${tag}/translations`, spec);
  check('an unknown collection is refused (400)', ghost.status === 400 && /does not exist/.test(ghost.error), ghost.error);
  check('an alias cannot be a translatable field', (await scaffold({ ...spec, fields: [{ name: 'x', type: 'o2m' }] })).status === 400);
  check('a unique field cannot be translatable', (await scaffold({ ...spec, fields: [{ name: 'x', type: 'string', unique: true }] })).status === 400);
  check('a field clashing with the link fields is refused', (await scaffold({ ...spec, fields: [{ name: parentField, type: 'string' }] })).status === 400);
  check('non-admins cannot set translations up', (await scaffold(spec, '', 'bogus')).status === 401);

  const dry = await scaffold(spec, '?dry_run=true');
  check('dry run: previews the plan and the SQL without changing anything', dry.status === 200 && dry.json.meta.applied === false && dry.json.meta.changes.some((c) => c.includes(trc)) && dry.json.meta.sql.some((s) => s.includes('UNIQUE')), dry.json?.meta?.changes);
  check('…nothing was created', (await a.rest('GET', `/collections/${trc}`)).status === 404 && (await a.rest('GET', `/collections/${langs}`)).status === 404);

  const made = await scaffold(spec);
  check('one call creates the languages, the translations collection and the field', made.status === 200 && made.json.meta.applied === true && made.data.field === 'translations' && made.data.translations_collection === trc, made.json);
  check('…and seeds the languages', JSON.stringify(made.json.meta.languages.added) === '["it-IT","en-US","fr-FR"]', made.json.meta.languages);
  const field = (await a.rest('GET', `/collections/${art}`)).data.fields.find((f) => f.name === 'translations');
  check('the field has the dedicated type and knows its language field', field.type === 'translations' && field.relation.related === trc && field.relation.foreign_field === parentField && field.relation.language_field === langField, field);
  const lf = (await a.rest('GET', `/collections/${trc}`)).data.fields.find((f) => f.name === langField);
  check('the language field is unique together with the link to the item', lf.unique === true && lf.required === true && lf.unique_with.join() === parentField, lf);
  const constraint = q(`SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = '"${trc}"'::regclass AND contype = 'u'`)[0]?.[0] ?? '';
  check('…and the database has the composite UNIQUE constraint', constraint === `UNIQUE (${langField}, ${parentField})`, constraint);
  check('the alias field has no column of its own', q(`SELECT count(*) FROM information_schema.columns WHERE table_name = '${art}' AND column_name = 'translations'`)[0][0] === '0');
  const again = await scaffold(spec);
  check('setting it up twice is refused', again.status === 400 && /already/.test(again.error), again.error);

  // ============================================================================================ the guarantee
  const base = (await a.rest('POST', `/items/${art}`, { slug: 'base', status: 'published' })).data;
  const lang = (code) => q(`SELECT id FROM "${langs}" WHERE code = '${code}'`)[0][0];
  const row = (parent, code, extra = {}) => a.rest('POST', `/items/${trc}`, { [parentField]: parent, [langField]: lang(code), ...extra });
  check('a translation row is created through the normal API', (await row(base.id, 'it-IT', { title: 'Base' })).status === 200);
  const dupe = await row(base.id, 'it-IT', { title: 'Again' });
  check('a second translation of the same language and item is refused by the database (409)', dupe.status === 409 && dupe.code === 'RECORD_NOT_UNIQUE', dupe.json);
  check('…while the same language on another item is fine', (await row((await a.rest('POST', `/items/${art}`, { slug: 'other' })).data.id, 'it-IT', { title: 'Other' })).status === 200);
  check('…and the same item in another language is fine', (await row(base.id, 'en-US', { title: 'Base EN' })).status === 200);
  const viaArray = await a.rest('PATCH', `/items/${art}/${base.id}`, { translations: [{ [langField]: lang('en-US'), title: 'dup via array' }] });
  check('the nested array form cannot create a duplicate either', viaArray.status === 409 || viaArray.status === 400, viaArray.status);
  check('a translation must have both links (NOT NULL)', (await a.rest('POST', `/items/${trc}`, { title: 'orphan' })).status === 400);
  const parallel = await Promise.all(Array.from({ length: 8 }, () => row(base.id, 'fr-FR', { title: 'race' })));
  check('eight simultaneous creations of one translation leave exactly one', parallel.filter((r) => r.status === 200).length === 1 && parallel.every((r) => [200, 409].includes(r.status)) && q(`SELECT count(*) FROM "${trc}" WHERE "${parentField}" = '${base.id}' AND "${langField}" = '${lang('fr-FR')}'`)[0][0] === '1', parallel.map((r) => r.status));

  // ====================================================================================== writing by language code
  const created = await a.rest('POST', `/items/${art}`, { slug: 'alpha', status: 'published', translations: { 'it-IT': { title: 'Ciao', body: 'Corpo italiano' }, 'en-US': { title: 'Hello' } } });
  check('create an item with its translations keyed by language code', created.status === 200, created.json);
  const alpha = created.data.id;
  const rows = () => a.rest('GET', `/items/${trc}?filter=${encodeURIComponent(JSON.stringify({ [parentField]: { _eq: alpha } }))}&fields=${langField}.code,title,body,summary&limit=-1`);
  const byCode = async () => Object.fromEntries((await rows()).data.map((r) => [r[langField].code, r]));
  let t = await byCode();
  check('…two translation rows with the right languages and values', Object.keys(t).sort().join() === 'en-US,it-IT' && t['it-IT'].title === 'Ciao' && t['it-IT'].body === 'Corpo italiano' && t['en-US'].title === 'Hello', t);

  await a.rest('PATCH', `/items/${art}/${alpha}`, { translations: { 'it-IT': { title: 'Ciao mondo' } } });
  t = await byCode();
  check('a partial update changes only the language named, and only the fields given', t['it-IT'].title === 'Ciao mondo' && t['it-IT'].body === 'Corpo italiano' && t['en-US'].title === 'Hello', t);
  await a.rest('PATCH', `/items/${art}/${alpha}`, { translations: { 'fr-FR': { title: 'Bonjour' } } });
  t = await byCode();
  check('a language that had none is added, the others untouched', Object.keys(t).sort().join() === 'en-US,fr-FR,it-IT' && t['fr-FR'].title === 'Bonjour');
  await a.rest('PATCH', `/items/${art}/${alpha}`, { translations: { 'en-US': null } });
  t = await byCode();
  check('null removes that language’s translation', Object.keys(t).sort().join() === 'fr-FR,it-IT');
  const same = await a.rest('PATCH', `/items/${art}/${alpha}`, { translations: { 'it-IT': { title: 'Ciao mondo', [parentField]: base.id, [langField]: lang('fr-FR'), id: uuid() } } });
  t = await byCode();
  check('the link fields and the id in a payload are ignored (a translation cannot be moved)', same.status === 200 && t['it-IT'].title === 'Ciao mondo' && Object.keys(t).length === 2, t);
  const unknown = await a.rest('PATCH', `/items/${art}/${alpha}`, { translations: { 'xx-XX': { title: 'nope' } } });
  check('an unknown language is refused (400) and nothing is written', unknown.status === 400 && /unknown language `xx-XX`/.test(unknown.error) && Object.keys(await byCode()).length === 2, unknown.error);
  check('a payload that is not an object is refused', (await a.rest('PATCH', `/items/${art}/${alpha}`, { translations: { 'it-IT': 'text' } })).status === 400);
  const atomic = await a.rest('PATCH', `/items/${art}/${alpha}`, { status: 'draft', translations: { 'it-IT': { title: 'half' }, 'xx-XX': { title: 'bad' } } });
  check('a failing language rolls the whole write back (the item keeps its status and title)', atomic.status === 400 && (await a.rest('GET', `/items/${art}/${alpha}?fields=status`)).data.status === 'published' && (await byCode())['it-IT'].title === 'Ciao mondo');
  const ops = await a.rest('PATCH', `/items/${art}/${alpha}`, { translations: { create: [{ [langField]: lang('en-US'), title: 'via ops' }] } });
  t = await byCode();
  check('the {create, update, delete} form still works', ops.status === 200 && t['en-US']?.title === 'via ops', ops.json);
  const itRow = (await rows()).data.find((x) => x[langField].code === 'it-IT');
  const replaced = await a.rest('PATCH', `/items/${art}/${alpha}`, { translations: [{ id: itRow.id, title: 'Solo italiano' }] });
  t = await byCode();
  check('the array form still replaces the whole set (rows are matched by id)', replaced.status === 200 && Object.keys(t).join() === 'it-IT' && t['it-IT'].title === 'Solo italiano', t);
  await a.rest('PATCH', `/items/${art}/${alpha}`, { translations: { 'it-IT': { title: 'Ciao', body: 'Corpo italiano' }, 'en-US': { title: 'Hello', body: 'English body' }, 'fr-FR': { title: 'Bonjour' } } });

  // ================================================================================================== reading
  const read = (query) => a.rest('GET', `/items/${art}/${alpha}?${query}`);
  let r = await read('fields=slug,title,body&lang=it-IT');
  check('lang: the translated fields come back on the item', r.status === 200 && r.data.title === 'Ciao' && r.data.body === 'Corpo italiano' && r.data.slug === 'alpha' && r.data._language === 'it-IT', r.json);
  r = await read('fields=slug,title&lang=en-US');
  check('…in the language asked for', r.data.title === 'Hello' && r.data._language === 'en-US' && !('body' in r.data));
  r = await read('fields=title,body,summary&lang=fr-FR,it-IT');
  check('a fallback chain fills each field from the first language that has a value', r.data.title === 'Bonjour' && r.data.body === 'Corpo italiano' && r.data.summary === null && r.data._language === 'fr-FR', r.data);
  r = await read('fields=title&lang=de-DE,en-US');
  check('languages that do not exist or have no translation are skipped along the chain', r.data.title === 'Hello' && r.data._language === 'en-US');
  r = await read('fields=title,body&lang=de-DE');
  check('no translation at all: the fields are null (a stable shape) and _language is null', r.status === 200 && r.data.title === null && r.data.body === null && r.data._language === null, r.data);
  r = await read('fields=*&lang=it-IT');
  check('fields=* with lang includes every translatable field and the item’s own', r.data.title === 'Ciao' && r.data.slug === 'alpha' && r.data.status && Array.isArray(r.data.translations), Object.keys(r.data));
  r = await read('fields=title');
  check('without lang a translated name is not a field of the item (400), as before', r.status === 400 && /does not exist/.test(r.error), r.error);
  r = await read('fields=slug,translations.title,translations.' + langField + '.code&lang=it-IT');
  check('the relation can still be expanded in the same request', Array.isArray(r.data.translations) && r.data.translations.length === 3 && r.data.slug === 'alpha');
  for (const bad of ['', 'it IT', "it'--", 'a,b,c,d,e,f,g,h,i']) {
    const res = await read(`fields=slug&lang=${encodeURIComponent(bad)}`);
    check(`lang=${JSON.stringify(bad)} is ${bad === '' ? 'ignored' : 'refused (400)'}`, bad === '' ? res.status === 200 : res.status === 400, res.status);
  }
  const noTr = await a.rest('GET', `/items/pages?lang=it-IT&fields=slug`);
  check('lang on a collection with its own translations field works the same way (the example’s pages)', noTr.status === 200);
  const plain = await a.rest('GET', `/items/${langs}?lang=it-IT`);
  check('lang on a collection without a translations field is a clear 400', plain.status === 400 && /needs a translations field/.test(plain.error), plain.error);

  await a.rest('POST', `/items/${art}`, { slug: 'zeta', translations: { 'it-IT': { title: 'Zeta IT' } } });
  await a.rest('POST', `/items/${art}`, { slug: 'omega', translations: { 'en-US': { title: 'Omega EN' } } });
  await a.rest('POST', `/items/${art}`, { slug: 'nothing' });
  const list = await a.rest('GET', `/items/${art}?fields=slug,title&lang=it-IT,en-US&sort=slug&limit=-1&filter=${encodeURIComponent(JSON.stringify({ slug: { _in: ['zeta', 'omega', 'nothing', 'alpha'] } }))}`);
  const by = Object.fromEntries(list.data.map((x) => [x.slug, x]));
  check('a list resolves every item in one request, each with its own best language', by.zeta.title === 'Zeta IT' && by.zeta._language === 'it-IT' && by.omega.title === 'Omega EN' && by.omega._language === 'en-US' && by.nothing.title === null && by.alpha.title === 'Ciao', by);
  const filtered = await a.rest('GET', `/items/${art}?lang=it-IT&fields=slug,title&filter=${encodeURIComponent(JSON.stringify({ translations: { _some: { title: { _eq: 'Zeta IT' } } } }))}`);
  check('items can be filtered by a translated value through the relation', filtered.data.length === 1 && filtered.data[0].slug === 'zeta' && filtered.data[0].title === 'Zeta IT', filtered.json);
  const noSort = await a.rest('GET', `/items/${art}?lang=it-IT&fields=slug&sort=title`);
  check('sorting by a translated name is refused rather than ignored', noSort.status >= 400, noSort.status);

  // an item field that is also translatable (the default language lives on the item): replaced when a translation exists
  const dflt = `tr_dflt_${tag}`;
  await a.rest('POST', '/collections', { name: dflt, fields: [{ name: 'title', type: 'string' }, { name: 'tagline', type: 'string' }] });
  const sc2 = await a.rest('POST', `/collections/${dflt}/translations`, { fields: [{ name: 'title', type: 'string' }, { name: 'tagline', type: 'string' }, { name: 'extra', type: 'string' }], languages_collection: langs });
  check('a collection can translate fields it also has itself (the default language stays on the item)', sc2.status === 200, sc2.json);
  const d1 = (await a.rest('POST', `/items/${dflt}`, { title: 'Default title', tagline: 'Default tagline', translations: { 'it-IT': { title: 'Titolo', extra: 'Extra IT' }, 'fr-FR': { tagline: 'Slogan' } } })).data.id;
  const d2 = (await a.rest('POST', `/items/${dflt}`, { title: 'Only default', tagline: 'Only tagline' })).data.id;
  let dr = (await a.rest('GET', `/items/${dflt}/${d1}?fields=title,tagline,extra&lang=it-IT`)).data;
  check('the translation replaces the item’s own value when there is one…', dr.title === 'Titolo' && dr.extra === 'Extra IT', dr);
  check('…and the item’s own value stays where the translation has none (the default language)', dr.tagline === 'Default tagline', dr);
  dr = (await a.rest('GET', `/items/${dflt}/${d1}?fields=title,tagline,extra&lang=fr-FR,it-IT`)).data;
  check('along a chain each field comes from the first language that has it, else from the item', dr.tagline === 'Slogan' && dr.title === 'Titolo' && dr.extra === 'Extra IT' && dr._language === 'fr-FR', dr);
  dr = (await a.rest('GET', `/items/${dflt}/${d2}?fields=title,tagline,extra&lang=it-IT`)).data;
  check('an item with no translation at all keeps its own values; a translation-only field is null', dr.title === 'Only default' && dr.tagline === 'Only tagline' && dr.extra === null && dr._language === null, dr);
  dr = (await a.rest('GET', `/items/${dflt}/${d1}?fields=title`)).data;
  check('without lang nothing is replaced', dr.title === 'Default title' && !('_language' in dr));
  const gd = await a.graphql(`{ ${dflt}(lang: "it-IT", filter: {id: {_eq: "${d1}"}}) { title tagline extra } }`);
  check('GraphQL applies the same rule', gd.json?.data?.[dflt]?.[0]?.title === 'Titolo' && gd.json.data[dflt][0].tagline === 'Default tagline' && gd.json.data[dflt][0].extra === 'Extra IT', gd.json);

  // ============================================================================================== language lifecycle
  const delLang = await a.rest('DELETE', `/items/${langs}/${lang('it-IT')}`);
  check('a language in use cannot be deleted (RESTRICT)', delLang.status === 400 && delLang.code === 'INVALID_FOREIGN_KEY', delLang.json);
  const doomed = (await a.rest('POST', `/items/${art}`, { slug: 'doomed', translations: { 'it-IT': { title: 'x' } } })).data.id;
  await a.rest('DELETE', `/items/${art}/${doomed}`);
  check('deleting an item deletes its translations', q(`SELECT count(*) FROM "${trc}" WHERE "${parentField}" = '${doomed}'`)[0][0] === '0');

  // ===================================================================================================== permissions
  const polRead = uuid(), polBlind = uuid(), polWrite = uuid(), roleRead = uuid(), roleBlind = uuid(), roleWrite = uuid();
  const put = (id, name, permissions) => a.rest('PUT', `/policies/${id}`, { name, app_access: true, permissions });
  await put(polRead, 'tr-read', [
    { collection: art, action: 'read', fields: ['id', 'slug', 'translations'] },
    { collection: trc, action: 'read', fields: ['id', parentField, langField, 'title'] }, // no body, no summary
  ]);
  await put(polBlind, 'tr-blind', [{ collection: art, action: 'read', fields: ['id', 'slug', 'translations'] }]);
  await put(polWrite, 'tr-write', [
    { collection: art, action: 'read', fields: ['id', 'slug'] },
    { collection: art, action: 'update', fields: ['slug', 'translations'] },
  ]);
  for (const [r, p] of [[roleRead, polRead], [roleBlind, polBlind], [roleWrite, polWrite]]) await a.rest('PUT', `/roles/${r}`, { name: `role-${p.slice(0, 4)}`, policies: [p] });
  const login = async (role) => {
    const email = `tr${rnd()}@example.com`;
    await a.rest('POST', '/users', { email, password: 'editor-pass-123', role });
    return (await a.rest('POST', '/auth/login', { email, password: 'editor-pass-123' }, null)).data.access_token;
  };
  const [uRead, uBlind, uWrite] = [await login(roleRead), await login(roleBlind), await login(roleWrite)];
  const asRead = await a.rest('GET', `/items/${art}/${alpha}?fields=*&lang=it-IT`, undefined, uRead);
  check('a reader sees only the translated fields they may read: title yes, body no (null, not leaked)', asRead.status === 200 && asRead.data.title === 'Ciao' && asRead.data.body === null && !JSON.stringify(asRead.json).includes('Corpo italiano'), asRead.json);
  const asBlind = await a.rest('GET', `/items/${art}/${alpha}?fields=slug,title&lang=it-IT`, undefined, uBlind);
  check('without read access to the translations the item simply has none (no error, nothing leaked)', asBlind.status === 200 && asBlind.data.slug === 'alpha' && !JSON.stringify(asBlind.json).includes('Ciao'), asBlind.json);
  const asWrite = await a.rest('PATCH', `/items/${art}/${alpha}`, { translations: { 'it-IT': { title: 'Hacked' } } }, uWrite);
  check('writing translations needs permission on the translations collection (403)', asWrite.status === 403 && (await byCode())['it-IT'].title === 'Ciao', asWrite.json);
  const asWriteCreate = await a.rest('PATCH', `/items/${art}/${alpha}`, { translations: { 'es-ES': { title: 'x' } } }, uWrite);
  check('…and an unknown language does not reveal anything to someone who cannot write (still an error)', asWriteCreate.status >= 400);

  // ============================================================================================ GraphQL and snapshot
  const gql = await a.graphql(`{ ${art}(filter: {slug: {_eq: "alpha"}}) { slug translations { title ${langField} { code } } } }`);
  const gt = gql.json?.data?.[art]?.[0]?.translations ?? [];
  check('GraphQL exposes the field as a typed list of translations', gql.status === 200 && gt.length === 3 && gt.some((x) => x.title === 'Ciao' && x[langField].code === 'it-IT'), gql.json);
  const gl = await a.graphql(`{ ${art}(filter: {slug: {_eq: "alpha"}}, lang: "fr-FR,it-IT") { slug title body _language } }`);
  const gi = gl.json?.data?.[art]?.[0];
  check('GraphQL: the lang argument resolves translated fields with the same fallback chain', gl.status === 200 && gi?.title === 'Bonjour' && gi.body === 'Corpo italiano' && gi._language === 'fr-FR' && gi.slug === 'alpha', gl.json);
  const gb = await a.graphql(`query($id: ID!) { ${art}_by_id(id: $id, lang: "en-US") { title _language } }`, { id: alpha });
  check('…also on by_id', gb.json?.data?.[`${art}_by_id`]?.title === 'Hello', gb.json);
  const gn = await a.graphql(`{ ${art}(filter: {slug: {_eq: "alpha"}}) { slug title } }`);
  check('…and without lang a translated field is not queryable (a clear error)', gn.json?.errors?.[0]?.message?.includes('Cannot query field "title"'), gn.json);
  const gnest = await a.graphql(`{ ${art}(lang: "it-IT", limit: 1) { translations { title } } }`);
  check('…while the relation stays available next to it', Array.isArray(gnest.json?.data?.[art]?.[0]?.translations), gnest.json);
  const gtext = (await a.rest('GET', '/graphql/schema')).text;
  check('the SDL documents the lang argument, the translated fields and _language', gtext.includes(`${art}(filter: JSON, sort: [String], limit: Int, offset: Int, page: Int, search: String, lang: String)`) && gtext.slice(gtext.indexOf(`type ${art} {`)).split('}')[0].includes('  title: String') && gtext.includes('  _language: String'));
  const snap = (await a.rest('GET', '/schema/snapshot?format=yaml')).text;
  check('the snapshot YAML carries the type, the language field and the composite unique', /type: translations/.test(snap) && new RegExp(`language_field: ${langField}`).test(snap) && /unique_with:/.test(snap));

  // ====================================================================================== promotion between environments
  let r2 = sh(['schema', 'diff', '--from', 'a', '--to', 'b', '--exit-code']);
  check('schema diff A→B lists the new collections and the translations field', r2.code === 2 && r2.out.includes(`+ collection ${trc}`) && /translations/.test(r2.out), r2.out.slice(0, 300));
  r2 = sh(['schema', 'apply', '--from', 'a', '--to', 'b', '--yes']);
  check('schema apply A→B', r2.code === 0, r2.out.slice(-250));
  check('B has the same composite constraint', sql('ferro_e2e_b', `SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = '"${trc}"'::regclass AND contype = 'u'`)[0]?.[0] === `UNIQUE (${langField}, ${parentField})`);
  check('…and the schemas are identical', sh(['schema', 'diff', '--from', 'a', '--to', 'b', '--exit-code']).code === 0);
  r2 = sh(['data', 'push', '--from', 'a', '--to', 'b', '--select', `${art}:{"slug":{"_eq":"alpha"}}`, '--yes']);
  check('data push of one item brings its translations and the languages they use', r2.code === 0 && new RegExp(`${trc}\\s+3`).test(r2.out) && new RegExp(`${langs}\\s+3`).test(r2.out), r2.out.slice(-400));
  const onB = await b.rest('GET', `/items/${art}?filter=${encodeURIComponent(JSON.stringify({ slug: { _eq: 'alpha' } }))}&fields=slug,title,body&lang=it-IT`);
  check('on B the item reads in Italian', onB.data?.[0]?.title === 'Ciao' && onB.data[0]._language === 'it-IT', onB.json);
  r2 = sh(['data', 'push', '--from', 'a', '--to', 'b', '--select', `${art}:{"slug":{"_eq":"alpha"}}`, '--yes']);
  check('pushing again changes nothing (idempotent, no duplicate translations)', r2.code === 0 && sql('ferro_e2e_b', `SELECT count(*) FROM "${trc}"`)[0][0] === '3', r2.out.slice(-200));

  // the same page translated independently on both sides
  const beta = (await a.rest('POST', `/items/${art}`, { slug: 'beta', translations: { 'it-IT': { title: 'Beta A' }, 'en-US': { title: 'Beta EN' } } })).data.id;
  const betaB = (await b.rest('POST', `/items/${art}`, { slug: 'beta', translations: { 'it-IT': { title: 'Beta B' }, 'fr-FR': { title: 'Beta FR' } } }));
  check('(set-up) B created its own "beta" with its own translations', betaB.status === 200, betaB.json);
  r2 = sh(['data', 'push', '--from', 'a', '--to', 'b', '--select', `${art}:{"slug":{"_eq":"beta"}}`, '--yes']);
  const betaRows = (await b.rest('GET', `/items/${trc}?fields=${langField}.code,title&limit=-1&filter=${encodeURIComponent(JSON.stringify({ [parentField]: { _eq: betaB.data.id } }))}`)).data ?? [];
  const betaByCode = Object.fromEntries(betaRows.map((x) => [x[langField].code, x.title]));
  check('promoting an item translated on both sides merges per language (the same language is not duplicated)', r2.code === 0 && betaRows.length === 3 && betaByCode['it-IT'] === 'Beta A' && betaByCode['en-US'] === 'Beta EN' && betaByCode['fr-FR'] === 'Beta FR', [r2.out.slice(-300), betaByCode]);
}
