# Ferro Admin

Single-page app in plain JavaScript (ES modules, no dependencies, no build step).

- Served by the Ferro server on `/admin` when `FERRO_ADMIN_DIR` points to this folder
  (the Docker image does it already).
- `http://localhost:8055/admin/?api=https://other-host` talks to another server (needs CORS there).
- Tests: `node --test test/*.test.mjs` (Node >= 20).

Layout: `js/api.js` (HTTP + auth), `js/schema.js` (model helpers), `js/form.js` (form generated from the model,
including inline O2M/M2M and M2A blocks), `js/views/*` (one file per area), `js/main.js` (shell and routing).
