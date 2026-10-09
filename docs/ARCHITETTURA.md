# Ferro CMS — Documento di progetto

> Headless CMS in Rust, ispirato a Directus, progettato attorno a un requisito
> che Directus non soddisfa bene: **promuovere modello dati, permessi e contenuti
> tra ambienti (dev → test → prod) in modo sicuro, ripetibile e verificabile.**

Indice

1. [Perché: i problemi da risolvere](#1-perché-i-problemi-da-risolvere)
2. [Principi di progetto](#2-principi-di-progetto)
3. [Architettura](#3-architettura)
4. [Modello dati](#4-modello-dati)
5. [Persistenza su PostgreSQL](#5-persistenza-su-postgresql)
6. [Schema as code: diff, plan, apply](#6-schema-as-code-diff-plan-apply)
7. [Migrazione dei contenuti tra ambienti](#7-migrazione-dei-contenuti-tra-ambienti)
8. [API REST](#8-api-rest)
9. [Autenticazione e permessi](#9-autenticazione-e-permessi)
10. [File e asset](#10-file-e-asset)
11. [CLI `ferro`](#11-cli-ferro)
12. [CI/CD e flusso di lavoro](#12-cicd-e-flusso-di-lavoro)
13. [Deploy, esercizio, sicurezza](#13-deploy-esercizio-sicurezza)
14. [Migrazione da Directus](#14-migrazione-da-directus)
15. [Admin UI (progetto)](#15-admin-ui-progetto)
16. [Stato, limiti e roadmap](#16-stato-limiti-e-roadmap)

---

## 1. Perché: i problemi da risolvere

L'esperienza con Directus su più ambienti ha prodotto una serie di tool
"tampone". Ognuno corrisponde a un problema strutturale che Ferro risolve
nel design, non con script esterni:

| Tool attuale | Problema in Directus | Soluzione in Ferro |
|---|---|---|
| `diff`, `diff-model`, `diff-schema`, `live-diff` | Il modello vive solo nel DB; confrontare ambienti richiede di scaricare `/fields` e filtrare il rumore | Il modello è un **file YAML versionato** (uno per collection). `ferro schema diff --from dev --to prod` restituisce un piano semantico, ordinato e con livello di rischio; `--report` produce l'HTML |
| `diff-policies`, `policies-porting`, `policy-pilot` | Ruoli/policy/permessi hanno id diversi per ambiente e vanno ricreati a mano | Ruoli e policy fanno parte dello **snapshot**, con **UUID stabili**: si promuovono come il modello |
| `data-porting`, `migrate-data`, `import-data`, `migrate-blocks-*` | ID auto-increment diversi tra ambienti, dipendenze da risolvere a mano (`idToImport` per collection), cartelle che devono preesistere | **Tutti gli id sono UUID v7** generati una sola volta: lo stesso contenuto ha lo stesso id ovunque. `ferro data push` calcola da solo la **chiusura delle dipendenze** (M2O, O2M, M2M, M2A, file, cartelle) |
| `diff-data` (Jenkinsfile per collection) | Nessun modo nativo di vedere cosa cambierebbe | `ferro data diff` = import in **dry-run**: report campo-per-campo, da usare anche nelle pipeline |
| `fix-sorting-m2a` | Junction M2A con sort incoerenti dopo porting parziali | Import con `--prune`: i figli rimossi nella sorgente vengono rimossi/staccati nel target; i parent esportati "completi" sono marcati nel bundle |
| `create-system-fields` | Campi di sistema da aggiungere a mano, anche alle `_translations` | `date_created`, `user_created`, `date_updated`, `user_updated` sono **impliciti** (`system_fields: true` di default) |
| `wrong-cdn-links-detector`, file | I file sono legati al singolo storage/ambiente | Il binario ha chiave = id del file; il sync trasferisce solo i binari mancanti (checksum SHA-256) |
| `directus-from-dump` | Per avere un ambiente allineato si ripristina un dump | `ferro schema apply` + `ferro data push` ricostruiscono un ambiente da git + sorgente |
| `user-activity-audit` | — | `ferro_activity` + `ferro_revisions` per ogni scrittura, con `origin` (`api`, `sync`) |

## 2. Principi di progetto

1. **Identità stabile.** Ogni item ha `id uuid` (v7, ordinabile nel tempo). Mai
   sequenze. Gli id non cambiano tra ambienti, quindi le relazioni restano valide
   dopo una copia. Dove gli id divergono già (contenuti creati a mano in due
   ambienti) si dichiara una **natural key** (`sync.natural_key: [slug]`) e
   l'import rimappa gli id.
2. **Il modello è codice.** Lo snapshot (collection + campi + relazioni + ruoli +
   policy) si salva in git, si rivede in PR, si applica con un piano. Le
   modifiche fatte da Admin UI passano dalla **stessa pipeline** (snapshot
   modificato → diff → apply), quindi UI e git non possono divergere: basta un
   `ferro schema pull` per riportare in git una modifica fatta da UI.
3. **Tutto o niente.** PostgreSQL ha DDL transazionale: un piano di schema viene
   applicato interamente o per nulla. Un import di dati gira in una transazione
   con vincoli `DEFERRED`: l'ordine di inserimento è irrilevante, i cicli sono
   ammessi, e alla fine **ogni foreign key è verificata** prima del commit.
4. **Prima vedi, poi fai.** Ogni operazione di scrittura ha un dry-run che esegue
   *davvero* tutto (DDL e insert compresi) e poi fa rollback: il report è esatto,
   non una stima. Sugli ambienti `protected` la CLI mostra prima il dry-run e
   chiede conferma.
5. **Spiegabile.** I piani e i report sono dati (JSON) e documenti (HTML) che si
   allegano a una build o a una change request.
6. **Compatibilità concettuale con Directus.** Stessi concetti (collection,
   field, M2O/O2M/M2M/M2A, policy, filtri `_eq`/`_in`/…, `fields=a.b.*`,
   `{ data: … }`), per ridurre la curva di apprendimento e semplificare la
   migrazione dei frontend.

## 3. Architettura

```
                ┌──────────────────────────────┐
  Admin UI ───► │                              │
  Frontend ───► │   ferro-server (axum)        │  REST /items /collections /schema /sync /files ...
  ferro CLI ──► │   auth · routing · errori    │
                └──────────────┬───────────────┘
                               │
                ┌──────────────▼───────────────┐
                │   ferro-engine               │
                │  items (CRUD, espansione,    │   permissions · auth · activity
                │   scritture annidate)        │   files (Storage trait)
                │  query (filtri → SQL)        │   sync (export / import)
                │  ddl (plan → SQL)            │   schema_store (apply transazionale)
                └──────┬────────────────┬──────┘
                       │                │
                ┌──────▼──────┐   ┌─────▼──────┐
                │ PostgreSQL  │   │  Storage   │  local · S3 · Azure Blob
                └─────────────┘   └────────────┘

                ┌──────────────────────────────┐
                │   ferro-core (no I/O DB)     │  model · validate · diff · filter
                │   usato da engine e CLI      │  fields · graph · bundle · project · report
                └──────────────────────────────┘
```

| Crate | Responsabilità |
|---|---|
| `ferro-core` | Tipi del modello (snapshot), validazione, **diff/plan**, valutatore filtri in memoria, parsing `fields`, ordinamento topologico, formato bundle/report di import, lettura/scrittura YAML del progetto, report HTML. Nessuna dipendenza dal DB: testabile in isolamento. |
| `ferro-engine` | PostgreSQL e storage: migrazioni di sistema, DDL dai piani, compilazione filtri in SQL parametrico, servizio items, permessi, autenticazione, file, sync. |
| `ferro-storage` | Driver dello storage: `local`, **S3 e compatibili** (AWS SDK, catena di credenziali standard, URL firmati), **Azure Blob** (REST + SAS); configurazione da `FERRO_STORAGE_*`. Tiene le dipendenze pesanti fuori dal motore. |
| `ferro-server` | API HTTP (axum 0.7), estrazione dell'identità, mapping errori, CORS, limiti, ascolto `LISTEN/NOTIFY` per il ricaricamento dello schema. |
| `ferro-cli` | Binario `ferro`: `serve` + comandi di gestione ambienti (schema/data) via HTTP e `storage list/check/migrate` (diretti su DB e storage). |

**Un solo binario** (`ferro`) per server e strumenti: lo stesso artefatto gira in
produzione e in pipeline, con la stessa versione del formato di snapshot/bundle.

### Ciclo di una richiesta

1. L'estrattore `Acc` risolve il chiamante (JWT o token statico, altrimenti
   policy `public`) e costruisce l'`Accountability` (utente, ruolo, policy).
2. L'handler prende un `Arc<Schema>` immutabile (copia atomica del modello in
   memoria) e chiama il servizio items.
3. Il servizio calcola i permessi effettivi (`permissions::access`), compila
   filtro utente + filtro di permesso in SQL con parametri, esegue la query che
   restituisce direttamente JSON (`jsonb_agg(to_jsonb(...))`), poi espande le
   relazioni richieste con query batch (una per relazione, mai N+1).
4. Le scritture avvengono in transazione; activity e revisioni sono scritte
   nella stessa transazione.

## 4. Modello dati

### 4.1 Snapshot

Lo snapshot è la configurazione completa e indipendente dall'ambiente:

```yaml
version: 1
collections: [...]   # modello
roles: [...]         # ruoli (id stabili)
policies: [...]      # policy e permessi (id stabili)
```

Su disco è una directory, un file per oggetto, per avere diff leggibili in PR:

```
schema/
  collections/pages.yaml
  collections/pages_blocks.yaml
  roles/editor.yaml
  policies/editor.yaml
```

L'**hash** dello snapshot (SHA-256 del JSON canonico, ordine delle chiavi
normalizzato) identifica lo stato di un ambiente: due ambienti con lo stesso hash
hanno esattamente lo stesso modello e gli stessi permessi (`ferro env status`).

### 4.2 Collection

```yaml
name: pages
meta:
  icon: article
  display_template: "{{title}}"
  sort_field: title          # ordinamento di default
  singleton: false           # true = un solo item (es. impostazioni sito)
  revisions: true            # storico completo degli item
  translations: { it-IT: Pagine }
sync:
  natural_key: [slug]        # match tra ambienti quando gli id differiscono
  exclude: false             # true = mai esportata (dati specifici dell'ambiente)
system_fields: true          # date_created, user_created, date_updated, user_updated
fields: [...]
```

`id` (uuid, PK) è implicito e non si dichiara.

### 4.3 Tipi di campo

| `type` | Colonna PostgreSQL | Note |
|---|---|---|
| `string` | `varchar(max_length)` (255) | |
| `text` | `text` | |
| `integer` / `bigint` | `integer` / `bigint` | |
| `float` | `double precision` | |
| `decimal` | `numeric(precision, scale)` (10,5) | |
| `boolean` | `boolean` | |
| `uuid` | `uuid` | |
| `json` | `jsonb` | operatori `_contains` su array/oggetti |
| `date` / `datetime` / `time` | `date` / `timestamptz` / `time` | |
| `m2o` | `uuid` + FK `DEFERRABLE` + indice | `relation.related`, `relation.on_delete` |
| `file` | `uuid` + FK verso `ferro_files` | |
| `m2a_item` | `uuid` + indice | colonna della junction M2A: `relation.collection_field`, `relation.allowed` |
| `o2m` / `m2m` / `m2a` | nessuna (alias) | `relation.related` + `relation.foreign_field` |

Attributi comuni: `required` (NOT NULL), `unique`, `indexed`, `default` (letterale
o `$NOW`), `validation` (condizione con gli operatori dei filtri, es.
`{ _nempty: true }`), `meta` (interfaccia, opzioni, display, note, larghezza,
gruppo, traduzioni: usati solo dall'Admin UI).

### 4.4 Relazioni

Tutte le relazioni "a molti" sono **o2m verso una collection figlia o una
junction esplicita**. Le junction non sono generate di nascosto, sono
collection dichiarate: si vedono nello snapshot, si diffano, si migrano.

```yaml
# M2O
- { name: category, type: m2o, relation: { related: categories, on_delete: set_null } }

# O2M (inverso di un m2o del figlio)
- { name: comments, type: o2m, relation: { related: comments, foreign_field: page, sort_field: sort } }

# M2M: o2m verso la junction + quale m2o della junction punta al lato opposto
- { name: tags, type: m2m, relation: { related: pages_tags, foreign_field: pages_id, junction_field: tags_id } }

# M2A (page builder a blocchi): o2m verso la junction
- { name: blocks, type: m2a, relation: { related: pages_blocks, foreign_field: pages_id, sort_field: sort } }
```

Junction M2A:

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

**Traduzioni**: pattern esplicito con una collection `pages_translations`
(m2o `pages_id`, m2o `languages_code`) e un `o2m` dal parent: nessun tipo
speciale, quindi diff e migrazione funzionano come per ogni altra relazione.

### 4.5 Validazione dello snapshot

`ferro schema validate` (e ogni apply) controlla: identificatori validi
(`[A-Za-z_][A-Za-z0-9_]{0,62}`), prefisso `ferro_` riservato, duplicati, `id` e
campi di sistema non ridichiarati, coerenza delle relazioni (target esistenti,
`foreign_field` che sia davvero un m2o verso il parent, junction con
`junction_field` m2o, M2A con un `m2a_item`, `allowed` esistenti),
`required` + `on_delete: set_null` incompatibili, natural key su campi colonna,
permessi su collection/campi esistenti, ruoli che puntano a policy esistenti.

## 5. Persistenza su PostgreSQL

### 5.1 Tabelle di sistema

Create da migrazioni versionate interne (`ferro_system_migrations`), eseguite
all'avvio sotto advisory lock (sicuro con più istanze).

| Tabella | Contenuto |
|---|---|
| `ferro_collections`, `ferro_roles`, `ferro_policies` | Lo snapshot corrente (definizioni JSONB) |
| `ferro_schema_history` | Ogni piano applicato: hash da/a, piano, snapshot risultante, autore, data |
| `ferro_users`, `ferro_sessions` | Utenti (argon2id), refresh token (solo hash), token statici (solo hash) |
| `ferro_files`, `ferro_folders` | Metadati dei file, cartelle annidate |
| `ferro_activity`, `ferro_revisions` | Audit di ogni scrittura; revisioni con `data` (item dopo) e `delta` (payload) |
| `ferro_flows` · `ferro_flow_queue` · `ferro_flow_runs` | Definizioni dei flow (parte dello snapshot), coda dei job, storico delle esecuzioni |
| `ferro_sync_state` | Per item importato: hash del contenuto all'ultimo sync (strategia `safe`) |
| `ferro_settings` | Impostazioni di progetto |

Le collection di sistema sono esposte al motore items con definizioni
sintetiche che **non includono** `password_hash` e `token_hash`: non sono
selezionabili via API neanche per errore.

### 5.2 Nomi deterministici dei vincoli

`fk_<hash16(table.column)>`, `uq_…`, `ix_…`: identici in ogni ambiente. Il
generatore DDL sa sempre quale vincolo eliminare o rinominare (anche dopo rename
di collection o campi, che rinominano i vincoli di conseguenza).

### 5.3 Foreign key `DEFERRABLE INITIALLY IMMEDIATE`

In uso normale si comportano come FK classiche; durante l'import il motore
esegue `SET CONSTRAINTS ALL DEFERRED` e inserisce in qualunque ordine, poi
`SET CONSTRAINTS ALL IMMEDIATE` verifica tutto prima del commit (anche in dry-run).

## 6. Schema as code: diff, plan, apply

### 6.1 Diff

`diff(corrente, desiderato) → Plan`:

- collection: create / drop / **rename** (hint `renamed_from`) / update meta-sync-system_fields
- campi: add / drop / **rename** (hint `renamed_from`) / alter (tipo, null, default, unique, indice, relazione, validazione, meta)
- ruoli e policy: confronto per **id**

Gli hint `renamed_from` sono "one-shot": servono a distinguere un rename da
drop+add (che perderebbe i dati), vengono applicati e poi rimossi dallo snapshot
salvato.

Ogni cambiamento ha un **rischio**:

| Rischio | Esempi |
|---|---|
| `safe` | nuova collection, nuovo campo nullable, meta, allargamento tipo (`varchar(100)→varchar(200)`, `integer→bigint`, `string→text`) |
| `risky` | campo `required` aggiunto su tabella esistente senza default, `unique` su dati esistenti: possono fallire sui dati (il dry-run lo scopre) |
| `destructive` | drop di collection/campi colonna, cambio tipo non allargante, `system_fields` disattivato |

### 6.2 Plan → SQL

Fasi ordinate (in un'unica transazione):

1. rename tabelle (+ vincoli) → 2. rename colonne (+ vincoli) → 3. drop vincoli
modificati → 4. drop colonne → 5. drop tabelle → 6. create tabelle →
7. add colonne → 8. alter colonne → 9. add vincoli/indici

### 6.3 Apply

`POST /schema/apply` / `ferro schema apply`:

1. advisory lock (un solo apply alla volta per database)
2. validazione dello snapshot desiderato
3. **lock ottimistico**: `expected_hash` deve coincidere con l'hash attuale del
   target (il piano rivisto è quello che verrà applicato; se qualcuno ha
   modificato il target nel frattempo, l'apply viene rifiutato)
4. i cambi `destructive` richiedono `allow_destructive`
5. DDL + salvataggio snapshot + riga in `ferro_schema_history` + `NOTIFY ferro_schema`
6. commit (o rollback se `dry_run`)

Le altre istanze ricevono il `NOTIFY` e ricaricano lo schema in memoria.

## 7. Migrazione dei contenuti tra ambienti

### 7.1 Bundle

```
bundle/
  bundle.json   # formato ferro-bundle/1: items per collection, definizioni delle
                # collection coinvolte, cartelle, metadati file, parent "completi"
  files/<id>    # binari
```

Il bundle contiene le **definizioni** delle collection che trasporta: l'import
verifica la compatibilità campo per campo (esistenza e tipo) **prima** di
scrivere qualsiasi cosa. Se il modello non è allineato l'errore dice di fare
prima `ferro schema apply`.

### 7.2 Selezione

```
--select pages                                  # tutta la collection
--select 'pages:id=0199…,0199…'                 # per id
--select 'pages:{"slug":{"_in":["home","chi-siamo"]}}'   # per filtro
--shallow                                       # solo gli item, senza dipendenze
```

### 7.3 Chiusura delle dipendenze (export)

Ogni item visitato ha un livello: **shallow** (serve come dipendenza) o **deep**
(se ne vuole anche il contenuto "posseduto").

| Relazione | Regola di default | `sync:` alternativo |
|---|---|---|
| radici della selezione | deep | — |
| `m2o` | target incluso **shallow** (le sue m2o seguite, i suoi figli no) | `deep` (anche i figli), `reference` / `skip` (non esportato, deve esistere nel target) |
| `o2m` / `m2m` / `m2a` di un item deep | figli/junction inclusi **deep** | `reference` / `skip` |
| `m2a_item` (blocchi) | target incluso **deep** (composizione) | `reference` / `skip` |
| `file` | riga `ferro_files` + binario + catena di cartelle | — |
| utenti | mai esportati; `user_created/updated` rimossi, altri riferimenti a utenti inesistenti nel target → `null` (con warning) | — |
| collection con `sync.exclude` | mai esportata | — |

Esempio: `--select 'pages:{"slug":{"_eq":"home"}}'` esporta la pagina, la sua
categoria (shallow: non tutte le pagine della categoria), le righe di
`pages_blocks`, ogni blocco con i suoi figli, le immagini con le cartelle.

### 7.4 Import

In una transazione con vincoli differiti:

1. formato e compatibilità dello schema (errori → niente viene scritto)
2. **natural key**: per gli item il cui id non esiste nel target, se esiste un
   item con la stessa natural key si adotta l'id del target, e tutte le
   referenze nel bundle (m2o, m2a_item) vengono riscritte
3. riferimenti a utenti inesistenti → `null`
4. cartelle, file (il binario deve essere già nello storage del target)
5. collection in ordine topologico; per ogni item:
   - non esiste → `create`
   - uguale (ignorando `date_*`/`user_*`) → `unchanged`
   - diverso → secondo la **strategia**:

| Strategia | Item diverso nel target |
|---|---|
| `overwrite` (default) | aggiornato |
| `safe` | aggiornato solo se nel target non è stato modificato dall'ultimo sync (hash in `ferro_sync_state`), altrimenti **conflict** |
| `insert-only` | `skip` |

6. `--prune`: per i parent esportati deep, i figli presenti nel target ma non
   nella sorgente vengono **eliminati** (junction, figli con FK obbligatoria) o
   **staccati** (FK nullable)
7. `SET CONSTRAINTS ALL IMMEDIATE`: ogni FK deve essere valida (un item che
   punta a qualcosa che non è né nel bundle né nel target fa fallire l'import con
   messaggio esplicito)
8. con conflitti e senza `--force` → rollback; con `dry_run` → rollback; altrimenti
   commit + activity `import` con il riepilogo

Il report contiene, per ogni item non invariato, l'azione e le differenze campo
per campo (prima/dopo), più l'id sorgente se è stato rimappato.

### 7.5 Comandi

```bash
# cosa cambierebbe in prod (niente viene scritto)
ferro data diff --from dev --to prod --select 'pages:{"slug":{"_eq":"home"}}' --report diff.html

# promozione in un passo (file inclusi, solo quelli mancanti)
ferro data push --from dev --to prod --select pages --strategy safe --prune

# via file (es. approvazione manuale, ambienti non raggiungibili tra loro)
ferro data export --from dev --select pages --out ./bundle-home
ferro data import --to prod --bundle ./bundle-home --dry-run --report import.html
ferro data import --to prod --bundle ./bundle-home --yes
```

L'import è **idempotente**: ripeterlo produce solo `unchanged`.

## 8. API REST

Formato di risposta `{ "data": …, "meta": … }`, errori
`{ "errors": [{ "message": …, "extensions": { "code": … } }] }`.

### 8.1 Items

| Metodo e path | |
|---|---|
| `GET /items/:collection` | lista (`fields`, `filter`, `sort`, `limit`, `offset`, `page`, `search`, `meta=total_count,filter_count`) |
| `GET /items/:collection/:id` | singolo |
| `POST /items/:collection` | crea (oggetto o array); singleton: upsert |
| `PATCH /items/:collection/:id` | aggiorna |
| `PATCH /items/:collection` | batch: `{ keys: [...], data: {...} }` o `[{ id, ... }]` |
| `DELETE /items/:collection/:id`, `DELETE /items/:collection` (`[ids]`) | elimina |

**fields**: `*`, `title,author.name`, `blocks.item.*`, `*.*`. Gli alias
(o2m/m2m/m2a) con `*` restituiscono la lista di id; con un sotto-albero gli
oggetti. Per M2A il sotto-albero si applica a ogni collection target (campi
inesistenti ignorati).

**filter** (JSON): `_eq _neq _lt _lte _gt _gte _in _nin _null _nnull _empty _nempty
_contains _ncontains _icontains _nicontains _starts_with _nstarts_with _istarts_with
_ends_with _nends_with _iends_with _between _nbetween`, logici `_and _or`,
relazionali `{ author: { name: { _eq: … } } }` (m2o), `{ blocks: { _some: … } }` /
`_none` (o2m). Variabili: `$CURRENT_USER`, `$CURRENT_ROLE`, `$NOW`.

**Scritture annidate**:

```json
{
  "title": "Home",
  "category": { "code": "corporate" },
  "blocks": [
    { "collection": "block_hero", "item": { "headline": "Benvenuti" }, "sort": 1 },
    { "id": "0199…", "sort": 2 }
  ]
}
```

- m2o con oggetto: crea (o aggiorna se ha un `id` esistente) e collega
- o2m/m2m/m2a come **array**: sostituzione completa (elementi assenti eliminati se
  junction/FK obbligatoria, altrimenti staccati)
- o2m come **oggetto** `{ create: [], update: [], delete: [] }`: modifica parziale
- `m2a_item` con oggetto: crea l'item nella collection indicata da `collection`

Tutto in una transazione, permessi verificati a ogni livello.

### 8.2 Altre risorse

| Area | Endpoint |
|---|---|
| Auth | `POST /auth/login` `{email,password}` → `{access_token, refresh_token, expires}`, `POST /auth/refresh`, `POST /auth/logout` |
| Modello | `GET/POST /collections`, `GET/PATCH/DELETE /collections/:name`, `POST /fields/:collection`, `PATCH/DELETE /fields/:collection/:field` (query `allow_destructive`, `dry_run`) |
| Accessi | `GET /roles`, `PUT/DELETE /roles/:id`, `GET /policies`, `PUT/DELETE /policies/:id` |
| Schema | `GET /schema/snapshot` (`?format=yaml`), `POST /schema/plan`, `POST /schema/apply`, `GET /schema/history` |
| Utenti | `GET/POST /users`, `GET /users/me`, `GET/PATCH/DELETE /users/:id`, `POST /users/:id/token` |
| File | `GET/POST /files` (multipart, `?replace=<id>`), `GET/PATCH/DELETE /files/:id`, `GET /assets/:id` (`?download`, `?access_token=`), `GET/POST /folders` |
| Audit | `GET /activity`, `GET /revisions` |
| Sync | `POST /sync/export`, `POST /sync/missing-files`, `GET/PUT /sync/files/:id`, `POST /sync/import`, `DELETE /sync/state` |
| GraphQL | `POST /graphql`, `GET /graphql?query=…` (solo query), `GET /graphql/schema` (SDL), vedi §8.3 |
| Realtime | `GET /websocket` (WebSocket), vedi §8.5 |
| Server | `GET /server/health`, `GET /server/info` |

### 8.3 GraphQL

Stessa autenticazione, stessi permessi, stesse regole del REST: i campi radice
chiamano gli stessi servizi (`items::read_many`, `create_one`, `update_one`…).
Lo schema (SDL) è generato dal modello: `GET /graphql/schema` o il pulsante
"Download schema" del playground in Admin UI.

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
  create_pages_item(data: { title: "Nuova", slug: "nuova",
      blocks: [{ collection: "block_text", item: { body: "Ciao" }, sort: 1 }] }) { id }
}
```

| Radice | Significato |
|---|---|
| `<c>(filter, sort, limit, offset, page, search)` | lista (`[<c>!]!`) |
| `<c>_by_id(id)` · `<c>_count(filter, search)` | singolo o `null` · conteggio |
| `<c>` (singleton) · `update_<c>(data)` | item unico |
| `create_<c>_item(s)(data)` | creazione (scrittura annidata come nel REST) |
| `update_<c>_item(id, data)` · `update_<c>_items(ids, data)` | aggiornamento |
| `delete_<c>_item(id)` · `delete_<c>_items(ids)` | cancellazione → `{ id }` / `{ ids }` |

- I tipi hanno il nome della collection; le relazioni sono tipate (`category { code }`),
  i campi M2A (`item`) sono **union** dei tipi ammessi, interrogabili con `... on <collection>`.
- Filtri e payload usano lo scalare `JSON` con gli stessi formati del REST
  (`{"status":{"_eq":"published"}}`, scritture annidate): un solo vocabolario da conoscere.
- Supportati: alias, variabili con default, fragment (nominati e inline), `@skip`/`@include`,
  `__typename`. Tutte le mutation di una richiesta girano in **una transazione**.
- Gli errori sono in `errors[]` (HTTP 200) con `extensions.code` (`FORBIDDEN`, `NOT_FOUND`, …);
  una query può avere dati parziali, una mutation con errore annulla tutto (`data: null`).
- Le mutation via `GET` sono rifiutate. Profondità massima di selezione: 32.
- **Non supportati**: subscription; introspection (`__schema`/`__type`), quindi GraphiQL e gli
  strumenti che la richiedono non funzionano: usare l'SDL scaricato (per esempio con
  `graphql-codegen`) o il playground dell'Admin UI; stesso nome di campo con alias diversi e
  sotto-selezioni diverse nello stesso oggetto (l'ultimo vince).

### 8.4 Flow: webhook, hook, automazioni, endpoint personalizzati

Un solo concetto, il **flow**, copre ciò che altrove si divide in webhook, flow e hook.
I flow fanno parte dello **snapshot** (`schema/flows/*.yaml`, id UUID stabili): si rivedono in PR,
si confrontano (`ferro schema diff`) e si promuovono tra ambienti insieme a modello e permessi.

| Vuoi | Trigger | Esempio |
|---|---|---|
| Webhook in uscita | `event` + operazione `request` | `notify-on-publish.yaml` |
| Hook che valida o riscrive una scrittura | `event` con `blocking: true` | `require-title.yaml` |
| Job pianificato | `schedule` (cron, UTC) | `weekly-digest.yaml` |
| Endpoint personalizzato | `webhook` (`/x/<slug>`, `/hooks/<id>`) | `hello-endpoint.yaml` |
| Pulsante nell'editor | `manual` | `unpublish.yaml` |

**Trigger.** `event`: `<collection>.items.<create|update|delete|*>` (anche `*.items.update`, `*`).
`webhook`: metodi, `access` (`public` | `authenticated` | `admin`), `secret_env` (nome di una variabile
d'ambiente con un segreto condiviso, atteso nell'header `X-Ferro-Secret`: il segreto non entra mai nello
snapshot), `run_async` (risponde 202 ed esegue in background).

**Operazioni** (`type`): `log`, `request` (HTTP), `item_create`, `item_update`, `item_delete`, `item_read`,
`condition`, `transform`, `trigger_flow`, `fail`. Si eseguono lungo i collegamenti `resolve` / `reject`
(non nell'ordine dell'elenco); `condition` segue `resolve` se la regola corrisponde e `reject` altrimenti;
un errore segue `reject` se c'è, altrimenti fa fallire il flow.

**Catena di dati.** Ogni opzione stringa accetta `{{ percorso }}`: `$trigger` (evento, `payload`, `key`,
`keys`, `item`, `query`, `body`, `headers`), `$last`, `$env` (variabili `FERRO_FLOW_ENV_<NOME>`),
`$accountability`, e un'entrata per ogni operazione già eseguita (`{{ fetch.data.email }}`). Una stringa
che è esattamente un riferimento conserva il tipo JSON (numeri, array, oggetti). Il campo `output` è un
template reso a fine flow: è la risposta HTTP di un endpoint, o il nuovo payload di un hook bloccante.

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

**Esecuzione.**
- Gli eventi non bloccanti sono accodati in `ferro_flow_queue` *nella stessa transazione della scrittura*:
  esistono solo se la scrittura va a buon fine. Un worker su ogni istanza li prende con
  `FOR UPDATE SKIP LOCKED`: ogni job gira **una volta sola**, qualunque istanza lo prenda.
- Un flow è **una transazione**: se fallisce, nulla di ciò che ha scritto viene mantenuto.
- Gli hook bloccanti girano dentro la scrittura, dopo il controllo dei permessi; il payload riscritto è
  **validato di nuovo** (permessi sui campi, regole di validazione) e un errore annulla la scrittura.
- Le scritture fatte dai flow hanno origine `flow` e **non innescano altri flow** (niente cicli); il
  concatenamento è esplicito (`trigger_flow`, al massimo 5 livelli).
- Gli schedule scattano una volta per minuto (chiave di deduplica); i minuti persi con tutte le istanze
  spente non si recuperano.
- Storico delle esecuzioni (log di ogni passo) in `ferro_flow_runs`, conservato 30 giorni:
  `GET /flows/:id/runs` o Admin → Automations → Runs.
- `run_as`: `system` (default, senza restrizioni) oppure `caller` (permessi dell'utente che ha causato
  l'esecuzione: hook bloccanti, webhook sincroni, azioni manuali).

**Sicurezza delle richieste in uscita.** Solo http/https; per default sono rifiutati host privati
(localhost, 10/8, 172.16/12, 192.168/16, link-local, `.internal`, `.local`). Nessun redirect seguito,
timeout 10 s, risposta limitata a 1 MiB. `FERRO_FLOW_ALLOWED_HOSTS` restringe ai soli host elencati;
`FERRO_FLOW_ALLOW_PRIVATE=true` ammette gli indirizzi privati. Limite noto: un nome DNS che *risolve* a
un indirizzo privato non viene intercettato (usare la lista di host ammessi).

| Endpoint | |
|---|---|
| `GET /flows` · `PUT/DELETE /flows/:id` | gestione (admin), con anteprima del piano come il modello |
| `POST /flows/:id/run` | esecuzione manuale `{collection, keys, payload}` |
| `GET /flows/:id/runs` | storico |
| `GET /flow-actions?collection=` | azioni manuali che l'utente può eseguire |
| `ANY /hooks/:id` · `ANY /x/:slug` | endpoint in ingresso dei flow `webhook` |

### 8.5 Realtime (WebSocket)

`GET /websocket` apre una connessione su cui il client si sottoscrive ai cambiamenti di una
collection. Messaggi JSON (frame di testo):

```text
→ { "type": "auth", "access_token": "…" }        (oppure ?access_token= nell'URL; senza token: policy pubbliche)
← { "type": "auth", "status": "ok" | "error" }
→ { "type": "subscribe", "uid": "a", "collection": "pages", "event": "update",
    "query": { "fields": ["title", "blocks.*"], "filter": { "status": { "_eq": "published" } } } }
← { "type": "subscription", "event": "init", "uid": "a" }
← { "type": "subscription", "event": "create|update|delete|resync", "uid": "a", "data": [ … ] }
→ { "type": "unsubscribe", "uid": "a" }     → { "type": "ping" }  ←  { "type": "pong" }
← { "type": "error", "uid": "a", "code": "NOT_FOUND|FORBIDDEN|INVALID_PAYLOAD|LIMIT", "message": "…" }
```

**Come funziona.** Ogni scrittura di item (REST, GraphQL, flow) pubblica con `pg_notify`, **dentro la
stessa transazione**, una notifica minima (collection, azione, al più 40 id): PostgreSQL la consegna
solo se la transazione va a buon fine, e a tutte le istanze. Ogni istanza la inoltra ai propri client.
Una scrittura annullata (vincolo violato, hook bloccante che rifiuta) non produce alcun evento.

**Sicurezza.** La notifica non contiene dati: per ogni sottoscrizione il server **rilegge** gli item con i
permessi di quel client, i campi richiesti e (in AND) il filtro della sottoscrizione. Un client non
vede mai ciò che non potrebbe leggere via REST: un anonimo con la policy pubblica riceve solo le
pagine pubblicate, un editor solo i campi consentiti. L'identità è ricontrollata a **ogni consegna**: un
token scaduto o un utente sospeso viene disconnesso all'evento successivo.

**Semantica.**
- `create`/`update`: `data` = gli item (campi della sottoscrizione). Un item che con l'update esce dal
  filtro della sottoscrizione non genera evento.
- `delete`: `data` = gli id (l'item non esiste più; il filtro non si può valutare).
- `resync`: possono essere stati persi eventi (connessione del server al database interrotta, client
  troppo lento): rileggere. `truncated: true`: la scrittura toccava più di 40 item.
- Limiti: 50 sottoscrizioni per connessione, 2000 connessioni per istanza, messaggi fino a 64 KB, ping del
  server ogni 30 s e disconnessione dopo 100 s di silenzio. Collection di sistema non sottoscrivibili.
- Non generano eventi: l'import di dati (`ferro data import/push`) e i file (`/files`). Dopo un import
  i client vedono i dati alla successiva lettura.

**Admin UI.** Le liste e gli editor si sottoscrivono da soli e mostrano un avviso ("un altro utente ha
modificato…", con pulsante per ricaricare) senza mai sovrascrivere ciò che l'utente sta scrivendo; il
salvataggio dell'utente stesso non genera l'avviso. Le estensioni usano `ferro.realtime.subscribe(...)` e
`ferro.onLeave(...)` (vedi `examples/site/extensions/hello-panel`).

## 9. Autenticazione e permessi

- **Password**: argon2id. **Access token**: JWT HS256 (15 min, `FERRO_SECRET`).
  **Refresh token**: casuale, ruotato a ogni uso, salvato solo come hash.
- **Token statici** (`ferro user token`, `POST /users/:id/token`): per CI e per la
  CLI tra ambienti; salvati come SHA-256, mostrati una volta sola.
- **Ruolo → policy**: un utente ha un ruolo, il ruolo ha N policy. Le policy con
  `public: true` si applicano anche agli anonimi e si sommano a quelle del ruolo.
- **Permesso** = (collection, azione `create|read|update|delete`):
  - `fields`: campi leggibili/scrivibili (`*` = tutti)
  - `filter`: regola sugli item (es. `{ user_created: { _eq: $CURRENT_USER } }`)
  - `validation`: condizione che il payload deve rispettare
  - `presets`: valori di default in creazione
- Più permessi sulla stessa coppia si **uniscono**: campi in unione, filtri in OR
  (un permesso senza filtro dà accesso a tutti gli item).
- `admin_access` bypassa tutto ed è richiesto per schema, sync, utenti.
- Il ruolo e la policy **Administrator** hanno id fissi, identici in ogni ambiente.

### Hardening verificato dai test (`tests/e2e/05-security.mjs`)

- **Nessun valore utente nell'SQL**: valori sempre parametri, identificatori validati e quotati. Provati nomi di collection/campo,
  filtri, ordinamenti, `fields` e percorsi con tentativi di iniezione.
- **Permessi per campo anche nei canali indiretti**: un filtro, un ordinamento, una ricerca (`search`) o un conteggio
  (`meta=filter_count`, `_count`) su un campo che l'utente non può leggere è rifiutato (403), e la ricerca ignora quei campi;
  nei filtri relazionali valgono i permessi della collection collegata. Altrimenti si potrebbe dedurre il contenuto di un campo
  protetto a tentativi. Lo stesso vale per GraphQL e per le sottoscrizioni realtime (stesso percorso di lettura).
- **Il jolly `collection: "*"` non copre le collection di sistema** (`ferro_*`): utenti, attività e revisioni richiedono un
  permesso esplicito.
- **Token**: firma, algoritmo (`none` rifiutato), issuer, scadenza (tolleranza 5 s) e soggetto sono verificati; il refresh token
  ruota a ogni uso e un token già usato non può essere riutilizzato; la rotazione del token statico invalida il precedente.
- **Blocco dei tentativi di accesso**: dopo `FERRO_LOGIN_MAX_FAILURES` (10) password errate per la stessa email la email è
  bloccata per `FERRO_LOGIN_LOCKOUT_SECS` (900 s) con 429, anche con la password giusta; le email inesistenti contano allo stesso
  modo (nessuna enumerazione degli account); per indirizzo il limite è 10 volte più alto. Il contatore è per istanza.
- **Assegnazione di massa**: `user_created`, `date_created` e gli altri campi gestiti dal server sono ignorati se inviati.
- **Percorsi**: id di file solo UUID, chiavi di storage validate, `ServeDir` rifiuta `..` (anche codificato).
- **Input anomali**: JSON non valido o annidato 5000 livelli, caratteri NUL, overflow numerici, date non valide danno 4xx, mai 500.
- **CORS**: nessuna origine ammessa di default.

## 10. File e asset

- Upload multipart; il binario è salvato con chiave = id del file (UUID v7),
  checksum SHA-256, MIME da header o estensione.
- `?replace=<id>` sostituisce il binario mantenendo id e riferimenti.
- `GET /assets/:id` serve il binario solo se il chiamante può leggere la riga in
  `ferro_files` (le policy pubbliche abilitano gli asset pubblici); `ETag` = checksum.
- **Storage a più location.** Ogni location ha un nome e un driver; ogni riga di
  `ferro_files` ricorda la propria (`storage`), i nuovi upload vanno alla location di
  default. Così si può affiancare un disco a un bucket e spostare i file con
  `ferro storage migrate`. La chiave dell'oggetto resta l'id del file (più
  l'eventuale prefisso della location).
- Driver: `local` (directory, volume condiviso con più istanze), `s3` (Amazon S3 e
  compatibili: MinIO, R2, B2, Wasabi, Spaces, GCS con chiavi HMAC; credenziali esplicite
  oppure catena AWS: variabili, profilo, ruolo ECS/EKS/EC2), `azure` (Blob Storage con
  token SAS). All'avvio ogni location viene provata con un oggetto di prova
  (scrittura + cancellazione): bucket inesistente o credenziali errate fermano il
  server con un messaggio chiaro, invece di fallire al primo upload.
- **Coerenza tra DB e storage.** Le cancellazioni dei binari avvengono *dopo* il commit
  (un rollback non perde mai un file); un upload fallito dopo la scrittura del blob
  lo rimuove (nessun orfano); sostituire un file spostato in un'altra location elimina
  il vecchio blob solo a commit avvenuto. Limite noto: `?replace=` sulla *stessa*
  location sovrascrive l'oggetto prima del commit (un errore successivo nella stessa
  richiesta lascia il blob nuovo con la riga vecchia); lo storage non è transazionale.
- **Download.** Di default il server legge l'oggetto e lo inoltra. Con
  `FERRO_STORAGE_<NOME>_PRESIGN=<secondi>` risponde `307` verso un URL firmato
  (`Content-Disposition` e tipo già nella firma, `no-store`): i permessi sono verificati
  *prima* di firmare e il traffico dei binari non passa dal server. L'Admin UI usa
  `<img src>`, quindi funziona senza CORS; un `fetch` da altra origine richiede CORS sul bucket.
- Nel sync i binari viaggiano come prima (`PUT /sync/files/:id`) verso la location di
  default del target, indipendentemente da dove stanno nell'origine.
- Nel sync si trasferiscono solo i binari che il target non ha o con checksum diverso.

## 11. CLI `ferro`

```
ferro serve                                   avvia il server (variabili FERRO_*)
ferro migrate                                 crea/aggiorna le tabelle di sistema
ferro init                                    crea ferro.toml e schema/
ferro env list | status                       ambienti, raggiungibilità, hash dello schema
ferro schema pull     --from dev              ambiente → schema/ (da committare)
ferro schema validate                         valida schema/
ferro schema import-directus --file snap.json  snapshot Directus → schema/
ferro schema diff     [--from schema] --to prod [--exit-code] [--report f.html] [--json]
ferro schema apply    [--from schema] --to prod [--dry-run] [--allow-destructive] [--yes]
ferro data export     --from dev --select … --out dir
ferro data import     --to prod --bundle dir [--strategy …] [--prune] [--dry-run] [--force] [--yes]
ferro data diff       --from dev --to prod --select … [--exit-code] [--report f.html]
ferro data push       --from dev --to prod --select … [opzioni di import]
ferro storage list                            location configurate, file e byte per location
ferro storage check [--exit-code]             prova di scrittura + ogni file esiste dove dice la riga (exit 2 se manca)
ferro storage migrate --from local --to s3 [--delete-source] [--dry-run]
                                              copia (checksum verificato), aggiorna la riga, poi elimina l'origine
ferro user create     --email … --password … [--admin | --role <id>]
ferro user token      --email …
```

`--from`/`--to` accettano il nome di un ambiente di `ferro.toml` o un percorso
(directory schema o file `.yaml`/`.json`). Codici di uscita: `0` ok, `1` errore,
`2` differenze trovate (con `--exit-code`, per le pipeline).

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
protected = true      # scritture solo con --yes o digitando il nome dell'ambiente
```

## 12. CI/CD e flusso di lavoro

### Modello

```
 sviluppatore             git (PR)                 test                prod
 ────────────             ────────                 ────                ────
 modifica da UI dev  ──►  ferro schema pull  ──►  review del diff
 o YAML a mano            (commit schema/)        ferro schema apply --to test
                                                   ferro schema diff --to prod --report
                                                                         ferro schema apply --to prod --yes
```

- **Drift detection** notturna: `ferro schema diff --to prod --exit-code` fallisce
  se prod è stato modificato fuori da git.
- **Promozione contenuti** come job parametrizzato (sostituisce i Jenkinsfile per
  collection di `migrate-data`/`diff-data`): `ferro data diff` per l'anteprima
  (report allegato alla build), `ferro data push` dopo approvazione.

Esempio Jenkins: vedi [`deploy/Jenkinsfile.example`](../deploy/Jenkinsfile.example).

## 13. Deploy, esercizio, sicurezza

### Configurazione (variabili d'ambiente)

| Variabile | Default | |
|---|---|---|
| `FERRO_DATABASE_URL` | — | obbligatoria |
| `FERRO_SECRET` | — | obbligatoria, ≥ 32 caratteri |
| `FERRO_ENV` | `local` | nome dell'ambiente (bundle, report) |
| `FERRO_BIND` | `0.0.0.0:8055` | |
| `FERRO_STORAGE_LOCATIONS` | `local` | nomi delle location, separati da virgola |
| `FERRO_STORAGE_DEFAULT` | la prima | location dei nuovi upload |
| `FERRO_STORAGE_LOCAL_ROOT` | `./uploads` | directory della location `local` |
| `FERRO_STORAGE_<NOME>_DRIVER` | — | `local`, `s3` o `azure` (per `local` è il default) |
| `FERRO_STORAGE_<NOME>_BUCKET`, `_REGION`, `_PREFIX`, `_ENDPOINT`, `_FORCE_PATH_STYLE` | — / `AWS_REGION` / — / — / vero con endpoint | driver `s3` |
| `FERRO_STORAGE_<NOME>_ACCESS_KEY_ID`, `_SECRET_ACCESS_KEY`, `_SESSION_TOKEN` | catena AWS | credenziali (opzionali) |
| `FERRO_STORAGE_<NOME>_PRESIGN` | spento | secondi di validità dell'URL firmato (redirect 307) |
| `FERRO_STORAGE_<NOME>_ACCOUNT_URL`, `_CONTAINER`, `_SAS_TOKEN`, `_PREFIX` | — | driver `azure` |
| `FERRO_ACCESS_TOKEN_TTL` / `FERRO_REFRESH_TOKEN_TTL` | 900 / 604800 s | |
| `FERRO_MAX_BODY_MB` | 256 | upload e bundle |
| `FERRO_QUERY_LIMIT_MAX` | -1 | limite massimo per `limit` (-1 = nessuno) |
| `FERRO_ADMIN_EMAIL` / `FERRO_ADMIN_PASSWORD` | — | primo amministratore se non esistono utenti |
| `FERRO_CORS_ORIGINS` | — | lista separata da virgole o `*` |
| `FERRO_ADMIN_DIR` | — | build statica dell'Admin UI servita su `/admin` |
| `FERRO_EXTENSIONS_DIR` | — | estensioni dell'Admin UI, servite su `/extensions` |
| `FERRO_FLOW_ENV_<NOME>` | — | segreti disponibili ai flow come `{{ $env.NOME }}` |
| `FERRO_FLOW_ALLOWED_HOSTS` | — | se impostato, i flow possono chiamare solo questi host |
| `FERRO_FLOW_ALLOW_PRIVATE` | `false` | ammette richieste dei flow verso indirizzi privati |
| `FERRO_DB_MAX_CONNECTIONS` | 10 | |
| `FERRO_LOGIN_MAX_FAILURES` / `FERRO_LOGIN_LOCKOUT_SECS` | 10 / 900 | blocco dei tentativi di accesso per email |
| `FERRO_LOG` | `info` | filtro `tracing` |

### Scalabilità

Server stateless: più istanze dietro un load balancer, schema riallineato via
`LISTEN/NOTIFY`, migrazioni e apply serializzati da advisory lock. Con più istanze lo storage
deve essere condiviso: bucket S3/Azure, oppure volume condiviso per `local`.

### Sicurezza

- Nessun valore utente concatenato nell'SQL: identificatori validati e quotati,
  valori sempre parametri (`CAST($n AS <tipo dallo schema>)`).
- Campi sensibili non esposti dal motore; token e refresh salvati solo come hash.
- Chiavi di storage validate (niente path traversal).
- Operazioni di schema/sync/utenti solo con `admin_access`; ambienti `protected`
  con doppia conferma nella CLI.

### Backup

Il database è l'unica sorgente di verità per contenuti e utenti (`pg_dump`
standard); lo snapshot è anche in git; lo storage va salvato a parte.

## 14. Migrazione da Directus

Piano consigliato (il convertitore del **modello** esiste, quello dei **dati** è in roadmap):

1. **Modello**: `npx directus schema snapshot -y snapshot.json`, poi
   `ferro schema import-directus --file snapshot.json` scrive `schema/`
   (`collections`/`fields`/`relations` di Directus → `fields` con `relation`;
   le junction M2M/M2A restano collection dichiarate; i quattro campi di sistema
   Directus diventano `system_fields: true`; cartelle e campi solo grafici sono
   scartati; ogni scarto o approssimazione è stampato come warning, e il risultato
   viene validato). Ruoli e permessi non sono convertiti (punto 2).
2. **Policy**: export di `directus_policies`/`directus_permissions` → policy Ferro
   con UUID nuovi stabili (una volta, poi vivono in git).
3. **Id**: per le collection con PK integer, id Ferro = **UUID v5**
   deterministico di `(collection, id_directus)`. Su più ambienti Directus con id
   diversi per lo stesso contenuto, dichiarare `sync.natural_key` e lasciare che
   l'import rimappi.
4. **Dati**: lettura via API/DB Directus → bundle Ferro → `ferro data import`.
5. **File**: copia dei binari con chiave = nuovo id, `ferro_folders` dalle
   `directus_folders`.
6. **Frontend**: stessi concetti (`/items`, `fields`, `filter`, `{ data }`); cambiano
   gli id (uuid) e i nomi delle collection di sistema.

## 15. Admin UI

Cartella [`admin/`](../admin/): SPA in **JavaScript puro** (moduli ES, nessuna
dipendenza, nessuna build). Il server la serve su `/admin` quando è impostato
`FERRO_ADMIN_DIR`; usa solo l'API pubblica. Per sviluppare contro un altro server:
`/admin/?api=https://host` (serve CORS sull'altro server).

| Sezione | Cosa fa |
|---|---|
| GraphQL | Playground: editor, variabili, risultato, download dello schema SDL |
| Automations | Flow: modelli di partenza, editor di trigger e operazioni, anteprima del piano, storico delle esecuzioni, esecuzione manuale |
| Content | Elenco con ricerca, filtri, ordinamento, paginazione, cancellazione multipla; scheda item con form **generato dal modello** |
| Form | Widget per ogni tipo; selettori M2O con etichette; file (upload, scelta, anteprima); **O2M** e **M2M** inline (riordino con sort field); **M2A a blocchi** (aggiungi/riordina/rimuovi blocchi, form annidati fino a 3 livelli); validazione dei campi obbligatori; i campi non leggibili per i permessi non vengono mai riscritti a `null` |
| History | Revisioni di un item con ripristino dei campi semplici |
| Data model | Collection e campi (tipi, relazioni, default, interfaccia, validazione). **Ogni salvataggio fa prima un dry-run e mostra il piano e l'SQL**; i cambi distruttivi sono evidenziati e richiedono conferma |
| Access control | Ruoli e policy con permessi (campi, filtro item, validazione, preset), con la stessa anteprima del piano |
| Files | Libreria con cartelle, drag & drop, metadati, sostituzione del binario, download |
| Users | Utenti, ruoli, stato, password, generazione token API |
| Environment | Nome e hash dello schema, storico delle modifiche, download dello snapshot YAML, **allineamento a un altro ambiente** (URL + token) o a uno snapshot JSON, sempre con anteprima |
| Activity | Audit per collection/azione |

Sicurezza: tutto il testo è inserito come nodi di testo (mai `innerHTML`); il
refresh token è in `localStorage`, l'access token solo in memoria; gli asset
nelle anteprime usano `?access_token=`.

Test: `cd admin && node --test test/*.test.mjs` esegue i moduli reali su un DOM
finto con un server simulato (generazione e rilettura del payload annidato,
flusso "salva → anteprima → applica", rendering di tutte le viste).

### Estensioni

Cartella indicata da `FERRO_EXTENSIONS_DIR`, una sottocartella per estensione:
`<nome>/manifest.json` (`name`, `version`, `description`, `entry`) e un modulo ES che esporta
`default function register(ferro)`. L'Admin UI le carica all'avvio (`GET /server/extensions`, file serviti
su `/extensions`); un'estensione che fallisce è segnalata in console e non blocca l'app.

```js
export default function register(ferro) {
  ferro.addPage({ id: 'overview', title: 'Overview', render: (root) => { /* ... */ } });     // voce nel menu
  ferro.addInterface('stars', (field, value, ctx) => ({ el, get: () => value }));            // widget di campo
  ferro.addItemAction({ label: 'Copy slug', collections: ['pages'], run: async ({ item }) => {} });
}
```

`ferro.api` (client HTTP autenticato), `ferro.dom` (`h`, `button`, `modal`, `toast`…) e `ferro.schema`
sono a disposizione. Un'interfaccia si attiva impostando `meta.interface` del campo al suo nome e ha la
precedenza sui widget predefiniti. Esempio: `examples/site/extensions/hello-panel`.

Le estensioni girano **con i privilegi dell'utente che usa l'admin**: installare solo codice di cui ci si
fida. La logica lato server si esprime con i flow (anche chiamando i vostri servizi via `request`);
codice Rust arbitrario richiede di compilarlo nel binario.

## 16. Stato, limiti e roadmap

### Implementato

- Modello completo (tipi, relazioni M2O/O2M/M2M/M2A/file, singleton, system
  fields, validazioni), validazione dello snapshot
- Diff/plan/apply transazionale con rename, rischio, lock ottimistico, storico,
  multi-istanza
- API REST items con filtri, ricerca, sort, paginazione, conteggi, espansione
  relazionale, scritture annidate, singleton
- Auth (password, JWT, refresh, token statici), RBAC con policy
  (campi/filtri/validazione/preset, pubbliche)
- File e cartelle con storage locale, S3 (e compatibili) o Azure Blob, asset con permessi
- Activity e revisioni
- Sync: export con chiusura delle dipendenze, import con compatibilità,
  natural key, strategie, prune, verifica FK, dry-run, report HTML/JSON
- CLI per tutte le operazioni di ambiente
- Admin UI (vedi §15)
- GraphQL (query e mutation, union per i blocchi M2A, SDL generato; vedi §8.3)
- Realtime via WebSocket con permessi per client e propagazione tra istanze (§8.5)
- Flow (webhook, hook bloccanti e non, job pianificati, endpoint personalizzati, azioni manuali; §8.4) ed estensioni dell'Admin UI (§15)

### Limiti noti

- **Stato della verifica** (2026-10-09): il workspace compila senza warning; 64 test Rust, 30 test dell'Admin UI,
  **312 controlli di integrazione** su due ambienti con PostgreSQL 16 reale (schema, sincronizzazione, flow, GraphQL, sicurezza,
  realtime, tra cui due istanze sullo stesso database) e **36 controlli in un browser vero** (Edge/Chromium). Si eseguono con
  `scripts/ci.sh [browser]` (workflow GitHub in `.github/workflows/ci.yml`); dettagli in `tests/README.md`.
- Mai eseguiti: l'immagine Docker, il compose, il Jenkinsfile di esempio e la prima esecuzione della CI su un runner Linux.
- Nessun test di carico; nessun audit di sicurezza indipendente.
- I filtri relazionali annidati supportano m2o e o2m-like (non `m2a_item`).
- Filtri in query string solo in forma JSON (`filter={…}`), non in forma
  `filter[campo][_eq]=`.
- Binari tenuti in memoria in upload/sync (adeguato fino a ~centinaia di MB per file).
- Prune non elimina gli item M2A rimasti orfani dopo la rimozione della riga di junction (come Directus).
- Le cancellazioni salvano l'ultimo contenuto in `ferro_revisions`, ma non c'è ancora un endpoint di ripristino.

### Roadmap

1. Test di integrazione su PostgreSQL reale (testcontainers) e CI
2. Admin UI: wizard per relazioni M2M/M2A, colonne e filtri salvati, editor WYSIWYG, test su browser reale
3. Trasformazioni immagini (`/assets/:id?width=…`), streaming di upload/download (oggi i file stanno in memoria, max `FERRO_MAX_BODY_MB`), multipart upload S3, Azure con SharedKey/Entra ID
4. Convertitore dei **dati** da Directus (`directus_uuid(collection, id)` in `ferro-core` fornisce già gli id deterministici)
5. Introspection e subscription GraphQL, filtri in forma `filter[...]`
6. Eventi di sistema per i flow (login, upload file, import), operazioni `email` e `sleep`; realtime anche per import e file; subscription GraphQL
7. Bozze/versioni dei contenuti e pubblicazione programmata
8. OpenAPI generata dallo snapshot (tipi per i frontend)
