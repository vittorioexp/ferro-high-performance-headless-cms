# Ferro CMS

A headless CMS written in Rust, inspired by Directus and built around **promoting the
data model, permissions and content between environments** (dev → test → prod).

- **Schema as code**: collections, fields, relations, roles and policies as YAML files
  versioned in git; transactional `diff` / `plan` / `apply` with risk levels and an
  optimistic lock.
- **Stable ids** (UUID v7) everywhere: the same content has the same id in every environment.
- **Content promotion** with automatic dependency closure (M2O, O2M, M2M, M2A, files,
  folders), natural keys, conflict strategies, prune, and a dry run with a
  field-by-field report.
- A REST API that follows Directus's concepts (`/items`, `fields`, `filter`, `{ data }`).
- **Flows** live in the snapshot (and are promoted between environments like everything
  else): outgoing webhooks, blocking hooks that validate or rewrite writes, scheduled
  jobs, custom endpoints and manual actions; Admin UI **extensions** (pages, field
  widgets, actions).
- **Realtime** over WebSocket (`/websocket`): subscriptions to a collection with fields
  and filters, delivered with each client's own permissions, across instances too.
- **File storage** on disk, on **S3 and compatibles** (MinIO, R2, B2, Wasabi…) or on
  **Azure Blob**, even several locations at once; optional signed-URL downloads;
  `ferro storage migrate` moves files between locations (`FERRO_STORAGE_*`, see the docs).
- **Image transformations** on `/assets/:id`: resize, crop, fit modes, JPEG / PNG / WebP output and quality, with
  limits against abuse, an in-memory cache and `ETag` / `304` (`?width=300&height=200&fit=cover&format=webp`).
- **GraphQL** with the same permissions (`POST /graphql`, SDL schema at `/graphql/schema`):
  typed relations, M2A blocks as unions (`... on block_text`), transactional mutations.

The complete design document is **[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)**.

## Layout

```
crates/
  ferro-core     model, validation, diff/plan, filters, bundles, reports (no database I/O)
  ferro-engine   PostgreSQL: DDL, queries, items, permissions, auth, files, sync
  ferro-storage  storage drivers: disk, S3 and compatibles, Azure Blob
  ferro-server   HTTP API (axum)
  ferro-cli      the `ferro` binary: server + environment tools
admin/           Admin UI (plain JavaScript, no build step), served on /admin
examples/site    example project (ferro.toml + schema/)
deploy/          docker-compose, Dockerfile, example Jenkinsfile
docs/            design document
```

## Quick start

Requirements: Rust ≥ 1.80, PostgreSQL ≥ 13 (or Docker).

```bash
# database + server
docker compose -f deploy/docker-compose.yml up -d postgres
cp .env.example .env            # and set FERRO_SECRET
export $(grep -v '^#' .env | xargs)
cargo run -p ferro-cli -- serve

# Admin UI: http://localhost:8055/admin (FERRO_ADMIN_DIR=./admin, sign in with FERRO_ADMIN_EMAIL / FERRO_ADMIN_PASSWORD)

# in another terminal: a token for the CLI
cargo run -p ferro-cli -- user token --email admin@example.com
export FERRO_LOCAL_TOKEN=ferro_...

# apply the example model
cd examples/site
cargo run -p ferro-cli -- schema diff  --to local
cargo run -p ferro-cli -- schema apply --to local

# content
curl -H "Authorization: Bearer $FERRO_LOCAL_TOKEN" -H 'content-type: application/json' \
  -d '{"title":"Home","slug":"home","blocks":[{"collection":"block_hero","item":{"headline":"Hello"},"sort":1}]}' \
  http://localhost:8055/items/pages
curl -H "Authorization: Bearer $FERRO_LOCAL_TOKEN" \
  'http://localhost:8055/items/pages?fields=*,blocks.item.*&filter={"slug":{"_eq":"home"}}'
```

## Typical flows between environments

```bash
ferro env status                                        # schema hash of every environment
ferro schema pull  --from dev                           # dev -> schema/ (commit it to git)
ferro schema diff  --to prod --report schema-diff.html  # what would change in prod
ferro schema apply --to prod                            # asks for confirmation (protected environment)

ferro data diff --from dev --to prod --select 'pages:{"slug":{"_eq":"home"}}' --report diff.html
ferro data push --from dev --to prod --select 'pages:{"slug":{"_eq":"home"}}' --strategy safe --prune
```

## Tests

```bash
scripts/ci.sh [browser]   # everything: build, Rust tests, Admin UI, PostgreSQL integration (and browser) — see tests/README.md
cargo test                # unit tests of the crates (no database needed)
(cd admin && node --test test/*.test.mjs)   # Admin UI tests (Node >= 20)
```

> Status: builds without warnings; 71 Rust tests + 30 Admin UI tests + 358 integration
> checks + 36 browser checks, run end to end against PostgreSQL 16 with two environments,
> an S3-compatible server and a real browser (see "Known limits" in
> docs/ARCHITECTURE.md). The CI workflow is written but has not yet run on a real runner.
