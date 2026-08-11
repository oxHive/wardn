#!/usr/bin/env bash
# Truncates every control-plane table in the local dev Postgres.
#
# The integration test suite doesn't clean up after itself between runs (see
# docs/superpowers/plans/2026-08-11-walking-skeleton.md's ledger for why —
# it's a known, deliberately-deferred gap, not an oversight), so rows
# accumulate in the persistent podman-compose Postgres container over time.
# Run this whenever that becomes annoying, or before `cargo test` if a
# previous run left the DB in a state migration 0002's CHECK constraint would
# reject on next connect.
set -euo pipefail

CONTAINER="${GATEWAY_PG_CONTAINER:-hivemind-gateway_postgres_1}"

podman exec "$CONTAINER" psql -U gateway -d gateway -c \
  "TRUNCATE role_permissions, roles, api_keys, database_mappings, org_members, workspaces, orgs, users CASCADE;"

echo "Dev Postgres ($CONTAINER) reset."
