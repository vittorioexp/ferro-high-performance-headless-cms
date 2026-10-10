# Tests

| Layer | Where | Run | Needs |
|---|---|---|---|
| Rust unit tests | `crates/*/src` | `cargo test --workspace` | Rust |
| Admin UI unit tests (fake DOM, simulated server) | `admin/test` | `cd admin && node --test test/*.test.mjs` | Node ≥ 20 |
| Integration (two real environments) | `tests/e2e` | `node tests/e2e/run.mjs [suite words]` | built binary, PostgreSQL, `psql` |
| Real browser | `tests/browser` | `cd tests/browser && npm i && node run.mjs && node auth.mjs` | + Chromium / Edge |
| Everything | `scripts/ci.sh [browser]` | | all of the above |

## Integration suites (`tests/e2e`)

`run.mjs` creates the databases `ferro_e2e_a` and `ferro_e2e_b` (dropping them first), starts two servers on
ports 18055/18056 with the example project applied, runs the suites in order, stops everything and exits non-zero
on any failure. Server logs: `target/e2e/*.log`.

| Suite | Covers |
|---|---|
| `00-core` | nested M2A writes and reads, filters, uniqueness, hooks, custom endpoints, files, editor permissions, token rotation, realtime smoke |
| `01-schema` | renames, type changes, destructive changes, rollbacks, optimistic lock, name validation, referential actions, history |
| `02-sync` | schema and data promotion A→B: M2M, natural keys, prune, strategies, conflicts, bundles, files, protected environments |
| `03-flows` | outbound requests, data chain, secrets, loops, conditions, webhooks (secret, async), chaining, `run_as`, blocking hooks, SSRF |
| `04-graphql` | variables, fragments, directives, singleton, many-mutations, atomicity, GET, errors, permissions |
| `05-security` | SQL injection, forged JWTs, mass assignment, row/field permissions, filter/sort/search/count oracles, path traversal, malformed input, brute force |
| `06-realtime` | subscriptions, filters, per-client permissions, validation, limits, suspended users, two instances, schema propagation |
| `07-storage` | S3-compatible bucket next to the disk: upload/replace/delete, 12 MB file, signed redirects (tampered signature, other key), permissions before signing, orphan cleanup, multi-location, `ferro storage list/check/migrate`, startup errors, promotion bucket → disk and back. Skipped unless `E2E_S3_ENDPOINT` is set |
| `09-auth-email` | two-factor authentication (RFC 6238 codes, replay, recovery codes spent once under concurrency, secrets encrypted and never exposed, throttling, admin / CLI reset), password reset by email (no account enumeration, Host-header poisoning, single use, expiry, sessions ended, throttle), invitations (renewal, fallback link, token kinds not interchangeable) against a fake SMTP server |
| `10-auth-sso` | single sign-on against a mock OpenID provider: PKCE, state cookie, open redirects, first sign-in rules (registration, domains, verified email, pre-created / invited / local / bound accounts), forged and replayed ID tokens (`none`, HMAC key confusion, wrong key / aud / iss / nonce / azp / expiry), key rotation, userinfo, OAuth 2.0 without an ID token, provider outages |
| `11-auth-ldap` | directory login against a real LDAP server (ldapjs): search-then-bind, filter injection, empty-password pitfall, ambiguous entries, account binding by entryUUID, second factor, lockout, outages, `ldaps://` with and without certificate verification |
| `12-translations` | guided set-up (dry run, validation, seeding), the composite UNIQUE constraint and 8 concurrent creates, writing by language code (partial update, null, unknown language, rollback, reserved keys, operations / array forms), reading with `lang` (fallback chain, per-field, defaults on the item, stable shape, lists, GraphQL), permissions on the translations collection, language lifecycle (RESTRICT / CASCADE), promotion with merge per language |
| `08-images` | resize, every fit mode, cover position, crop, JPEG / PNG / WebP output, quality, transparency, validation (17 invalid requests), non-images and corrupt files, a decompression bomb, permissions, `ETag` / `304`, a burst of 24 concurrent transformations; pixels are checked by decoding the real output |

Storage suite: needs an S3-compatible server with a bucket, e.g. SeaweedFS (`weed server -s3 -s3.port=19000 -s3.config=s3.json`,
bucket created with `curl -X POST http://127.0.0.1:8888/buckets/ferro-test/`) and then
`E2E_S3_ENDPOINT=http://127.0.0.1:19000 E2E_S3_FILER=http://127.0.0.1:8888 node tests/e2e/run.mjs storage`
(also `E2E_S3_BUCKET`, `E2E_S3_KEY`, `E2E_S3_SECRET`; the filer URL is optional and lets the suite look into the bucket independently).
MinIO's community binaries are no longer distributed, hence SeaweedFS. The Azure driver is covered by unit tests against a fake blob service only.

The identity suites need no external service: the SMTP server, the OpenID provider and the LDAP server are small programs in
`tests/e2e` (`identity.mjs`, `mock-idp.mjs`, `mock-ldap.mjs`). Run `npm install` in `tests/e2e` once (it installs `ldapjs`); the
`ldaps://` checks also need the `openssl` binary and are skipped without it.

Environment variables: `E2E_PG` (default `postgres://ferro@localhost:55432`), `FERRO_BIN` (default `target/debug/ferro`).
Pass words to run only some suites: `node tests/e2e/run.mjs security realtime`.

## What is not covered

- Load and soak tests; behaviour with more than a few thousand items per collection.
- Azure Blob against the real service, S3 against real AWS (only an S3-compatible server is exercised); no multipart upload for large objects.
- Docker image, compose file and the Jenkins example have never been built or executed here.
- Only PostgreSQL 16 on Windows (and Linux in CI) has been exercised.
- No independent security audit.
