// Admin UI in a real browser. Starts its own environment (see tests/e2e/lib.mjs for the requirements).
//
//   cd tests/browser && npm install
//   BROWSER_PATH="C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe" node run.mjs     (Windows, Edge)
//   npx playwright-core install chromium && node run.mjs                                             (managed Chromium)
import { join } from 'node:path';

import { chromium } from 'playwright-core';

import { Env, ROOT, check, cli, rnd, sleep, summary } from '../e2e/lib.mjs';

const env = new Env('ui', 18060, 'ferro_e2e_ui', {
  FERRO_ADMIN_DIR: join(ROOT, 'admin'),
  FERRO_EXTENSIONS_DIR: join(ROOT, 'examples/site/extensions'),
  FERRO_FLOW_ALLOW_PRIVATE: 'true',
});
const ADMIN = `${env.url}/admin/`;
const PASSWORD = 'e2e-password-123';
let browser;
let ok = false;

try {
  await env.start();
  await applyExampleSchema();

  // ---- data -------------------------------------------------------------------------------------
  const cat = (await env.rest('POST', '/items/categories', { code: 'corp', label: 'Corporate' })).data;
  const home = (
    await env.rest('POST', '/items/pages', {
      title: 'Home', slug: 'home', status: 'published', category: cat.id,
      blocks: [
        { collection: 'block_hero', item: { headline: 'Welcome' }, sort: 1 },
        { collection: 'block_cards', item: { title: 'Cards', cards: [{ title: 'Uno', sort: 1 }, { title: 'Due', sort: 2 }] }, sort: 2 },
      ],
    })
  ).data;
  // a many-to-many model for the M2M editor test
  await env.rest('POST', '/collections', { name: 'ui_tags', fields: [{ name: 'name', type: 'string', required: true }] });
  await env.rest('POST', '/collections', { name: 'ui_posts', fields: [{ name: 'title', type: 'string' }] });
  await env.rest('POST', '/collections', { name: 'ui_posts_tags', system_fields: false, fields: [{ name: 'posts_id', type: 'm2o', relation: { related: 'ui_posts', on_delete: 'cascade' } }, { name: 'tags_id', type: 'm2o', relation: { related: 'ui_tags' } }] });
  await env.rest('POST', '/fields/ui_posts', { name: 'tags', type: 'm2m', relation: { related: 'ui_posts_tags', foreign_field: 'posts_id', junction_field: 'tags_id' } });
  const tagA = (await env.rest('POST', '/items/ui_tags', { name: 'alpha' })).data;
  await env.rest('POST', '/items/ui_tags', { name: 'beta' });
  const post = (await env.rest('POST', '/items/ui_posts', { title: 'A post' })).data;

  browser = await chromium.launch({ executablePath: process.env.BROWSER_PATH || undefined, headless: true });
  const errors = [];
  const newPage = async (opts = {}) => {
    const ctx = await browser.newContext({ viewport: { width: 1300, height: 900 }, ...opts });
    const page = await ctx.newPage();
    page.on('pageerror', (e) => errors.push(`pageerror: ${e.message}`));
    page.on('console', (m) => m.type() === 'error' && !/Failed to load resource/.test(m.text()) && errors.push(`console: ${m.text()}`));
    return page;
  };
  const signIn = async (page, email = 'admin@example.com', password = PASSWORD) => {
    await page.goto(ADMIN);
    await page.fill('input[type=email]', email);
    await page.fill('input[type=password]', password);
    await page.click('button[type=submit]');
    await page.waitForSelector('nav#nav a');
  };

  // ======================================================================================================
  console.log('\n## login and navigation');
  const page = await newPage();
  await page.goto(ADMIN);
  await page.waitForSelector('form.login-card');
  await page.fill('input[type=email]', 'admin@example.com');
  await page.fill('input[type=password]', 'wrong');
  await page.click('button[type=submit]');
  await page.waitForSelector('.error-box:not([hidden])');
  check('wrong password is reported', (await page.textContent('.error-box')).includes('invalid credentials'));
  await page.fill('input[type=password]', PASSWORD);
  await page.click('button[type=submit]');
  await page.waitForSelector('nav#nav a');
  const nav = await page.$$eval('nav#nav a', (a) => a.map((x) => x.textContent));
  check('sidebar: collections, library and administration', ['Pages', 'Files', 'GraphQL', 'Data model', 'Access control', 'Automations', 'Environment', 'Activity', 'Overview'].every((n) => nav.includes(n)), nav.join('|'));
  check('environment badge', (await page.textContent('.env')) === 'ui');

  console.log('\n## item editor');
  await page.click('nav#nav a:has-text("Pages")');
  await page.waitForSelector('table tbody tr.clickable');
  await page.fill('input[type=search]', 'home');
  await page.press('input[type=search]', 'Enter');
  await page.waitForSelector('td:text-is("home")');
  check('list: search finds the page', true);
  await page.click('table tbody tr.clickable:has(td:text-is("home"))');
  await page.waitForSelector('.relation-editor .row-card');
  const blocks = await page.$$eval('.relation-editor .row-card > .row-head strong', (e) => e.map((x) => x.textContent));
  check('M2A blocks and their children are editable inline', blocks.some((b) => b.startsWith('Block Hero')) && blocks.some((b) => b.startsWith('Block Cards')) && blocks.includes('Uno'), blocks.join('|'));
  check('relation select shows the saved category', (await page.$$eval('.form select', (s) => s.map((x) => x.selectedOptions[0]?.textContent))).includes('corp'));
  check('extension action and manual flow buttons', (await page.isVisible('button:has-text("Copy slug")')) && (await page.isVisible('button:has-text("Unpublish page")')));
  const titleInput = page.locator('.form input[type=text]').first();
  await titleInput.fill('Home (edited)');
  await page.click('button:has-text("Save")');
  await page.waitForSelector('.toast.ok');
  await page.reload();
  await page.waitForSelector('.form input[type=text]');
  check('save persists, category survives (no accidental null)', (await page.locator('.form input[type=text]').first().inputValue()) === 'Home (edited)' && (await env.rest('GET', `/items/pages/${home.id}?fields=category`)).data.category === cat.id);
  await page.locator('.form input[type=text]').first().fill('');
  await page.click('button:has-text("Save")');
  await page.waitForSelector('.toast.error');
  check('required field is enforced before sending', (await page.textContent('.toast.error')).includes('Required'));
  await page.locator('.form input[type=text]').first().fill('Home');
  await page.click('button:has-text("Save")');
  await page.waitForSelector('.toast.ok >> nth=-1');

  console.log('\n## history');
  await env.rest('PATCH', `/items/pages/${home.id}`, { title: 'Changed elsewhere' });
  await page.goto(`${ADMIN}#/content/pages/${home.id}`);
  await page.waitForSelector('.form input[type=text]');
  await page.click('button:has-text("History")');
  await page.waitForSelector('.modal table tbody tr');
  const restore = page.locator('.modal tr', { hasText: 'update' }).locator('button:has-text("Restore fields")').last();
  await restore.click();
  await page.click('.overlay:last-of-type button:text-is("Restore")');
  await page.waitForSelector('.toast.ok');
  await sleep(800);
  check('restoring a revision brings the old value back', (await env.rest('GET', `/items/pages/${home.id}?fields=title`)).data.title !== 'Changed elsewhere');

  console.log('\n## files');
  await page.goto(`${ADMIN}#/content/pages/${home.id}`);
  await page.waitForSelector('.file-field');
  const coverField = () => page.locator('.form > .field', { has: page.locator('label', { hasText: /^Cover/ }) });
  await coverField().locator('input[type=file]').setInputFiles({ name: 'cover.txt', mimeType: 'text/plain', buffer: Buffer.from('cover content') });
  await coverField().locator('.file-chip').waitFor();
  check('upload from the form shows the file', (await coverField().textContent()).includes('cover.txt'));
  await page.click('button:has-text("Save")');
  await page.waitForSelector('.toast.ok >> nth=-1');
  const cover = (await env.rest('GET', `/items/pages/${home.id}?fields=cover.filename_download`)).data.cover;
  check('the cover is saved on the item', cover?.filename_download === 'cover.txt');
  await page.reload();
  await coverField().locator('.file-chip').waitFor();
  check('after a reload the file name is shown (not just an id)', (await coverField().textContent()).includes('cover.txt'));
  await page.goto(`${ADMIN}#/files`);
  await page.waitForSelector('.file-card');
  await page.click('.file-card >> nth=0');
  await page.waitForSelector('.modal');
  check('files library: detail dialog with a download link', await page.isVisible('.modal a:has-text("Download")'));
  await page.keyboard.press('Escape');
  check('Escape closes the dialog', (await page.$$('.overlay')).length === 0);

  console.log('\n## many-to-many editor');
  await page.goto(`${ADMIN}#/content/ui_posts/${post.id}`);
  await page.waitForSelector('.relation-editor');
  await page.click('button:has-text("+ Add")');
  await page.waitForSelector('.relation-editor .row-inline select');
  await page.waitForFunction(() => document.querySelector('.relation-editor .row-inline select')?.options.length > 2);
  await page.selectOption('.relation-editor .row-inline select', { label: 'alpha' });
  await page.click('button:has-text("Save")');
  await page.waitForSelector('.toast.ok');
  const rows = (await env.rest('GET', `/items/ui_posts/${post.id}?fields=tags.tags_id`)).data.tags;
  check('M2M: selecting a tag creates the junction row', rows.length === 1 && rows[0].tags_id === tagA.id, rows);

  console.log('\n## lists: filters and bulk delete');
  for (let i = 0; i < 3; i++) await env.rest('POST', '/items/categories', { code: `bulk-${i}-${rnd()}` });
  await page.goto(`${ADMIN}#/content/categories`);
  await page.waitForSelector('table tbody tr.clickable');
  await page.click('button:has-text("+ Filter")');
  await page.locator('.cond input[type=text]').fill('bulk-');
  await page.selectOption('.cond select >> nth=1', '_istarts_with').catch(() => {});
  await page.click('button:has-text("Apply filters")');
  await page.waitForFunction(() => document.querySelectorAll('table tbody tr.clickable').length === 3);
  check('filter by field and operator', true);
  await page.locator('table tbody input[type=checkbox]').evaluateAll((boxes) => boxes.forEach((b) => b.click()));
  await page.click('button:has-text("Delete 3 selected")');
  await page.click('.modal button:has-text("Delete")');
  await page.waitForFunction(() => document.querySelectorAll('table tbody tr.clickable').length === 0);
  check('bulk delete removes the selected items', true);

  console.log('\n## model, access, users, automations');
  await page.goto(`${ADMIN}#/model/pages`);
  await page.waitForSelector('button:has-text("+ New field")');
  await page.click('button:has-text("+ New field")');
  const field = `extra_${rnd()}`;
  await page.fill('input[placeholder=field_name]', field);
  await page.selectOption('.modal select >> nth=0', 'm2o');
  await page.waitForSelector('.modal select option[value=categories]', { state: 'attached' });
  await page.selectOption('.modal select >> nth=1', 'categories');
  await page.click('.modal button:has-text("Save")');
  await page.waitForSelector('.changes li');
  const plan = await page.$$eval('.changes li', (e) => e.map((x) => x.textContent));
  check('model editor: a relation field shows its plan before applying', plan.some((p) => p.includes(field)), plan.join('; '));
  await page.click('.modal button:has-text("Apply")');
  await page.waitForSelector('.toast.ok');
  await sleep(800);
  const added = (await env.rest('GET', '/collections/pages')).data.fields.find((f) => f.name === field);
  check('…and the relation exists afterwards', added?.type === 'm2o' && added.relation.related === 'categories', added);

  await page.goto(`${ADMIN}#/access`);
  await page.waitForSelector('tr.clickable');
  await page.locator('table').nth(1).locator('tr:has-text("Editor")').click(); // the policies table (the first one lists roles)
  await page.waitForSelector('.modal');
  check('access editor opens a policy with its permissions', (await page.$$('.modal .row-card')).length >= 3);
  await page.keyboard.press('Escape');

  await page.goto(`${ADMIN}#/users`);
  await page.waitForSelector('button:has-text("+ New user")');
  await page.click('button:has-text("+ New user")');
  const newEmail = `ui${rnd()}@example.com`;
  await page.locator('.modal input[type=text]').first().fill(newEmail);
  await page.locator('.modal input[type=password]').fill('a-long-password-1');
  await page.click('.modal button:has-text("Save")');
  await page.waitForSelector(`td:text-is("${newEmail}")`);
  check('users: create a user from the UI', (await env.rest('POST', '/auth/login', { email: newEmail, password: 'a-long-password-1' }, null)).status === 200);
  await page.click(`tr:has(td:text-is("${newEmail}"))`);
  await page.click('.modal button:has-text("Generate API token")');
  await page.waitForSelector('pre.token');
  check('users: an API token is generated and shown once', (await page.textContent('pre.token')).startsWith('ferro_'));
  await page.keyboard.press('Escape');
  await page.keyboard.press('Escape');

  await page.goto(`${ADMIN}#/flows`);
  await page.waitForSelector('tr.clickable');
  await page.click('button:has-text("+ New flow")');
  await page.waitForSelector('.tile');
  check('automations: templates are offered', (await page.$$('.modal .tile')).length === 5);
  await page.click('.modal .tile:has-text("Scheduled job")'); // (the endpoint template would clash with the example slug)
  await page.waitForSelector('.modal textarea');
  await page.click('.modal button:has-text("Save")');
  await page.waitForSelector('.changes li');
  check('automations: a new flow shows its plan', (await page.textContent('.changes')).includes('+ flow'));
  await page.keyboard.press('Escape');
  await page.keyboard.press('Escape');

  console.log('\n## live updates');
  await page.goto(`${ADMIN}#/content/pages`);
  await page.waitForSelector('table tbody tr.clickable');
  await sleep(800);
  await env.rest('POST', '/items/pages', { title: 'live', slug: `live-${rnd()}`, status: 'draft' });
  await page.waitForSelector('.banner:not([hidden])', { timeout: 6000 });
  check('list: banner when another client adds an item', (await page.textContent('.banner')).includes('added items'));

  console.log('\n## safety of rendered content');
  const xss = (await env.rest('POST', '/items/pages', { title: '<img src=x onerror="window.__xss=1"><script>window.__xss=2</script>', slug: `xss-${rnd()}`, status: 'draft' })).data;
  await page.goto(`${ADMIN}#/content/pages`);
  await page.waitForSelector('table tbody tr.clickable');
  await page.fill('input[type=search]', 'xss-');
  await page.press('input[type=search]', 'Enter');
  await sleep(800);
  check('markup in a title is shown as text in the list', (await page.textContent('table tbody')).includes('<img src=x'));
  await page.goto(`${ADMIN}#/content/pages/${xss.id}`);
  await page.waitForSelector('.form input[type=text]');
  await sleep(500);
  check('…and in the editor (no script ran)', (await page.evaluate(() => window.__xss)) === undefined && (await page.locator('.form input[type=text]').first().inputValue()).startsWith('<img'));
  await page.goto(`${ADMIN}#/activity`);
  await page.waitForSelector('main h1');
  check('…and in the activity log', (await page.evaluate(() => window.__xss)) === undefined);

  console.log('\n## session');
  await page.reload();
  await page.waitForSelector('nav#nav a');
  check('session restored after reload', true);
  await page.click('button:has-text("Sign out")');
  await page.waitForSelector('form.login-card');
  check('sign out returns to the login form', true);

  console.log('\n## non-administrator');
  const editorMail = `ed${rnd()}@example.com`;
  await env.rest('POST', '/users', { email: editorMail, password: 'editor-pass-123', role: '0199a000-0000-7000-8000-00000000e001' });
  const ed = await newPage();
  await signIn(ed, editorMail, 'editor-pass-123');
  const edNav = await ed.$$eval('nav#nav a', (a) => a.map((x) => x.textContent));
  check('an editor does not see the administration section', !edNav.includes('Data model') && !edNav.includes('Users') && edNav.includes('Pages'), edNav.join('|'));
  await ed.goto(`${ADMIN}#/model`);
  await ed.waitForSelector('.error-box');
  check('…and cannot open an admin page by URL', (await ed.textContent('.error-box')).includes('Administrator'));

  console.log('\n## look and feel');
  const dark = await newPage({ colorScheme: 'dark' });
  await signIn(dark);
  const bg = await dark.evaluate(() => getComputedStyle(document.body).backgroundColor);
  check('dark mode follows the system setting', bg !== 'rgb(245, 246, 248)', bg);
  const mobile = await newPage({ viewport: { width: 390, height: 800 } });
  await signIn(mobile);
  await mobile.click('nav#nav a:has-text("Pages")');
  await mobile.waitForSelector('table');
  check('mobile layout: no horizontal scroll of the page', await mobile.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 2));

  console.log('\nbrowser errors:', errors.length ? `\n  ${[...new Set(errors)].join('\n  ')}` : 'none');
  check('no JavaScript errors in the console', errors.length === 0, errors.slice(0, 3));
  ok = summary();
} catch (err) {
  console.error(err);
  check('the browser suite did not crash', false, String(err).split('\n')[0]);
  summary();
} finally {
  if (browser) await browser.close();
  await env.stop();
}
process.exit(ok ? 0 : 1);

async function applyExampleSchema() {
  // the e2e project file knows environments a and b: use the server API with the example directory's snapshot
  const { spawnSync } = await import('node:child_process');
  const { BIN } = await import('../e2e/lib.mjs');
  const toml = join(ROOT, 'target/e2e/ui.toml');
  (await import('node:fs')).writeFileSync(toml, `[project]\nschema = "${join(ROOT, 'examples/site/schema').replace(/\\/g, '/')}"\n\n[environments.ui]\nurl = "${env.url}"\ntoken_env = "FERRO_UI_TOKEN"\n`);
  const r = spawnSync(BIN, ['--project', toml, 'schema', 'apply', '--to', 'ui', '--yes'], { env: { ...process.env, FERRO_UI_TOKEN: env.token }, encoding: 'utf8' });
  if (r.status !== 0) throw new Error(`cannot apply the example schema:\n${r.stdout}${r.stderr}`);
}
