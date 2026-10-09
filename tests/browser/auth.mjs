// Account security in a real browser: forgot password, emailed reset link, invitation, two-factor enrolment with a real QR
// code and sign-in with a code, and single sign-on through a mock identity provider.
// Same requirements as run.mjs (browser, PostgreSQL, built binary).
import { join } from 'node:path';

import { chromium } from 'playwright-core';

import { Env, ROOT, check, rnd, sleep, summary } from '../e2e/lib.mjs';
import { FakeSmtp, linkIn, totp } from '../e2e/identity.mjs';
import { MockIdp } from '../e2e/mock-idp.mjs';

const ADMIN_ROLE = '00000000-0000-7000-8000-000000000002';
const PORT = 18061;
const PUBLIC = `http://127.0.0.1:${PORT}`;
const smtp = await new FakeSmtp().start();
const idp = await new MockIdp(18071).start();
idp.clients = { 'cid-ui': 's-ui' };
const env = new Env('uiauth', PORT, 'ferro_e2e_uiauth', {
  FERRO_ADMIN_DIR: join(ROOT, 'admin'),
  FERRO_PUBLIC_URL: PUBLIC,
  FERRO_SMTP_HOST: '127.0.0.1',
  FERRO_SMTP_PORT: String(smtp.port),
  FERRO_SMTP_SECURE: 'none',
  FERRO_EMAIL_FROM: 'Ferro <no-reply@example.com>',
  FERRO_AUTH_PROVIDERS: 'mock',
  FERRO_AUTH_MOCK_CLIENT_ID: 'cid-ui',
  FERRO_AUTH_MOCK_CLIENT_SECRET: 's-ui',
  FERRO_AUTH_MOCK_ISSUER_URL: idp.issuer,
  FERRO_AUTH_MOCK_LABEL: 'Sign in with Mock',
  FERRO_AUTH_MOCK_ALLOW_PUBLIC_REGISTRATION: 'true',
  FERRO_AUTH_MOCK_DEFAULT_ROLE: ADMIN_ROLE,
});
const ADMIN = `${PUBLIC}/admin/`;
let browser;
let ok = false;

try {
  await env.start();
  browser = await chromium.launch({ executablePath: process.env.BROWSER_PATH || undefined, headless: true });
  const errors = [];
  const newPage = async () => {
    const ctx = await browser.newContext({ viewport: { width: 1200, height: 900 } });
    const page = await ctx.newPage();
    page.on('pageerror', (e) => errors.push(`pageerror: ${e.message}`));
    // an ApiError is the admin reporting an expected refusal (wrong password / code in these tests): it also shows as a toast
    page.on('console', (m) => m.type() === 'error' && !/Failed to load resource|^ApiError:/.test(m.text()) && errors.push(`console: ${m.text()}`));
    return page;
  };
  const signIn = async (page, email, password, otp) => {
    await page.goto(ADMIN);
    await page.waitForSelector('form.login-card');
    await page.fill('input[type=email]', email);
    await page.fill('input[type=password]', password);
    await page.click('button[type=submit]');
    if (otp !== undefined) {
      await page.waitForSelector('input[aria-label="Authentication code"]', { state: 'visible' });
      await page.fill('input[aria-label="Authentication code"]', otp);
      await page.click('button[type=submit]');
    }
  };
  const user = async (email, password = 'start-password-1') => (await env.rest('POST', '/users', { email, password, role: ADMIN_ROLE })).data;

  // =========================================================================================== forgot password
  console.log('\n## forgot password and the emailed link');
  const ann = `ann${rnd()}@example.com`;
  await user(ann, 'old-password-1');
  const page = await newPage();
  await page.goto(ADMIN);
  await page.waitForSelector('form.login-card');
  await page.waitForSelector('a:has-text("Forgot your password?")');
  check('the sign-in page offers "Forgot your password?" and the provider button', (await page.isVisible('a:has-text("Forgot your password?")')) && (await page.isVisible('a.sso-button:has-text("Sign in with Mock")')));
  const t0 = Date.now();
  await page.fill('input[type=email]', ann);
  await page.click('a:has-text("Forgot your password?")');
  await page.waitForSelector('h2:has-text("Reset your password")');
  check('the address typed on the sign-in page is carried over', (await page.inputValue('input[type=email]')) === ann);
  await page.click('button[type=submit]');
  await page.waitForSelector('h2:has-text("Check your email")');
  const mail = await smtp.wait(ann, t0);
  const link = linkIn(mail ?? { text: '' });
  check('the confirmation does not reveal whether the address exists, and an email arrives', !!mail && /If that address belongs to an account/.test(await page.textContent('body')), link);

  const page2 = await newPage();
  await page2.goto(link);
  await page2.waitForSelector('h2:has-text("Choose a new password")');
  check('the link opens the reset form and the token is removed from the address bar at once', !page2.url().includes('token=') && !page2.url().includes('#/reset'), page2.url());
  const [a, b] = await page2.$$('input[type=password]');
  await a.fill('brand-new-pass-1');
  await b.fill('something-else-1');
  await page2.click('button[type=submit]');
  await page2.waitForSelector('.error-box:not([hidden])');
  check('mismatching passwords are caught in the browser', /not the same/.test(await page2.textContent('.error-box')));
  await b.fill('brand-new-pass-1');
  await page2.click('button[type=submit]');
  await page2.waitForSelector('h2:has-text("Password changed")');
  await page2.click('a:has-text("Go to sign in")');
  await page2.waitForSelector('form.login-card');
  await signIn(page2, ann, 'old-password-1');
  await page2.waitForSelector('.error-box:not([hidden])');
  check('the old password no longer works', /invalid credentials/.test(await page2.textContent('.error-box')));
  await page2.fill('input[type=password]', 'brand-new-pass-1');
  await page2.click('button[type=submit]');
  await page2.waitForSelector('nav#nav a');
  check('…and the new one signs in', true);
  const reuse = await newPage();
  await reuse.goto(link);
  await reuse.waitForSelector('h2:has-text("Choose a new password")');
  const [c, d] = await reuse.$$('input[type=password]');
  await c.fill('yet-another-pass-1');
  await d.fill('yet-another-pass-1');
  await reuse.click('button[type=submit]');
  await reuse.waitForSelector('.error-box:not([hidden])');
  check('the same link cannot be used twice', /invalid or has expired/.test(await reuse.textContent('.error-box')));

  // ================================================================================================ invitation
  console.log('\n## invitation');
  const admin = await newPage();
  await signIn(admin, 'admin@example.com', 'e2e-password-123');
  await admin.waitForSelector('nav#nav a');
  await admin.goto(`${ADMIN}#/users`);
  await admin.waitForSelector('button:has-text("Invite by email")');
  const guest = `guest${rnd()}@example.com`;
  const t1 = Date.now();
  await admin.click('button:has-text("Invite by email")');
  await admin.fill('.modal input[type=email]', guest);
  await admin.selectOption('.modal select', ADMIN_ROLE);
  await admin.click('.modal button:has-text("Send the invitation")');
  const invMail = await smtp.wait(guest, t1);
  check('inviting from the Users page sends the email', !!invMail && /invited/i.test(invMail.subject), invMail?.subject);
  const guestPage = await newPage();
  await guestPage.goto(linkIn(invMail ?? { text: '' }));
  await guestPage.waitForSelector('h2:has-text("Welcome")');
  const [e1, e2] = await guestPage.$$('input[type=password]');
  await e1.fill('guest-pass-1234');
  await e2.fill('guest-pass-1234');
  await guestPage.click('button[type=submit]');
  await guestPage.waitForSelector('nav#nav a');
  check('accepting the invitation signs the guest straight into the app', (await guestPage.textContent('.topbar')).includes(guest));
  check('…the token left the address bar', !guestPage.url().includes('token='));
  await admin.reload();
  await admin.waitForSelector('table tbody tr');
  const guestRow = await admin.textContent(`tr:has(td:text-is("${guest}"))`);
  check('the Users list now shows the guest as active with a password sign-in and no 2FA', /active/.test(guestRow) && /password/.test(guestRow) && /—/.test(guestRow), guestRow);

  // ====================================================================================== two-factor authentication
  console.log('\n## two-factor authentication');
  await guestPage.click('a.account-link');
  await guestPage.waitForSelector('h1:has-text("My account")');
  await guestPage.click('button:has-text("Set up two-factor authentication")');
  await guestPage.fill('input[type=password]', 'wrong-password-1');
  await guestPage.click('button:has-text("Continue")');
  await guestPage.waitForSelector('.toast.error');
  check('the current password is checked before a secret is shown', !(await guestPage.isVisible('img.qr')));
  await guestPage.fill('input[type=password]', 'guest-pass-1234');
  await guestPage.click('button:has-text("Continue")');
  await guestPage.waitForSelector('img.qr');
  const qr = await guestPage.getAttribute('img.qr', 'src');
  const loaded = await guestPage.evaluate(() => { const i = document.querySelector('img.qr'); return i.complete && i.naturalWidth > 0; });
  check('a QR code image is displayed (an SVG data address, rendered)', qr.startsWith('data:image/svg+xml') && loaded, qr.slice(0, 40));
  const secret = (await guestPage.textContent('code.secret')).replace(/\s+/g, '');
  check('the secret is shown for manual entry', /^[A-Z2-7]{32}$/.test(secret), secret);
  await guestPage.fill('input[autocomplete=one-time-code]', '000000');
  await guestPage.click('button:has-text("Turn on")');
  await guestPage.waitForSelector('.toast.error');
  check('a wrong code is refused and the setup stays', await guestPage.isVisible('img.qr'));
  await guestPage.fill('input[autocomplete=one-time-code]', totp(secret));
  await guestPage.click('button:has-text("Turn on")');
  await guestPage.waitForSelector('pre.recovery');
  const codes = (await guestPage.textContent('pre.recovery')).trim().split(/\s+/);
  check('ten recovery codes are shown once', codes.length === 10 && codes.every((x) => /^[a-z2-7]{5}-[a-z2-7]{5}$/.test(x)), codes.length);
  await guestPage.click('button:has-text("I have saved them")');
  await guestPage.waitForSelector('button:has-text("Turn off…")');
  check('the account page now shows two-factor as on', /On\./.test(await guestPage.textContent('#tfa-holder')));
  await admin.reload();
  await admin.waitForSelector('table tbody tr');
  check('…and the administrator’s Users list shows the 2FA mark', /✓/.test(await admin.textContent(`tr:has(td:text-is("${guest}"))`)));

  await guestPage.click('button:has-text("Sign out")');
  await guestPage.waitForSelector('form.login-card');
  await signIn(guestPage, guest, 'guest-pass-1234');
  await guestPage.waitForSelector('input[aria-label="Authentication code"]', { state: 'visible' });
  check('signing in now asks for the code after the password', true);
  check('…without showing an error for that', !(await guestPage.isVisible('.error-box:not([hidden])')));
  await guestPage.fill('input[aria-label="Authentication code"]', '111111');
  await guestPage.click('button[type=submit]');
  await guestPage.waitForSelector('.error-box:not([hidden])');
  check('a wrong code is reported and cleared', /invalid two-factor code/.test(await guestPage.textContent('.error-box')) && (await guestPage.inputValue('input[aria-label="Authentication code"]')) === '');
  await guestPage.fill('input[aria-label="Authentication code"]', totp(secret, +1));
  await guestPage.click('button[type=submit]');
  await guestPage.waitForSelector('nav#nav a');
  check('the right code signs in', true);
  await guestPage.click('button:has-text("Sign out")');
  await guestPage.waitForSelector('form.login-card');
  await signIn(guestPage, guest, 'guest-pass-1234', codes[0]);
  await guestPage.waitForSelector('nav#nav a');
  check('a recovery code signs in too', true);
  await guestPage.click('a.account-link');
  await guestPage.waitForSelector('button:has-text("Turn off…")');
  await guestPage.click('button:has-text("Turn off…")');
  await guestPage.fill('input[type=password]', 'guest-pass-1234');
  await guestPage.fill('input[autocomplete=one-time-code]', codes[1]);
  await guestPage.click('button.danger:has-text("Turn off")');
  await guestPage.waitForSelector('button:has-text("Set up two-factor authentication")');
  check('turning two-factor off (password + recovery code) works', true);

  // ================================================================================================ single sign-on
  console.log('\n## single sign-on');
  const sso = await newPage();
  const person = `sso${rnd()}@example.com`;
  idp.identity = { sub: `sub-${rnd()}${rnd()}`, email: person, email_verified: true, given_name: 'Sam', family_name: 'Ssoer' };
  await sso.goto(ADMIN);
  await sso.waitForSelector('a.sso-button');
  await sso.click('a.sso-button:has-text("Sign in with Mock")');
  await sso.waitForSelector('nav#nav a');
  check('"Sign in with Mock" goes through the provider and lands in the app', (await sso.textContent('.topbar')).includes(person), sso.url());
  check('…with no code or token left in the address bar', !/code=|token=|access/.test(sso.url()), sso.url());
  await sso.reload();
  await sso.waitForSelector('nav#nav a');
  check('…and the session survives a reload', true);
  await sso.click('a.account-link');
  await sso.waitForSelector('h1:has-text("My account")');
  check('the account page says the user signs in through the organisation', /through your organisation \(mock\)/.test(await sso.textContent('body')));
  await sso.click('button:has-text("Sign out")');
  await sso.waitForSelector('form.login-card');

  idp.identity = { sub: `sub-${rnd()}${rnd()}`, email: `unv${rnd()}@example.com`, email_verified: false };
  await sso.waitForSelector('a.sso-button');
  await sso.click('a.sso-button:has-text("Sign in with Mock")');
  await sso.waitForSelector('h2:has-text("Sign-in failed")');
  check('a provider that does not vouch for the email ends on a clear failure page', /did not confirm your email/.test(await sso.textContent('.error-box')));
  await sso.click('a:has-text("Back to sign in")');
  await sso.waitForSelector('form.login-card');
  check('…with a way back to the sign-in page', true);

  await sleep(100);
  console.log('\nbrowser errors:', errors.length ? `\n  ${[...new Set(errors)].join('\n  ')}` : 'none');
  check('no JavaScript errors in the console', errors.length === 0, errors.slice(0, 3));
  ok = summary();
} catch (err) {
  console.error(err);
  check('the browser auth suite did not crash', false, String(err).split('\n')[0]);
  summary();
} finally {
  if (browser) await browser.close();
  await env.stop();
  smtp.stop();
  idp.stop();
}
process.exit(ok ? 0 : 1);
