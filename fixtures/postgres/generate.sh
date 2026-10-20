#!/usr/bin/env bash
# FastGrid M4 PostgreSQL Test Fixture Setup Script
#
# Starts PostgreSQL via docker-compose and verifies connection and seed data.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "${SCRIPT_DIR}"

echo "============================================================"
echo " FastGrid M4 PostgreSQL Test Fixture"
echo "============================================================"

if ! command -v docker >/dev/null 2>&1; then
    echo "Error: docker is required to spin up the PostgreSQL fixture." >&2
    exit 1
fi

echo "-> Starting PostgreSQL container..."
docker compose up -d

echo "-> Waiting for PostgreSQL to be ready..."
until docker compose exec postgres pg_isready -U fastgrid -d fastgrid_test >/dev/null 2>&1; do
    sleep 1
done

echo "============================================================"
echo "PostgreSQL is ready!"
echo "Host:     127.0.0.1"
echo "Port:     5432"
echo "User:     fastgrid"
echo "Database: fastgrid_test"
echo "Password: testpassword"
echo "============================================================"
