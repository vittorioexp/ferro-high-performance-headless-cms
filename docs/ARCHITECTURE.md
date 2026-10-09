# Ferro CMS — Design Document

> A headless CMS written in Rust, inspired by Directus and designed around one
> requirement that Directus does not meet well: **promoting the data model,
> permissions and content between environments (dev → test → prod) safely,
> repeatably and verifiably.**

Contents

1. [Why: the problems to solve](#1-why-the-problems-to-solve)
2. [Design principles](#2-design-principles)
3. [Architecture](#3-architecture)
4. [Data model](#4-data-model)
5. [Persistence on PostgreSQL](#5-persistence-on-postgresql)
6. [Schema as code: diff, plan, apply](#6-schema-as-code-diff-plan-apply)
7. [Moving content between environments](#7-moving-content-between-environments)
8. [REST API](#8-rest-api)
9. [Authentication and permissions](#9-authentication-and-permissions)
10. [Files and assets](#10-files-and-assets)
11. [The `ferro` CLI](#11-the-ferro-cli)
12. [CI/CD and workflow](#12-cicd-and-workflow)
13. [Deployment, operations, security](#13-deployment-operations-security)
14. [Migrating from Directus](#14-migrating-from-directus)
15. [Admin UI](#15-admin-ui)
16. [Status, limits and roadmap](#16-status-limits-and-roadmap)

---

## 1. Why: the problems to solve

Running Directus across several environments has produced a collection of
stopgap tools. Each one corresponds to a structural problem that Ferro solves
in its design rather than with external scripts:

| Current tool | Problem in Directus | Solution in Ferro |
|---|---|---|
| `diff`, `diff-model`, `diff-schema`, `live-diff` | The model lives only in the database; comparing environments means downloading `/fields` and filtering out the noise | The model is a **versioned YAML file** (one per collection). `ferro schema diff --from dev --to prod` returns an ordered, semantic plan with a risk level; `--report` produces an HTML version |
| `diff-policies`, `policies-porting`, `policy-pilot` | Roles, policies and permissions have different ids in each environment and must be recreated by hand | Roles and policies are part of the **snapshot**, with **stable UUIDs**: they are promoted like the model |
| `data-porting`, `migrate-data`, `import-data`, `migrate-blocks-*` | Auto-increment ids differ between environments, dependencies must be resolved by hand (`idToImport` per collection), folders must already exist | **All ids are UUID v7**, generated once: the same content has the same id everywhere. `ferro data push` computes the **dependency closure** (M2O, O2M, M2M, M2A, files, folders) by itself |
| `diff-data` (one Jenkinsfile per collection) | No native way to see what would change | `ferro data diff` is an import in **dry-run mode**: a field-by-field report, usable in pipelines too |
| `fix-sorting-m2a` | M2A junctions with inconsistent sort values after partial ports | Import with `--prune`: children removed in the source are removed or detached in the target; parents exported "complete" are flagged in the bundle |
| `create-system-fields` | System fields have to be added by hand, `_translations` collections included | `date_created`, `user_created`, `date_updated`, `user_updated` are **implicit** (`system_fields: true` by default) |
| `wrong-cdn-links-detector`, files | Files are tied to a single storage/environment | The binary's key is the file id; sync transfers only the missing binaries (SHA-256 checksum) |
| `directus-from-dump` | Getting an aligned environment means restoring a dump | `ferro schema apply` + `ferro data push` rebuild an environment from git + a source |
| `user-activity-audit` | — | `ferro_activity` + `ferro_revisions` for every write, with an `origin` (`api`, `sync`) |

## 2. Design principles

1. **Stable identity.** Every item has an `id uuid` (v7, time-ordered). Never
   sequences. Ids do not change between environments, so relations stay valid
   after a copy. Where ids already diverge (content created by hand in two
   environments) you declare a **natural key** (`sync.natural_key: [slug]`) and
   the import remaps the ids.
2. **The model is code.** The snapshot (collections + fields + relations + roles +
   policies) is stored in git, reviewed in a pull request, and applied through a
   plan. Changes made in the Admin UI go through the **same pipeline** (modified
   snapshot → diff → apply), so the UI and git cannot drift apart: a single
   `ferro schema pull` brings a change made in the UI back into git.
3. **All or nothing.** PostgreSQL has transactional DDL: a schema plan is applied
   completely or not at all. A data import runs in one transaction with
   `DEFERRED` constraints: insertion order is irrelevant, cycles are allowed, and
   at the end **every foreign key is checked** before the commit.
4. **See first, then act.** Every write operation has a dry run that *really*
   executes everything (DDL and inserts included) and then rolls back: the report
   is exact, not an estimate. On `protected` environments the CLI shows the dry
   run first and asks for confirmation.
5. **Explainable.** Plans and reports are data (JSON) and documents (HTML) that
   can be attached to a build or a change request.
6. **Conceptual compatibility with Directus.** The same concepts (collection,
   field, M2O/O2M/M2M/M2A, policy, `_eq`/`_in`/… filters, `fields=a.b.*`,
   `{ data: … }`), to flatten the learning curve and make migrating frontends
   easier.

## 3. Architecture

```
                ┌──────────────────────────────┐
  Admin UI ───► │                              │
  Frontend ───► │   ferro-server (axum)        │  REST /items /collections /schema /sync /files ...
  ferro CLI ──► │   auth · routing · errors    │
                └──────────────┬───────────────┘
                               │
                ┌──────────────▼───────────────┐
                │   ferro-engine               │
                │  items (CRUD, expansion,     │   permissions · auth · activity
                │   nested writes)             │   files (Storage trait)
                │  query (filters → SQL)       │   sync (export / import)
                │  ddl (plan → SQL)            │   schema_store (transactional apply)
                └──────┬────────────────┬──────┘
                       │                │
                ┌──────▼──────┐   ┌─────▼──────┐
                │ PostgreSQL  │   │  Storage   │  local · S3 · Azure Blob
                └─────────────┘   └────────────┘

                ┌──────────────────────────────┐
                │   ferro-core (no DB I/O)     │  model · validate · diff · filter
                │   used by engine and CLI     │  fields · graph · bundle · project · report
                └──────────────────────────────┘
```

| Crate | Responsibility |
|---|---|
| `ferro-core` | Model types (snapshot), validation, **diff/plan**, in-memory filter evaluator, `fields` parsing, topological sorting, bundle and import-report formats, reading and writing the project's YAML, HTML reports. No database dependency: it can be tested in isolation. |
| `ferro-engine` | PostgreSQL and storage: system migrations, DDL from plans, compilation of filters into parameterised SQL, the items service, permissions, authentication, files, sync. |
| `ferro-storage` | Storage drivers: `local`, **S3 and compatibles** (AWS SDK, standard credential chain, signed URLs), **Azure Blob** (REST + SAS); configuration from `FERRO_STORAGE_*`. Keeps the heavy dependencies out of the engine. |
| `ferro-identity` | Single sign-on (OpenID Connect, OAuth 2.0), LDAP login and outgoing email (SMTP); configuration from `FERRO_AUTH_*`, `FERRO_LDAP_*`, `FERRO_SMTP_*`. Keeps the LDAP, mail and TLS dependencies out of the engine. |
| `ferro-server` | HTTP API (axum 0.7), identity extraction, error mapping, CORS, limits, `LISTEN/NOTIFY` listener that reloads the schema. |
| `ferro-cli` | The `ferro` binary: `serve` plus environment-management commands (schema/data) over HTTP, and `storage list/check/migrate` (which work directly on the database and storage). |

**A single binary** (`ferro`) for the server and the tools: the same artifact runs
in production and in pipelines, with the same version of the snapshot/bundle format.

### Life of a request

1. The `Acc` extractor resolves the caller (JWT or static token, otherwise the
   `public` policy) and builds the `Accountability` (user, role, policies).
2. The handler takes an immutable `Arc<Schema>` (an atomic in-memory copy of the
   model) and calls the items service.
3. The service computes the effective permissions (`permissions::access`),
   compiles the user filter + the permission filter into parameterised SQL, runs
   the query, which returns JSON directly (`jsonb_agg(to_jsonb(...))`), and then
   expands the requested relations with batch queries (one per relation, never N+1).
4. Writes happen in a transaction; activity and revisions are written in the same
   transaction.

## 4. Data model

### 4.1 Snapshot

The snapshot is the complete, environment-independent configuration:

```yaml
version: 1
collections: [...]   # model
roles: [...]         # roles (stable ids)
policies: [...]      # policies and permissions (stable ids)
```

On disk it is a directory with one file per object, to keep diffs readable in pull requests:

```
schema/
  collections/pages.yaml
  collections/pages_blocks.yaml
  roles/editor.yaml
  policies/editor.yaml
```

The snapshot's **hash** (SHA-256 of the canonical JSON, with normalised key order)
identifies the state of an environment: two environments with the same hash have
exactly the same model and the same permissions (`ferro env status`).

### 4.2 Collection

```yaml
name: pages
meta:
  icon: article
  display_template: "{{title}}"
  sort_field: title          # default ordering
  singleton: false           # true = a single item (e.g. site settings)
  revisions: true            # full item history
  translations: { it-IT: Pagine }
sync:
  natural_key: [slug]        # match across environments when ids differ
  exclude: false             # true = never exported (environment-specific data)
system_fields: true          # date_created, user_created, date_updated, user_updated
fields: [...]
```

`id` (uuid, primary key) is implicit and is not declared.

### 4.3 Field types

| `type` | PostgreSQL column | Notes |
|---|---|---|
| `string` | `varchar(max_length)` (255) | |
| `text` | `text` | |
| `integer` / `bigint` | `integer` / `bigint` | |
| `float` | `double precision` | |
| `decimal` | `numeric(precision, scale)` (10,5) | |
| `boolean` | `boolean` | |
| `uuid` | `uuid` | |
| `json` | `jsonb` | `_contains` operators on arrays/objects |
| `date` / `datetime` / `time` | `date` / `timestamptz` / `time` | |
| `m2o` | `uuid` + `DEFERRABLE` FK + index | `relation.related`, `relation.on_delete` |
| `file` | `uuid` + FK to `ferro_files` | |
| `m2a_item` | `uuid` + index | column of the M2A junction: `relation.collection_field`, `relation.allowed` |
| `o2m` / `m2m` / `m2a` | none (alias) | `relation.related` + `relation.foreign_field` |

Common attributes: `required` (NOT NULL), `unique`, `indexed`, `default` (a
literal or `$NOW`), `validation` (a condition using the filter operators, e.g.
`{ _nempty: true }`), `meta` (interface, options, display, note, width, group,
translations: used only by the Admin UI).

### 4.4 Relations

Every "to-many" relation is an **o2m towards a child collection or an explicit
junction**. Junctions are not generated behind the scenes: they are declared
collections, so you can see them in the snapshot, diff them and migrate them.

```yaml
# M2O
- { name: category, type: m2o, relation: { related: categories, on_delete: set_null } }

# O2M (inverse of an m2o on the child)
- { name: comments, type: o2m, relation: { related: comments, foreign_field: page, sort_field: sort } }

# M2M: o2m to the junction + which m2o of the junction points to the other side
- { name: tags, type: m2m, relation: { related: pages_tags, foreign_field: pages_id, junction_field: tags_id } }

# M2A (block-based page builder): o2m to the junction
- { name: blocks, type: m2a, relation: { related: pages_blocks, foreign_field: pages_id, sort_field: sort } }
```

M2A junction:

```yaml
name: pages_blocks
system_fields: false
fields:
  - { name: pages_id, type: m2o, relation: { related: pages, on_delete: cascade } }
  - { name: collection, type: string, required: true }
  - name: item
    type: m2a_item
    relation: { collection_field: collection, allowed: [block_hero, block_text] }
  - { name: sort, type: integer }
```

**Translations** use an explicit pattern: a `pages_translations` collection
(m2o `pages_id`, m2o `languages_code`) and an `o2m` from the parent. There is no
special type, so diff and migration work as for any other relation.

### 4.5 Snapshot validation

`ferro schema validate` (and every apply) checks: valid identifiers
(`[A-Za-z_][A-Za-z0-9_]{0,62}`), the reserved `ferro_` prefix, duplicates, `id`
and system fields not redeclared, consistent relations (existing targets, a
`foreign_field` that really is an m2o towards the parent, junctions with an m2o
`junction_field`, M2A with an `m2a_item`, existing `allowed` collections),
`required` combined with `on_delete: set_null` (incompatible), natural keys on
column fields, permissions on existing collections/fields, and roles that point
to existing policies.

## 5. Persistence on PostgreSQL

### 5.1 System tables

Created by internal versioned migrations (`ferro_system_migrations`), run at
startup under an advisory lock (safe with several instances).

| Table | Contents |
|---|---|
| `ferro_collections`, `ferro_roles`, `ferro_policies` | The current snapshot (JSONB definitions) |
| `ferro_schema_history` | Every plan applied: from/to hash, plan, resulting snapshot, author, date |
| `ferro_users`, `ferro_sessions` | Users (argon2id) with their sign-in provider and identifier, encrypted two-factor secret and hashed recovery codes; refresh tokens (hash only), static tokens (hash only) |
| `ferro_auth_tokens` | Single-use tokens (hash only): password reset, invitation, single sign-on hand-over; with their purpose and expiry |
| `ferro_files`, `ferro_folders` | File metadata, nested folders |
| `ferro_activity`, `ferro_revisions` | Audit of every write; revisions with `data` (the item afterwards) and `delta` (the payload) |
| `ferro_flows` · `ferro_flow_queue` · `ferro_flow_runs` | Flow definitions (part of the snapshot), job queue, execution history |
| `ferro_sync_state` | For each imported item: the content hash at the last sync (`safe` strategy) |
| `ferro_settings` | Project settings |

System collections are exposed to the items engine with synthetic definitions that
**do not include** `password_hash` and `token_hash`: they cannot be selected through
the API, not even by mistake.

### 5.2 Deterministic constraint names

`fk_<hash16(table.column)>`, `uq_…`, `ix_…`: identical in every environment. The
DDL generator always knows which constraint to drop or rename (even after
collection or field renames, which rename the constraints accordingly).

### 5.3 `DEFERRABLE INITIALLY IMMEDIATE` foreign keys

In normal use they behave like ordinary foreign keys; during an import the engine
runs `SET CONSTRAINTS ALL DEFERRED` and inserts in any order, then
`SET CONSTRAINTS ALL IMMEDIATE` checks everything before the commit (also in dry-run mode).

## 6. Schema as code: diff, plan, apply

### 6.1 Diff

`diff(current, desired) → Plan`:

- collections: create / drop / **rename** (`renamed_from` hint) / update meta, sync and system_fields
- fields: add / drop / **rename** (`renamed_from` hint) / alter (type, null, default, unique, index, relation, validation, meta)
- roles and policies: compared by **id**

`renamed_from` hints are one-shot: they tell a rename apart from a drop + add
(which would lose data), are applied, and are then removed from the saved snapshot.

Every change has a **risk** level:

| Risk | Examples |
|---|---|
| `safe` | new collection, new nullable field, meta, type widening (`varchar(100)→varchar(200)`, `integer→bigint`, `string→text`) |
| `risky` | a `required` field added to an existing table with no default, `unique` on existing data: these can fail on the data (the dry run finds out) |
| `destructive` | dropping collections or column fields, a non-widening type change, `system_fields` switched off |

### 6.2 Plan → SQL

Ordered phases (all in a single transaction):

1. rename tables (+ constraints) → 2. rename columns (+ constraints) → 3. drop
changed constraints → 4. drop columns → 5. drop tables → 6. create tables →
7. add columns → 8. alter columns → 9. add constraints/indexes

### 6.3 Apply

`POST /schema/apply` / `ferro schema apply`:

1. advisory lock (one apply at a time per database)
2. validation of the desired snapshot
3. **optimistic lock**: `expected_hash` must match the target's current hash (the
   reviewed plan is the one that gets applied; if someone changed the target in
   the meantime, the apply is refused)
4. `destructive` changes require `allow_destructive`
5. DDL + snapshot save + a row in `ferro_schema_history` + `NOTIFY ferro_schema`
6. commit (or rollback with `dry_run`)

The other instances receive the `NOTIFY` and reload the schema in memory.

## 7. Moving content between environments

### 7.1 Bundle

```
bundle/
  bundle.json   # format ferro-bundle/1: items per collection, definitions of the
                # collections involved, folders, file metadata, "complete" parents
  files/<id>    # binaries
```

The bundle contains the **definitions** of the collections it carries: the import
checks compatibility field by field (existence and type) **before** writing
anything. If the model is not aligned, the error tells you to run
`ferro schema apply` first.

### 7.2 Selection

```
--select pages                                  # the whole collection
--select 'pages:id=0199…,0199…'                 # by id
--select 'pages:{"slug":{"_in":["home","about-us"]}}'   # by filter
--shallow                                       # only the items, without dependencies
```

### 7.3 Dependency closure (export)

Every visited item has a level: **shallow** (needed as a dependency) or **deep**
(its "owned" content is wanted as well).

| Relation | Default rule | Alternative `sync:` |
|---|---|---|
| roots of the selection | deep | — |
| `m2o` | target included **shallow** (its m2o are followed, its children are not) | `deep` (children too), `reference` / `skip` (not exported, must exist in the target) |
| `o2m` / `m2m` / `m2a` of a deep item | children/junction rows included **deep** | `reference` / `skip` |
| `m2a_item` (blocks) | target included **deep** (composition) | `reference` / `skip` |
| `file` | `ferro_files` row + binary + chain of folders | — |
| users | never exported; `user_created/updated` removed, other references to users that do not exist in the target → `null` (with a warning) | — |
| collections with `sync.exclude` | never exported | — |

Example: `--select 'pages:{"slug":{"_eq":"home"}}'` exports the page, its category
(shallow: not all the pages of the category), the `pages_blocks` rows, each block
with its children, and the images with their folders.

### 7.4 Import

In one transaction with deferred constraints:

1. format and schema compatibility (errors → nothing is written)
2. **natural key**: for items whose id does not exist in the target, if an item
   with the same natural key exists the target's id is adopted, and every
   reference in the bundle (m2o, m2a_item) is rewritten
3. references to users that do not exist → `null`
4. folders, files (the binary must already be in the target's storage)
5. collections in topological order; for each item:
   - does not exist → `create`
   - identical (ignoring `date_*`/`user_*`) → `unchanged`
   - different → depends on the **strategy**:

| Strategy | Item that differs in the target |
|---|---|
| `overwrite` (default) | updated |
| `safe` | updated only if it has not been modified in the target since the last sync (hash in `ferro_sync_state`), otherwise **conflict** |
| `insert-only` | `skip` |

6. `--prune`: for deep exported parents, children that exist in the target but not
   in the source are **deleted** (junctions, children with a mandatory FK) or
   **detached** (nullable FK)
7. `SET CONSTRAINTS ALL IMMEDIATE`: every FK must be valid (an item that points to
   something that is neither in the bundle nor in the target makes the import fail
   with an explicit message)
8. with conflicts and without `--force` → rollback; with `dry_run` → rollback;
   otherwise commit + an `import` activity with the summary

For each item that is not unchanged, the report contains the action and the
field-by-field differences (before/after), plus the source id if it was remapped.

### 7.5 Commands

```bash
# what would change in prod (nothing is written)
ferro data diff --from dev --to prod --select 'pages:{"slug":{"_eq":"home"}}' --report diff.html

# one-step promotion (files included, only the missing ones)
ferro data push --from dev --to prod --select pages --strategy safe --prune

# via files (e.g. manual approval, environments that cannot reach each other)
ferro data export --from dev --select pages --out ./bundle-home
ferro data import --to prod --bundle ./bundle-home --dry-run --report import.html
ferro data import --to prod --bundle ./bundle-home --yes
```

The import is **idempotent**: repeating it produces only `unchanged`.

## 8. REST API

Response format `{ "data": …, "meta": … }`, errors
`{ "errors": [{ "message": …, "extensions": { "code": … } }] }`.

### 8.1 Items

| Method and path | |
|---|---|
| `GET /items/:collection` | list (`fields`, `filter`, `sort`, `limit`, `offset`, `page`, `search`, `meta=total_count,filter_count`) |
| `GET /items/:collection/:id` | single item |
| `POST /items/:collection` | create (object or array); singleton: upsert |
| `PATCH /items/:collection/:id` | update |
| `PATCH /items/:collection` | batch: `{ keys: [...], data: {...} }` or `[{ id, ... }]` |
| `DELETE /items/:collection/:id`, `DELETE /items/:collection` (`[ids]`) | delete |

**fields**: `*`, `title,author.name`, `blocks.item.*`, `*.*`. Aliases (o2m/m2m/m2a)
with `*` return the list of ids; with a sub-tree they return the objects. For M2A
the sub-tree applies to every target collection (non-existent fields are ignored).

**filter** (JSON): `_eq _neq _lt _lte _gt _gte _in _nin _null _nnull _empty _nempty
_contains _ncontains _icontains _nicontains _starts_with _nstarts_with _istarts_with
_ends_with _nends_with _iends_with _between _nbetween`, logical `_and _or`,
relational `{ author: { name: { _eq: … } } }` (m2o), `{ blocks: { _some: … } }` /
`_none` (o2m). Variables: `$CURRENT_USER`, `$CURRENT_ROLE`, `$NOW`.

**Nested writes**:

```json
{
  "title": "Home",
  "category": { "code": "corporate" },
  "blocks": [
    { "collection": "block_hero", "item": { "headline": "Welcome" }, "sort": 1 },
    { "id": "0199…", "sort": 2 }
  ]
}
```

- m2o with an object: creates (or updates, if it has an existing `id`) and links
- o2m/m2m/m2a as an **array**: full replacement (missing elements are deleted if
  the junction/FK is mandatory, otherwise detached)
- o2m as an **object** `{ create: [], update: [], delete: [] }`: partial change
- `m2a_item` with an object: creates the item in the collection named by `collection`

Everything runs in one transaction, with permissions checked at every level.

### 8.2 Other resources

| Area | Endpoints |
|---|---|
| Auth | `POST /auth/login` `{email,password,otp?}` → `{access_token, refresh_token, expires}` (`OTP_REQUIRED` / `INVALID_OTP` with two-factor), `POST /auth/refresh`, `POST /auth/logout`, `GET /auth/methods` (what the sign-in page may offer) |
| Account security | `POST /auth/password/request`, `POST /auth/password/reset`, `POST /auth/invite/accept`, `POST /users/invite`, `POST /auth/tfa/generate` · `enable` · `disable`, `DELETE /users/:id/tfa`; single sign-on: `GET /auth/oauth/:name`, `GET /auth/oauth/:name/callback`, `POST /auth/oauth/exchange` (see §9) |
| Model | `GET/POST /collections`, `GET/PATCH/DELETE /collections/:name`, `POST /fields/:collection`, `PATCH/DELETE /fields/:collection/:field` (query `allow_destructive`, `dry_run`) |
| Access | `GET /roles`, `PUT/DELETE /roles/:id`, `GET /policies`, `PUT/DELETE /policies/:id` |
| Schema | `GET /schema/snapshot` (`?format=yaml`), `POST /schema/plan`, `POST /schema/apply`, `GET /schema/history` |
| Users | `GET/POST /users`, `GET /users/me`, `GET/PATCH/DELETE /users/:id`, `POST /users/:id/token` |
| Files | `GET/POST /files` (multipart, `?replace=<id>`), `GET/PATCH/DELETE /files/:id`, `GET /assets/:id` (`?download`, `?access_token=`, image transformations §10), `GET/POST /folders` |
| Audit | `GET /activity`, `GET /revisions` |
| Sync | `POST /sync/export`, `POST /sync/missing-files`, `GET/PUT /sync/files/:id`, `POST /sync/import`, `DELETE /sync/state` |
| GraphQL | `POST /graphql`, `GET /graphql?query=…` (queries only), `GET /graphql/schema` (SDL), see §8.3 |
| Realtime | `GET /websocket` (WebSocket), see §8.5 |
| Server | `GET /server/health`, `GET /server/info` |

### 8.3 GraphQL

Same authentication, same permissions, same rules as REST: the root fields call
the same services (`items::read_many`, `create_one`, `update_one`…). The schema
(SDL) is generated from the model: `GET /graphql/schema`, or the "Download schema"
button of the playground in the Admin UI.

```graphql
query {
  pages(filter: { slug: { _eq: "home" } }, sort: ["-date_created"], limit: 10) {
    id
    title
    category { code }
    cover { id filename_download }
    blocks {
      collection
      item {
        __typename
        ... on block_text { body }
        ... on block_hero { headline image { id } }
      }
    }
  }
}

mutation {
  create_pages_item(data: { title: "New page", slug: "new-page",
      blocks: [{ collection: "block_text", item: { body: "Hello" }, sort: 1 }] }) { id }
}
```

| Root | Meaning |
|---|---|
| `<c>(filter, sort, limit, offset, page, search)` | list (`[<c>!]!`) |
| `<c>_by_id(id)` · `<c>_count(filter, search)` | single item or `null` · count |
| `<c>` (singleton) · `update_<c>(data)` | the single item |
| `create_<c>_item(s)(data)` | creation (nested writes as in REST) |
| `update_<c>_item(id, data)` · `update_<c>_items(ids, data)` | update |
| `delete_<c>_item(id)` · `delete_<c>_items(ids)` | delete → `{ id }` / `{ ids }` |

- Types are named after the collection; relations are typed (`category { code }`),
  and M2A fields (`item`) are **unions** of the allowed types, queried with
  `... on <collection>`.
- Filters and payloads use the `JSON` scalar with the same formats as REST
  (`{"status":{"_eq":"published"}}`, nested writes): a single vocabulary to learn.
- Supported: aliases, variables with defaults, fragments (named and inline),
  `@skip`/`@include`, `__typename`. All the mutations of a request run in **one
  transaction**.
- Errors are reported in `errors[]` (HTTP 200) with an `extensions.code`
  (`FORBIDDEN`, `NOT_FOUND`, …); a query can return partial data, while a mutation
  with an error cancels everything (`data: null`).
- Mutations over `GET` are rejected. Maximum selection depth: 32.
- **Not supported**: subscriptions; introspection (`__schema`/`__type`), so GraphiQL
  and tools that require it do not work: use the downloaded SDL (for example with
  `graphql-codegen`) or the Admin UI playground; the same field name with different
  aliases and different sub-selections in the same object (the last one wins).

### 8.4 Flows: webhooks, hooks, automations, custom endpoints

A single concept, the **flow**, covers what other systems split into webhooks,
flows and hooks. Flows are part of the **snapshot** (`schema/flows/*.yaml`, stable
UUID ids): they are reviewed in pull requests, compared (`ferro schema diff`) and
promoted between environments together with the model and the permissions.

| You want | Trigger | Example |
|---|---|---|
| An outgoing webhook | `event` + `request` operation | `notify-on-publish.yaml` |
| A hook that validates or rewrites a write | `event` with `blocking: true` | `require-title.yaml` |
| A scheduled job | `schedule` (cron, UTC) | `weekly-digest.yaml` |
| A custom endpoint | `webhook` (`/x/<slug>`, `/hooks/<id>`) | `hello-endpoint.yaml` |
| A button in the editor | `manual` | `unpublish.yaml` |

**Triggers.** `event`: `<collection>.items.<create|update|delete|*>` (also
`*.items.update`, `*`). `webhook`: methods, `access` (`public` | `authenticated` |
`admin`), `secret_env` (the name of an environment variable holding a shared secret,
expected in the `X-Ferro-Secret` header: the secret never enters the snapshot),
`run_async` (answers 202 and runs in the background).

**Operations** (`type`): `log`, `request` (HTTP), `item_create`, `item_update`,
`item_delete`, `item_read`, `condition`, `transform`, `trigger_flow`, `fail`. They
run along the `resolve` / `reject` links (not in list order); `condition` follows
`resolve` if the rule matches and `reject` otherwise; an error follows `reject` if
there is one, otherwise the flow fails.

**Data chain.** Every string option accepts `{{ path }}`: `$trigger` (event,
`payload`, `key`, `keys`, `item`, `query`, `body`, `headers`), `$last`, `$env`
(`FERRO_FLOW_ENV_<NAME>` variables), `$accountability`, and one entry for every
operation already executed (`{{ fetch.data.email }}`). A string that is exactly one
reference keeps its JSON type (numbers, arrays, objects). The `output` field is a
template rendered at the end of the flow: it is the HTTP response of an endpoint, or
the new payload of a blocking hook.

```yaml
id: 0199a000-0000-7000-8000-0000000c0001
name: Notify on publish
trigger: { type: event, events: [pages.items.update] }
operations:
  - key: only_published
    type: condition
    options: { rule: { $trigger: { payload: { status: { _eq: published } } } } }
    resolve: notify
  - key: notify
    type: request
    options:
      method: POST
      url: https://hooks.example.com/publish
      headers: { Authorization: "Bearer {{ $env.PUBLISH_HOOK_TOKEN }}" }
      body: { id: "{{ $trigger.key }}", title: "{{ $trigger.item.title }}" }
```

**Execution.**
- Non-blocking events are queued in `ferro_flow_queue` *in the same transaction as
  the write*: they exist only if the write succeeds. A worker on every instance
  picks them up with `FOR UPDATE SKIP LOCKED`: each job runs **exactly once**,
  whichever instance takes it.
- A flow is **one transaction**: if it fails, nothing it wrote is kept.
- Blocking hooks run inside the write, after the permission check; the rewritten
  payload is **validated again** (field permissions, validation rules) and an error
  cancels the write.
- Writes made by flows have the origin `flow` and **do not trigger other flows** (no
  loops); chaining is explicit (`trigger_flow`, at most 5 levels).
- Schedules fire once a minute (with a deduplication key); minutes missed while all
  the instances were down are not made up.
- Execution history (a log of every step) is kept in `ferro_flow_runs` for 30 days:
  `GET /flows/:id/runs` or Admin → Automations → Runs.
- `run_as`: `system` (default, unrestricted) or `caller` (the permissions of the user
  who caused the run: blocking hooks, synchronous webhooks, manual actions).

**Safety of outgoing requests.** Only http/https; private hosts are rejected by
default (localhost, 10/8, 172.16/12, 192.168/16, link-local, `.internal`, `.local`).
No redirects are followed, the timeout is 10 s and the response is limited to
1 MiB. `FERRO_FLOW_ALLOWED_HOSTS` restricts calls to the listed hosts only;
`FERRO_FLOW_ALLOW_PRIVATE=true` allows private addresses. Known limit: a DNS name
that *resolves* to a private address is not intercepted (use the allowed-hosts list).

| Endpoint | |
|---|---|
| `GET /flows` · `PUT/DELETE /flows/:id` | management (admin), with a plan preview like the model |
| `POST /flows/:id/run` | manual run `{collection, keys, payload}` |
| `GET /flows/:id/runs` | history |
| `GET /flow-actions?collection=` | manual actions the user can run |
| `ANY /hooks/:id` · `ANY /x/:slug` | incoming endpoints of `webhook` flows |

### 8.5 Realtime (WebSocket)

`GET /websocket` opens a connection on which the client subscribes to the changes
of a collection. JSON messages (text frames):

```text
→ { "type": "auth", "access_token": "…" }        (or ?access_token= in the URL; without a token: public policies)
← { "type": "auth", "status": "ok" | "error" }
→ { "type": "subscribe", "uid": "a", "collection": "pages", "event": "update",
    "query": { "fields": ["title", "blocks.*"], "filter": { "status": { "_eq": "published" } } } }
← { "type": "subscription", "event": "init", "uid": "a" }
← { "type": "subscription", "event": "create|update|delete|resync", "uid": "a", "data": [ … ] }
→ { "type": "unsubscribe", "uid": "a" }     → { "type": "ping" }  ←  { "type": "pong" }
← { "type": "error", "uid": "a", "code": "NOT_FOUND|FORBIDDEN|INVALID_PAYLOAD|LIMIT", "message": "…" }
```

**How it works.** Every item write (REST, GraphQL, flows) publishes with
`pg_notify`, **inside the same transaction**, a minimal notification (collection,
action, at most 40 ids): PostgreSQL delivers it only if the transaction succeeds,
and to all instances. Each instance forwards it to its own clients. A cancelled
write (constraint violated, blocking hook that refuses) produces no event.

**Security.** The notification carries no data: for each subscription the server
**re-reads** the items with that client's permissions, the requested fields and
(in AND) the subscription's filter. A client never sees what it could not read over
REST: an anonymous client with the public policy receives only published pages, an
editor only the allowed fields. The identity is re-checked on **every delivery**: an
expired token or a suspended user is disconnected at the next event.

**Semantics.**
- `create`/`update`: `data` = the items (the subscription's fields). An item that
  leaves the subscription's filter because of the update generates no event.
- `delete`: `data` = the ids (the item no longer exists; the filter cannot be evaluated).
- `resync`: events may have been lost (the server's connection to the database was
  interrupted, a client was too slow): re-read. `truncated: true`: the write touched
  more than 40 items.
- Limits: 50 subscriptions per connection, 2000 connections per instance, messages up
  to 64 KB, a server ping every 30 s and disconnection after 100 s of silence. System
  collections cannot be subscribed to.
- These do not generate events: data imports (`ferro data import/push`) and files
  (`/files`). After an import, clients see the data on their next read.

**Admin UI.** Lists and editors subscribe by themselves and show a notice ("another
user changed…", with a button to reload) without ever overwriting what the user is
typing; the user's own save does not produce the notice. Extensions use
`ferro.realtime.subscribe(...)` and `ferro.onLeave(...)` (see
`examples/site/extensions/hello-panel`).

## 9. Authentication and permissions

- **Password**: argon2id. **Access token**: JWT HS256 (15 min, `FERRO_SECRET`).
  **Refresh token**: random, rotated on every use, stored only as a hash.
- **Static tokens** (`ferro user token`, `POST /users/:id/token`): for CI and for
  the CLI between environments; stored as SHA-256, shown only once.
- **Role → policy**: a user has a role, and a role has N policies. Policies with
  `public: true` also apply to anonymous callers and are added to those of the role.
- **Permission** = (collection, action `create|read|update|delete`):
  - `fields`: readable/writable fields (`*` = all)
  - `filter`: a rule on the items (e.g. `{ user_created: { _eq: $CURRENT_USER } }`)
  - `validation`: a condition the payload must satisfy
  - `presets`: default values on creation
- Several permissions on the same pair are **merged**: fields are unioned, filters
  are OR-ed (a permission without a filter gives access to every item).
- `admin_access` bypasses everything and is required for schema, sync and users.
- The **Administrator** role and policy have fixed ids, identical in every environment.

### Ways to sign in

| Method | For | Configured with |
|---|---|---|
| Email + Ferro password | Everybody by default; API tokens for machines | — |
| **OpenID Connect / OAuth 2.0 single sign-on** | Microsoft Entra ID, Google, Okta, Keycloak, Auth0, GitHub… | `FERRO_AUTH_PROVIDERS`, `FERRO_AUTH_<NAME>_*` |
| **LDAP / Active Directory** | Directory accounts typed into the normal form | `FERRO_LDAP_*` |
| **Two-factor authentication** (TOTP) | Any account that signs in with a password or the directory | Per user, in *My account* |
| **Password reset by email** and **invitations** | Local accounts | `FERRO_SMTP_*`, `FERRO_PUBLIC_URL` |

#### Two-factor authentication (TOTP)

- RFC 6238: SHA-1, 6 digits, 30-second steps, one step of tolerance either way (clocks drift). Works with any authenticator
  app (Microsoft/Google Authenticator, 1Password, Authy…).
- **Enrolment** (`POST /auth/tfa/generate` → `POST /auth/tfa/enable`): the user confirms their current password (users
  without one — SSO, directory — need only their session), gets a secret, an `otpauth://` address and a QR code (rendered
  by the server as SVG and shown as an image, never injected as markup), and proves the app works by typing a code. Ten
  single-use **recovery codes** are returned once; only their hashes are stored.
- **Sign-in**: after the right password, `POST /auth/login` without `otp` answers `401 OTP_REQUIRED`; the client asks for
  the code and repeats the call with `otp` (a six-digit code or a recovery code). A wrong code is `INVALID_OTP`. The
  response never reveals that two-factor exists to someone who got the password wrong.
- **Replay protection**: the time step of the last accepted code is stored atomically; the same code (or an older one) is
  refused, so a code seen over a shoulder works for nobody. A recovery code is spent by removing its hash in one
  statement — three simultaneous uses spend it once.
- **Brute force**: wrong codes count towards the same lockout as wrong passwords (`FERRO_LOGIN_MAX_FAILURES`); enrolment
  and disabling are throttled too.
- **At rest**: the secret is encrypted with AES-256-GCM using a key derived from `FERRO_ENCRYPTION_KEY` (default:
  `FERRO_SECRET`). **Changing that key makes every stored secret unreadable**: set a dedicated `FERRO_ENCRYPTION_KEY` before
  anybody enrols if you expect to rotate `FERRO_SECRET`. The API never returns the secret or the codes (they are not part
  of the exposed `ferro_users` fields, so they cannot even be selected or filtered on); `tfa_enabled` and `provider` are.
- **Turning it off** needs the password (if any) and a code. An administrator can remove a user's second factor
  (`DELETE /users/:id/tfa`, Users page); with no other administrator available use `ferro user reset-tfa --email …`.
- **Scope**: applies to password logins and directory logins. Single sign-on skips it — the identity provider is the place
  for multi-factor policy. Enforcing it for everybody (or per role) is not implemented yet.

#### Password reset and invitations

Both rely on **single-use tokens**: 256 random bits, stored only as a SHA-256 hash, deleted as they are read (so concurrent
uses succeed once), bound to their purpose (a reset token is useless as an invitation and the other way round) and
expiring (reset 1 hour, invitation 7 days).

- `POST /auth/password/request {email, reset_url?}` always answers `202` with the same body whether or not the address has
  an account, and does the work in the background, so neither the answer nor its timing tells. Only active accounts with a
  Ferro password get a link (not suspended, not SSO/directory accounts). A newer request invalidates the previous link.
  Every request counts towards a throttle (5 per hour per address, 50 per client address): the endpoint can make Ferro mail
  anybody.
- `POST /auth/password/reset {token, password}` sets the password and **ends every session** of that user.
- `POST /users/invite {email, role?, invite_url?}` (administrators) creates the user as `invited` and mails a link;
  `POST /auth/invite/accept {token, password}` activates the account and signs the user in. Inviting a pending address again
  renews it; active or suspended accounts are a conflict. If sending fails (or SMTP is not configured but
  `FERRO_PUBLIC_URL` is) the link is returned to the administrator instead (`delivery: "link"`).
- **Links point at `FERRO_PUBLIC_URL`, never at the request's `Host` header** (otherwise an attacker could have a reset
  link mailed that points to their own site). A caller may pass `reset_url` / `invite_url` for another front end, but only
  for an origin that is the public URL or listed in `FERRO_AUTH_REDIRECT_ALLOWLIST`, compared as parsed origins
  (`https://cms.example.com.evil.net` and `https://cms.example.com@evil.net` are rejected).
- The default links open the admin app (`/admin/#/reset?token=…`, `#/invite?token=…`); the token sits in the URL
  *fragment* (never sent to a server or logged by a proxy), the page has `Referrer-Policy: no-referrer` semantics, and the
  app removes the token from the address bar and the history as soon as it has read it.
- Passwords must be 8–1024 characters everywhere (CLI, API, reset, invitation): the upper bound keeps hashing cost sane.

#### Single sign-on (OpenID Connect and OAuth 2.0)

Configured by environment variables per provider (see the table in §13): with an OpenID provider only the issuer address
is needed (endpoints come from discovery); a plain OAuth 2.0 provider lists its endpoints explicitly. The sign-in page
shows one button per provider.

1. `GET /auth/oauth/<name>` sends the browser to the provider with `state`, `nonce` and a **PKCE** (S256) challenge, and
   sets a short-lived, signed, `HttpOnly`, `SameSite=Lax` cookie holding the state, the nonce, the PKCE verifier and the
   page to return to. The verifier never travels in a URL.
2. The provider redirects to `/auth/oauth/<name>/callback`. Ferro checks the cookie and the `state` (CSRF), exchanges the
   code (client secret, PKCE verifier, exact `redirect_uri`) and — for OpenID — **verifies the ID token**: signature against
   the provider's published keys (cached; refetched when an unknown key id appears, e.g. after a rotation), issuer, audience
   (and `azp`), expiry, subject and nonce. Only asymmetric algorithms are accepted: `none` and HMAC tokens (the classic
   key-confusion attack) are refused. If the token has no email, the userinfo endpoint is consulted and must name the same
   subject. The discovery document must name the configured issuer.
3. Ferro finds or creates the user (rules below) and redirects the browser to the admin app with a **one-time code**
   (`#/sso?code=…`, valid 60 seconds, hashed in the database, single use). The app trades it for tokens with
   `POST /auth/oauth/exchange`: **tokens never appear in a URL**. A failure redirects with `?error=` and a short code
   (`denied`, `domain`, `email`, `state`, `token`, `provider`, `server`); details go to the server log only.
4. The page to return to (`?redirect=`) must be an allowed origin; anything else is a 400, so the flow cannot be used as an
   open redirector.

**Who gets in.** The provider's stable identifier (`sub`), not the email, identifies a person — changing an email at the
provider does not create a second account. For a first sign-in:

| Situation | Result |
|---|---|
| Already bound to this provider (same identifier) | Signs in (if the account is active) |
| An account was pre-created for this provider (`provider` set, no identifier yet) | Bound at the first sign-in |
| An *invited* user with that email | Activated and bound — signing in with the provider accepts the invitation |
| A Ferro password account with that email | **Refused**, unless the provider has `LINK_EXISTING_USERS=true` |
| An account bound to another provider | **Refused** |
| A suspended account | **Refused** |
| Nobody has that email | Refused, unless the provider has `ALLOW_PUBLIC_REGISTRATION=true` (then created with `DEFAULT_ROLE`) |

Further guards per provider: `REQUIRE_VERIFIED_EMAIL` (default on for OpenID: the ID token must say `email_verified`),
`ALLOWED_EMAIL_DOMAINS` (exact domains), and https for every endpoint (plain http only for `localhost`). Roles are not
derived from provider groups/claims: assign them in Ferro (or via `DEFAULT_ROLE`).

#### Directory login (LDAP / Active Directory)

Users type their directory login (e.g. `uid`) or email and directory password into the normal form. Ferro does the classic
two steps: a service account (`FERRO_LDAP_BIND_DN`; anonymous if unset) **searches** for the one entry matching
`FERRO_LDAP_USER_FILTER`, then Ferro **binds as that entry** with the typed password. The directory alone judges the
password; Ferro stores no hash.

- The typed login is **escaped per RFC 4515** before it enters the filter: `*`, `(`, `)`, `\` and NUL are plain
  characters, so `*)(uid=*` cannot widen the search. No match, or several, is just "invalid credentials".
- An **empty password is refused before talking to the directory** (a simple bind with an empty password is an
  "unauthenticated bind" that most servers answer with success).
- Accounts follow the same rules as single sign-on: bound by the entry's stable id (`entryUUID`; the DN if the server
  returns none), pre-created or created on first sign-in (`FERRO_LDAP_ALLOW_PUBLIC_REGISTRATION`), existing password accounts
  untouched unless `FERRO_LDAP_LINK_EXISTING_USERS`. A Ferro account with a password never falls through to the directory,
  and a directory account never to a password.
- Ferro's own second factor applies if the user enabled it. Wrong passwords count towards the login lockout; an unreachable
  directory or a refused service account is `503 DIRECTORY_UNAVAILABLE` and is not counted against the user.
- `ldaps://` verifies the server certificate (`FERRO_LDAP_TLS_VERIFY=false` only for test servers). STARTTLS
  (`FERRO_LDAP_STARTTLS`) is implemented by the client library but not covered by the tests.

#### Security events

Password resets, invitations, second-factor changes and single sign-on logins are written to the server log under the target
`ferro::audit` (with user ids and, for administrators' actions, who did it). They are not yet stored in `ferro_activity`.

### Hardening verified by the tests (`tests/e2e/05-security.mjs`)

- **No user value in SQL**: values are always parameters, identifiers are validated
  and quoted. Collection and field names, filters, sorting, `fields` and paths were
  tried with injection attempts.
- **Field permissions on indirect channels too**: a filter, sort, search (`search`)
  or count (`meta=filter_count`, `_count`) on a field the user cannot read is
  refused (403), and search ignores those fields; in relational filters the
  permissions of the linked collection apply. Otherwise the content of a protected
  field could be inferred by trial and error. The same holds for GraphQL and for
  realtime subscriptions (same read path).
- **The wildcard `collection: "*"` does not cover system collections** (`ferro_*`):
  users, activity and revisions require an explicit permission.
- **Tokens**: signature, algorithm (`none` is rejected), issuer, expiry (5 s
  leeway) and subject are verified; the refresh token rotates on every use and a
  token that was already used cannot be reused; rotating a static token invalidates
  the previous one.
- **Login throttling**: after `FERRO_LOGIN_MAX_FAILURES` (10) wrong passwords for
  the same email, the email is locked for `FERRO_LOGIN_LOCKOUT_SECS` (900 s) with a
  429, even with the right password; non-existent emails count the same way (no
  account enumeration); the per-address limit is ten times higher. The counter is
  per instance.
- **Mass assignment**: `user_created`, `date_created` and the other server-managed
  fields are ignored if sent.
- **Paths**: file ids are UUIDs only, storage keys are validated, and `ServeDir`
  rejects `..` (also when encoded).
- **Malformed input**: invalid JSON or JSON nested 5000 levels deep, NUL characters,
  numeric overflows and invalid dates give a 4xx, never a 500.
- **CORS**: no origin is allowed by default.

## 10. Files and assets

- Multipart upload; the binary is stored with the file id as its key (UUID v7),
  a SHA-256 checksum, and a MIME type taken from the header or the extension.
- `?replace=<id>` replaces the binary while keeping the id and the references.
- `GET /assets/:id` serves the binary only if the caller can read the row in
  `ferro_files` (public policies enable public assets); `ETag` = checksum.
- **Storage with several locations.** Each location has a name and a driver; each
  `ferro_files` row remembers its own (`storage`), and new uploads go to the default
  location. This lets you run a disk next to a bucket and move files with
  `ferro storage migrate`. The object key stays the file id (plus the location's
  optional prefix).
- Drivers: `local` (a directory; a shared volume when running several instances),
  `s3` (Amazon S3 and compatibles: MinIO, R2, B2, Wasabi, Spaces, GCS with HMAC keys;
  explicit credentials or the AWS chain: environment, profile, ECS/EKS/EC2 role),
  `azure` (Blob Storage with a SAS token). At startup every location is probed with a
  test object (write + delete): a missing bucket or wrong credentials stop the server
  with a clear message instead of failing on the first upload.
- **Consistency between the database and storage.** Binaries are deleted *after* the
  commit (a rollback never loses a file); an upload that fails after the blob was
  written removes it again (no orphans); replacing a file that moves to another
  location deletes the old blob only once the commit has happened. Known limit:
  `?replace=` on the *same* location overwrites the object before the commit (a later
  error in the same request leaves the new blob with the old row); storage is not
  transactional.
- **Download.** By default the server reads the object and forwards it. With
  `FERRO_STORAGE_<NAME>_PRESIGN=<seconds>` it answers `307` towards a signed URL
  (`Content-Disposition` and type already part of the signature, `no-store`):
  permissions are checked *before* signing and the binary traffic does not go through
  the server. The Admin UI uses `<img src>`, so it works without CORS; a `fetch` from
  another origin requires CORS on the bucket.
- In sync, binaries travel as before (`PUT /sync/files/:id`) to the target's default
  location, regardless of where they live in the source.
- Sync transfers only the binaries the target does not have or that have a different
  checksum.

### Image transformations

`GET /assets/:id` can resize, crop and convert raster images on the fly. The original is never modified:
variants are computed from it, so a replaced binary is picked up immediately. The same read permission as for the
original applies (checked **before** anything is decoded), and the response has its own `ETag`.

| Parameter | Values | Meaning |
|---|---|---|
| `width`, `height` | 1 … `FERRO_IMAGE_MAX_DIMENSION` (4096) | Target size in pixels. With only one of them the ratio is kept. |
| `fit` | `cover` (default when both sizes are given), `contain`, `inside`, `outside`, `fill` | `cover` fills the box and crops the overflow; `contain` fits inside the box and pads to exactly the box size; `inside` fits inside without padding (the result can be smaller than the box); `outside` covers the box without cropping (the result can be larger); `fill` stretches, ignoring the ratio. |
| `position` | `center` (default), `top`, `bottom`, `left`, `right`, `top_left`, … | Which part `cover` keeps. Needs both `width` and `height`. |
| `crop` | `x,y,width,height` | A rectangle of the original, in its pixels; applied **before** resizing. It must lie inside the image. |
| `format` | `jpg`, `png`, `webp` | Output format. Default: the source's own for JPEG, PNG and WebP; GIF, BMP and TIFF come out as PNG. |
| `quality` | 1 – 100 (default 80) | JPEG quality. WebP output is lossless, so it is ignored there. |
| `withoutEnlargement` | `true` / `false` | Never scale up: if the image is already smaller than the box, the output is smaller than requested. `without_enlargement` is accepted too. |
| `background` | hex colour, e.g. `ffffff` | Padding colour for `contain`, and the colour transparency is flattened onto when converting to JPEG (white by default). Without it, padding is transparent for PNG and WebP. |

```
/assets/<id>?width=300&height=200&fit=cover&position=top&format=webp
/assets/<id>?crop=120,40,600,400&width=300
/assets/<id>?width=800&format=jpg&quality=70&download=1
```

- Invalid or inconsistent parameters answer **400** with a message (unknown value, out of range, `fit` without a size,
  `position` without both sizes, a crop outside the image). Unknown query parameters are ignored and never reach the cache.
- Only JPEG, PNG, GIF, WebP, BMP and TIFF are transformed, recognised **by content**, not by the stored MIME type. Anything
  else (SVG, PDF, text, a corrupt file) answers 400 for a transformation and is still served as is without parameters.
  Animated GIFs are reduced to their first frame.
- The EXIF orientation of the source is applied; all metadata (EXIF, GPS, ICC profiles) is **dropped** from the output.
- Uploads record `width` and `height` of every supported image in `ferro_files` (read from the header, no decoding).
- **Protection against abuse.** The size is read from the header first: an image above `FERRO_IMAGE_MAX_INPUT_MEGAPIXELS`
  (64) is refused before decoding (a small file can declare a gigantic bitmap), and decoding has a memory ceiling derived
  from the same limit. Every computed size is checked against `FERRO_IMAGE_MAX_DIMENSION` **before** anything is allocated, so an
  extreme aspect ratio (`width=4096` on a 1×4000 image) is a 400, not a multi-gigabyte bitmap; `cover` cuts the needed window
  out of the original first and scales only that. At most `FERRO_IMAGE_CONCURRENCY` (default: the number of CPUs) transformations run at the same
  time, on blocking threads, so image work never stalls the request loop; the others wait their turn.
- **Caching.** Results are kept in a per-instance LRU cache of `FERRO_IMAGE_CACHE_MB` (64; `0` turns it off), keyed by file id,
  checksum and the *normalised* parameters (`format=JPEG&width=77` and `width=77&format=jpg` are the same variant). The
  `ETag` is the original's checksum plus a hash of those parameters, and `If-None-Match` is answered with **304** without
  decoding anything; browsers and CDNs can therefore cache variants safely (`Cache-Control: private`).
- Transformations are always produced by Ferro, also for files in an S3/Azure location with signed downloads enabled: only
  the untransformed original is redirected to the bucket.
- Limits: no AVIF output, WebP is lossless only, no rotation or filters, no per-file focal point, and the whole source image
  is decoded in memory (hence the megapixel ceiling). For heavy public traffic put a CDN in front: the variant URL is stable
  and cache-friendly.

## 11. The `ferro` CLI

```
ferro serve                                   start the server (FERRO_* variables)
ferro migrate                                 create/upgrade the system tables
ferro init                                    create ferro.toml and schema/
ferro env list | status                       environments, reachability, schema hash
ferro schema pull     --from dev              environment → schema/ (to commit)
ferro schema validate                         validate schema/
ferro schema import-directus --file snap.json  Directus snapshot → schema/
ferro schema diff     [--from schema] --to prod [--exit-code] [--report f.html] [--json]
ferro schema apply    [--from schema] --to prod [--dry-run] [--allow-destructive] [--yes]
ferro data export     --from dev --select … --out dir
ferro data import     --to prod --bundle dir [--strategy …] [--prune] [--dry-run] [--force] [--yes]
ferro data diff       --from dev --to prod --select … [--exit-code] [--report f.html]
ferro data push       --from dev --to prod --select … [import options]
ferro storage list                            configured locations, files and bytes per location
ferro storage check [--exit-code]             write probe + every file exists where its row says (exit 2 if one is missing)
ferro storage migrate --from local --to s3 [--delete-source] [--dry-run]
                                              copy (checksum verified), update the row, then delete the source
ferro user create     --email … --password … [--admin | --role <id>]
ferro user token      --email …
ferro user reset-tfa  --email …               remove a user's second factor (direct DB; lost phone, no other admin)
```

`--from`/`--to` accept the name of an environment in `ferro.toml` or a path (a
schema directory or a `.yaml`/`.json` file). Exit codes: `0` ok, `1` error, `2`
differences found (with `--exit-code`, for pipelines).

`ferro.toml`:

```toml
[project]
schema = "schema"

[environments.dev]
url = "https://cms-dev.example.com"
token_env = "FERRO_DEV_TOKEN"

[environments.prod]
url = "https://cms.example.com"
token_env = "FERRO_PROD_TOKEN"
protected = true      # writes only with --yes or by typing the environment name
```

## 12. CI/CD and workflow

### Model

```
 developer                git (PR)                 test                prod
 ─────────                ────────                 ────                ────
 change in dev UI    ──►  ferro schema pull  ──►  diff review
 or YAML by hand          (commit schema/)        ferro schema apply --to test
                                                  ferro schema diff --to prod --report
                                                                        ferro schema apply --to prod --yes
```

- Nightly **drift detection**: `ferro schema diff --to prod --exit-code` fails if
  prod was changed outside git.
- **Content promotion** as a parameterised job (replacing the per-collection
  Jenkinsfiles of `migrate-data`/`diff-data`): `ferro data diff` for the preview
  (report attached to the build), `ferro data push` after approval.

Jenkins example: see [`deploy/Jenkinsfile.example`](../deploy/Jenkinsfile.example).

## 13. Deployment, operations, security

### Configuration (environment variables)

| Variable | Default | |
|---|---|---|
| `FERRO_DATABASE_URL` | — | required |
| `FERRO_SECRET` | — | required, ≥ 32 characters |
| `FERRO_ENV` | `local` | name of the environment (bundles, reports) |
| `FERRO_BIND` | `0.0.0.0:8055` | |
| `FERRO_STORAGE_LOCATIONS` | `local` | names of the locations, comma-separated |
| `FERRO_STORAGE_DEFAULT` | the first one | location for new uploads |
| `FERRO_STORAGE_LOCAL_ROOT` | `./uploads` | directory of the `local` location |
| `FERRO_STORAGE_<NAME>_DRIVER` | — | `local`, `s3` or `azure` (the default for a location called `local`) |
| `FERRO_STORAGE_<NAME>_BUCKET`, `_REGION`, `_PREFIX`, `_ENDPOINT`, `_FORCE_PATH_STYLE` | — / `AWS_REGION` / — / — / true with an endpoint | `s3` driver |
| `FERRO_STORAGE_<NAME>_ACCESS_KEY_ID`, `_SECRET_ACCESS_KEY`, `_SESSION_TOKEN` | AWS chain | credentials (optional) |
| `FERRO_STORAGE_<NAME>_PRESIGN` | off | seconds a signed URL stays valid (307 redirect) |
| `FERRO_STORAGE_<NAME>_ACCOUNT_URL`, `_CONTAINER`, `_SAS_TOKEN`, `_PREFIX` | — | `azure` driver |
| `FERRO_ACCESS_TOKEN_TTL` / `FERRO_REFRESH_TOKEN_TTL` | 900 / 604800 s | |
| `FERRO_MAX_BODY_MB` | 256 | uploads and bundles |
| `FERRO_QUERY_LIMIT_MAX` | -1 | maximum value accepted for `limit` (-1 = none) |
| `FERRO_IMAGE_MAX_DIMENSION` | 4096 | largest width/height of a transformed image |
| `FERRO_IMAGE_MAX_INPUT_MEGAPIXELS` | 64 | largest source image that will be decoded |
| `FERRO_IMAGE_CONCURRENCY` | number of CPUs | transformations running at once |
| `FERRO_IMAGE_CACHE_MB` | 64 | per-instance cache of transformed images (0 = off) |
| `FERRO_ADMIN_EMAIL` / `FERRO_ADMIN_PASSWORD` | — | first administrator, if no users exist |
| `FERRO_CORS_ORIGINS` | — | comma-separated list, or `*` |
| `FERRO_ADMIN_DIR` | — | static Admin UI build, served on `/admin` |
| `FERRO_EXTENSIONS_DIR` | — | Admin UI extensions, served on `/extensions` |
| `FERRO_FLOW_ENV_<NAME>` | — | secrets available to flows as `{{ $env.NAME }}` |
| `FERRO_FLOW_ALLOWED_HOSTS` | — | if set, flows may only call these hosts |
| `FERRO_FLOW_ALLOW_PRIVATE` | `false` | lets flows call private addresses |
| `FERRO_DB_MAX_CONNECTIONS` | 10 | |
| `FERRO_LOGIN_MAX_FAILURES` / `FERRO_LOGIN_LOCKOUT_SECS` | 10 / 900 | login throttling per email |
| `FERRO_PUBLIC_URL` | — | where users reach Ferro: base of every emailed link and of the single sign-on callback (never taken from the request) |
| `FERRO_AUTH_REDIRECT_ALLOWLIST` | — | extra origins reset / invitation / sign-in flows may send users back to |
| `FERRO_ENCRYPTION_KEY` | `FERRO_SECRET` | key for secrets stored in the database (two-factor seeds); changing it invalidates them |
| `FERRO_TFA_ISSUER` | `Ferro` (`Ferro (<env>)` outside prod) | name shown in authenticator apps and emails |
| `FERRO_SMTP_HOST`, `_PORT`, `_SECURE` (`starttls`/`tls`/`none`), `_USER`, `_PASSWORD`, `FERRO_EMAIL_FROM` | — | outgoing email (password reset, invitations) |
| `FERRO_AUTH_PROVIDERS` | — | single sign-on provider names, comma-separated |
| `FERRO_AUTH_<NAME>_DRIVER`, `_CLIENT_ID`, `_CLIENT_SECRET`, `_ISSUER_URL` / `_AUTHORIZE_URL`, `_TOKEN_URL`, `_PROFILE_URL`, `_JWKS_URL`, `_SCOPE` | `openid` | the provider (`openid` needs only the issuer; `oauth2` needs the three endpoints) |
| `FERRO_AUTH_<NAME>_EMAIL_KEY`, `_IDENTIFIER_KEY`, `_FIRST_NAME_KEY`, `_LAST_NAME_KEY`, `_LABEL`, `_ICON` | `email`, `sub`/`id`, `given_name`, `family_name` | claims / profile fields to read, and the button text |
| `FERRO_AUTH_<NAME>_REQUIRE_VERIFIED_EMAIL`, `_ALLOWED_EMAIL_DOMAINS`, `_ALLOW_PUBLIC_REGISTRATION`, `_DEFAULT_ROLE`, `_LINK_EXISTING_USERS` | on (OpenID) / off, —, `false`, —, `false` | who may sign in and what happens at the first sign-in |
| `FERRO_LDAP_URL`, `_STARTTLS`, `_TLS_VERIFY`, `_BIND_DN`, `_BIND_PASSWORD`, `_USER_BASE_DN`, `_USER_FILTER`, `_TIMEOUT_SECS` | filter `(&(objectClass=person)(\|(uid={login})(mail={login})))` | directory login (see §9) |
| `FERRO_LDAP_EMAIL_ATTRIBUTE`, `_FIRST_NAME_ATTRIBUTE`, `_LAST_NAME_ATTRIBUTE`, `_IDENTIFIER_ATTRIBUTE` | `mail`, `givenName`, `sn`, `entryUUID` | attributes to read |
| `FERRO_LDAP_ALLOW_PUBLIC_REGISTRATION`, `_DEFAULT_ROLE`, `_LINK_EXISTING_USERS` | `false`, —, `false` | first sign-in rules, as for single sign-on |
| `FERRO_LOG` | `info` | `tracing` filter |

### Scalability

The server is stateless: run several instances behind a load balancer; the schema is
realigned through `LISTEN/NOTIFY`, and migrations and applies are serialised by
advisory locks. With several instances storage must be shared: an S3/Azure bucket, or
a shared volume for `local`.

### Security

- No user value is concatenated into SQL: identifiers are validated and quoted,
  values are always parameters (`CAST($n AS <type from the schema>)`).
- Sensitive fields are not exposed by the engine; tokens and refresh tokens are
  stored only as hashes.
- Storage keys are validated (no path traversal).
- Schema, sync and user operations require `admin_access`; `protected` environments
  need a double confirmation in the CLI.

### Backup

The database is the only source of truth for content and users (standard
`pg_dump`); the snapshot is also in git; storage has to be backed up separately.

## 14. Migrating from Directus

Suggested plan (the **model** converter exists; the **data** converter is on the roadmap):

1. **Model**: `npx directus schema snapshot -y snapshot.json`, then
   `ferro schema import-directus --file snapshot.json` writes `schema/` (Directus
   `collections`/`fields`/`relations` → `fields` with a `relation`; M2M/M2A junctions
   stay declared collections; Directus's four system fields become
   `system_fields: true`; folders and purely visual fields are dropped; every
   dropped or approximated item is printed as a warning, and the result is
   validated). Roles and permissions are not converted (step 2).
2. **Policies**: export `directus_policies`/`directus_permissions` → Ferro policies
   with new stable UUIDs (done once; from then on they live in git).
3. **Ids**: for collections with an integer primary key, the Ferro id is the
   deterministic **UUID v5** of `(collection, directus_id)`. Where several Directus
   environments have different ids for the same content, declare `sync.natural_key`
   and let the import remap.
4. **Data**: read through the Directus API/database → Ferro bundle → `ferro data import`.
5. **Files**: copy the binaries with the new id as key, and build `ferro_folders`
   from `directus_folders`.
6. **Frontend**: same concepts (`/items`, `fields`, `filter`, `{ data }`); what changes
   are the ids (uuid) and the names of the system collections.

## 15. Admin UI

The [`admin/`](../admin/) folder: a SPA in **plain JavaScript** (ES modules, no
dependencies, no build). The server serves it on `/admin` when `FERRO_ADMIN_DIR` is
set; it uses only the public API. To develop against another server:
`/admin/?api=https://host` (the other server needs CORS).

| Section | What it does |
|---|---|
| GraphQL | Playground: editor, variables, result, SDL schema download |
| Automations | Flows: starter templates, trigger and operation editor, plan preview, run history, manual runs |
| Content | List with search, filters, sorting, pagination, multi-delete; item page with a form **generated from the model** |
| Form | A widget for every type; M2O pickers with labels; files (upload, pick, preview); inline **O2M** and **M2M** (reordering with a sort field); **M2A blocks** (add/reorder/remove blocks, nested forms up to 3 levels); required-field validation; fields the user cannot read because of permissions are never written back as `null` |
| History | Revisions of an item, with restore of simple fields |
| Data model | Collections and fields (types, relations, defaults, interface, validation). **Every save first runs a dry run and shows the plan and the SQL**; destructive changes are highlighted and need confirmation |
| Access control | Roles and policies with permissions (fields, item filter, validation, presets), with the same plan preview |
| Files | Library with folders, drag & drop, metadata, binary replacement, download; lists and previews load small cropped thumbnails (`?width=…&height=…&fit=cover`) instead of the originals |
| Users | Users, roles, status, password, API token generation; **invite by email** (or get the link when Ferro cannot send mail); sign-in method and 2FA columns; remove someone's second factor |
| My account | Who am I, how I sign in; set up two-factor authentication (QR code, manual key, recovery codes) and turn it off |
| Sign-in | Email + password, a code step for two-factor accounts, "Forgot your password?", one button per single sign-on provider; the reset-password, invitation and sign-in-return pages (`#/reset`, `#/invite`, `#/sso`) work before anybody is signed in |
| Environment | Name and schema hash, change history, YAML snapshot download, **alignment to another environment** (URL + token) or to a JSON snapshot, always with a preview |
| Activity | Audit by collection/action |

Security: all text is inserted as text nodes (never `innerHTML`); the refresh token
is in `localStorage` and the access token only in memory; assets in previews use
`?access_token=`.

Tests: `cd admin && node --test test/*.test.mjs` runs the real modules on a fake DOM
with a simulated server (building and re-reading the nested payload, the "save →
preview → apply" flow, rendering of all the views).

### Extensions

A folder named by `FERRO_EXTENSIONS_DIR`, with one subfolder per extension:
`<name>/manifest.json` (`name`, `version`, `description`, `entry`) and an ES module
that exports `default function register(ferro)`. The Admin UI loads them at startup
(`GET /server/extensions`, files served on `/extensions`); an extension that fails is
reported in the console and does not block the app.

```js
export default function register(ferro) {
  ferro.addPage({ id: 'overview', title: 'Overview', render: (root) => { /* ... */ } });     // menu entry
  ferro.addInterface('stars', (field, value, ctx) => ({ el, get: () => value }));            // field widget
  ferro.addItemAction({ label: 'Copy slug', collections: ['pages'], run: async ({ item }) => {} });
}
```

`ferro.api` (authenticated HTTP client), `ferro.dom` (`h`, `button`, `modal`,
`toast`…) and `ferro.schema` are available. An interface is activated by setting the
field's `meta.interface` to its name, and takes precedence over the default widgets.
Example: `examples/site/extensions/hello-panel`.

Extensions run **with the privileges of the user who uses the admin**: install only
code you trust. Server-side logic is expressed with flows (including calling your own
services through `request`); arbitrary Rust code has to be compiled into the binary.

## 16. Status, limits and roadmap

### Implemented

- Complete model (types, M2O/O2M/M2M/M2A/file relations, singletons, system fields,
  validations), snapshot validation
- Transactional diff/plan/apply with renames, risk levels, optimistic lock, history,
  multi-instance
- REST items API with filters, search, sorting, pagination, counts, relational
  expansion, nested writes, singletons
- Auth (password, JWT, refresh, static tokens), RBAC with policies
  (fields/filters/validation/presets, public ones)
- Two-factor authentication (TOTP + recovery codes), password reset and invitations by
  email, single sign-on (OpenID Connect / OAuth 2.0), LDAP login (§9)
- Files and folders with local, S3 (and compatibles) or Azure Blob storage, assets
  with permissions, on-the-fly image transformations (resize, crop, formats)
- Activity and revisions
- Sync: export with dependency closure, import with compatibility checks, natural
  keys, strategies, prune, FK verification, dry run, HTML/JSON reports
- CLI for all the environment operations
- Admin UI (see §15)
- GraphQL (queries and mutations, unions for M2A blocks, generated SDL; see §8.3)
- Realtime over WebSocket with per-client permissions and propagation between
  instances (§8.5)
- Flows (webhooks, blocking and non-blocking hooks, scheduled jobs, custom endpoints,
  manual actions; §8.4) and Admin UI extensions (§15)

### Known limits

- **State of verification** (2026-10-09): the workspace builds without warnings;
  108 Rust tests, 46 Admin UI tests, **655 integration checks** over several environments
  with a real PostgreSQL 16 (schema, sync, flows, GraphQL, security, realtime —
  including two instances on the same database — storage against an
  S3-compatible server, image transformations checked pixel by pixel, and the identity flows — two-factor, password reset, invitations, single
  sign-on and LDAP — against a fake SMTP server, a mock OpenID provider and a real LDAP server) and **68 checks in a real browser** (Edge/Chromium; including the reset, invitation, two-factor and single sign-on
  screens end to end). They run
  with `scripts/ci.sh [browser]` (GitHub workflow in `.github/workflows/ci.yml`);
  details in `tests/README.md`.
- Never run: the Docker image, the compose file, the example Jenkinsfile and the first
  CI run on a Linux runner. The S3 driver has been exercised only against an
  S3-compatible server (SeaweedFS), never against AWS itself; the Azure driver only
  against a fake blob service in unit tests.
- No load tests; no independent security audit.
- **Identity: what was and was not exercised.** Single sign-on has been tested against a mock OpenID provider written for the
  test suite (it signs real RS256 tokens and enforces PKCE and the client secret), not against Microsoft Entra ID, Google or
  Okta themselves; provider quirks (for example Entra not sending `email_verified`, which needs
  `REQUIRE_VERIFIED_EMAIL=false`, or GitHub hiding private emails) are yours to configure. ID tokens signed with RS*, PS* or
  ES256/384 are accepted; EdDSA is not. LDAP was tested against ldapjs over plain `ldap://` and `ldaps://`; STARTTLS, Active
  Directory specifics (`objectGUID`, referrals, paged results) and SMTP over TLS/STARTTLS were not exercised (the SMTP
  transports are only constructed in tests; delivery was checked over plain SMTP).
- **Identity: not implemented.** SAML; passkeys / WebAuthn; requiring two-factor for everybody or per role; mapping directory
  groups or provider claims to roles; user provisioning/deprovisioning from a directory (an account removed there keeps its
  Ferro account until someone suspends it, though it can no longer sign in through the directory); listing and revoking
  sessions; confirming a changed email address.
- **Identity: operations.** Login and reset throttling is per instance (memory), like the rest of the rate limiting. The client
  address comes from `X-Forwarded-For`: run Ferro behind a proxy that overwrites that header, or the per-address limits can
  be dodged (the per-account limits still hold). Anybody can use up the reset-request allowance of an address (5 an hour) to
  stop its owner from asking; a locked reset only delays them. Rotating `FERRO_SECRET` without a separate
  `FERRO_ENCRYPTION_KEY` locks every two-factor user out until an administrator resets them.
- Nested relational filters support m2o and o2m-like relations (not `m2a_item`).
- Query-string filters only in JSON form (`filter={…}`), not in the
  `filter[field][_eq]=` form.
- Binaries are held in memory during upload/sync (fine up to a few hundred MB per file).
- Prune does not delete M2A items left orphaned after their junction row is removed
  (as in Directus).
- Deletions save the last content in `ferro_revisions`, but there is no restore
  endpoint yet.

### Roadmap

1. Run the CI workflow on a real runner (and add a testcontainers-based setup for
   local integration runs)
2. Admin UI: wizards for M2M/M2A relations, saved columns and filters, a WYSIWYG
   editor
3. Streaming uploads/downloads (today files are held in memory, up to
   `FERRO_MAX_BODY_MB`), S3 multipart upload, Azure with SharedKey/Entra ID; image
   transformations: AVIF and lossy WebP output, a per-file focal point, named presets,
   a shared (not per-instance) variant cache
4. A **data** converter from Directus (`directus_uuid(collection, id)` in `ferro-core`
   already provides the deterministic ids)
5. GraphQL introspection and subscriptions, filters in the `filter[...]` form
6. System events for flows (login, file upload, import), `email` and `sleep`
   operations; realtime for imports and files as well
7. Content drafts/versions and scheduled publishing
8. OpenAPI generated from the snapshot (types for frontends)
9. Identity: WebAuthn / passkeys, enforced two-factor by role, group-to-role mapping for directory and single sign-on,
   SAML, security events in `ferro_activity` and a session manager (see "Known limits")
