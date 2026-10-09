#!/usr/bin/env bash
# Everything that must pass before a change is merged. Needs: Rust, Node >= 20, PostgreSQL client tools (psql)
# and a PostgreSQL server reachable at E2E_PG (default postgres://ferro@localhost:55432) whose user can create databases.
#
#   scripts/ci.sh            unit tests + integration suite
#   scripts/ci.sh browser    also the real-browser suite (needs a browser: BROWSER_PATH, or `npx playwright-core install chromium`)
set -euo pipefail
cd "$(dirname "$0")/.."

echo "== build"
cargo build --workspace --all-targets
echo "== Rust tests"
cargo test --workspace
echo "== clippy (errors only)"
cargo clippy --workspace --all-targets -- -A clippy::all -D clippy::correctness
echo "== Admin UI unit tests"
(cd admin && node --test test/*.test.mjs)
echo "== integration suite (two environments, real PostgreSQL; mock SMTP, OpenID provider and LDAP server run in Node)"
(cd tests/e2e && npm install --no-audit --no-fund)
node tests/e2e/run.mjs
if [ "${1:-}" = "browser" ]; then
  echo "== browser suite"
  (cd tests/browser && npm install --no-audit --no-fund && node run.mjs && node auth.mjs)
fi
echo "== all green"
