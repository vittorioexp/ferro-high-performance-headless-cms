# Ferro CMS

Headless CMS in Rust, ispirato a Directus, costruito attorno alla **promozione di
modello, permessi e contenuti tra ambienti** (dev → test → prod).

- **Schema as code**: collection, campi, relazioni, ruoli e policy in YAML versionati in git;
  `diff` / `plan` / `apply` transazionale con livello di rischio e lock ottimistico.
- **ID stabili** (UUID v7) ovunque: lo stesso contenuto ha lo stesso id in ogni ambiente.
- **Promozione dei contenuti** con chiusura automatica delle dipendenze (M2O, O2M, M2M, M2A,
  file, cartelle), natural key, strategie di conflitto, prune, dry-run con report campo per campo.
- API REST compatibile nei concetti con Directus (`/items`, `fields`, `filter`, `{ data }`).
- **Flow** nello snapshot (promossi tra ambienti come il resto): webhook in uscita, hook bloccanti che validano o riscrivono le scritture, job pianificati, endpoint personalizzati, azioni manuali; **estensioni** per l'Admin UI (pagine, widget di campo, azioni).
- **Realtime** via WebSocket (`/websocket`): sottoscrizioni a una collection con campi e filtri, consegna con i permessi di ogni client, anche tra più istanze.
- **Storage dei file** su disco, **S3 e compatibili** (MinIO, R2, B2, Wasabi…) o **Azure Blob**, anche in più location insieme; download con URL firmati opzionali; `ferro storage migrate` sposta i file tra location (`FERRO_STORAGE_*`, vedi docs).
- **GraphQL** sugli stessi permessi (`POST /graphql`, schema SDL su `/graphql/schema`): relazioni tipate, blocchi M2A come union (`... on block_text`), mutation transazionali.

Documento di progetto completo: **[docs/ARCHITETTURA.md](docs/ARCHITETTURA.md)**.

## Struttura

```
crates/
  ferro-core     modello, validazione, diff/plan, filtri, bundle, report (nessun I/O DB)
  ferro-engine   PostgreSQL: DDL, query, items, permessi, auth, file, sync
  ferro-storage  driver dello storage: disco, S3 e compatibili, Azure Blob
  ferro-server   API HTTP (axum)
  ferro-cli      binario `ferro`: server + strumenti di ambiente
admin/           Admin UI (JavaScript puro, nessuna build), servita su /admin
examples/site    progetto di esempio (ferro.toml + schema/)
deploy/          docker-compose, Dockerfile, Jenkinsfile di esempio
docs/            progetto
```

## Avvio rapido

Requisiti: Rust ≥ 1.80, PostgreSQL ≥ 13 (oppure Docker).

```bash
# database + server
docker compose -f deploy/docker-compose.yml up -d postgres
cp .env.example .env            # e imposta FERRO_SECRET
export $(grep -v '^#' .env | xargs)
cargo run -p ferro-cli -- serve

# Admin UI: http://localhost:8055/admin (FERRO_ADMIN_DIR=./admin, accesso con FERRO_ADMIN_EMAIL / FERRO_ADMIN_PASSWORD)

# in un altro terminale: token per la CLI
cargo run -p ferro-cli -- user token --email admin@example.com
export FERRO_LOCAL_TOKEN=ferro_...

# applica il modello di esempio
cd examples/site
cargo run -p ferro-cli -- schema diff  --to local
cargo run -p ferro-cli -- schema apply --to local

# contenuti
curl -H "Authorization: Bearer $FERRO_LOCAL_TOKEN" -H 'content-type: application/json' \
  -d '{"title":"Home","slug":"home","blocks":[{"collection":"block_hero","item":{"headline":"Ciao"},"sort":1}]}' \
  http://localhost:8055/items/pages
curl -H "Authorization: Bearer $FERRO_LOCAL_TOKEN" \
  'http://localhost:8055/items/pages?fields=*,blocks.item.*&filter={"slug":{"_eq":"home"}}'
```

## Flussi tipici tra ambienti

```bash
ferro env status                                        # hash dello schema di ogni ambiente
ferro schema pull  --from dev                           # dev -> schema/ (commit in git)
ferro schema diff  --to prod --report schema-diff.html  # cosa cambierebbe in prod
ferro schema apply --to prod                            # chiede conferma (ambiente protetto)

ferro data diff --from dev --to prod --select 'pages:{"slug":{"_eq":"home"}}' --report diff.html
ferro data push --from dev --to prod --select 'pages:{"slug":{"_eq":"home"}}' --strategy safe --prune
```

## Test

```bash
scripts/ci.sh [browser]   # tutto: build, test Rust, Admin UI, integrazione su PostgreSQL (e browser) — vedi tests/README.md
cargo test            # unit test di core/engine/server (non serve un database)
(cd admin && node --test test/*.test.mjs)   # test dell'Admin UI (Node >= 20)
```

> Stato: compila senza warning, 64 test Rust + 30 test dell'Admin UI + 312 controlli di integrazione + 36 nel browser, provato end-to-end su PostgreSQL 16
> con due ambienti e nel browser (vedi "Limiti noti" in docs/ARCHITETTURA.md). I test di integrazione su
> database non sono ancora in CI.
