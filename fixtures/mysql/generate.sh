#!/usr/bin/env bash
# FastGrid M5 MySQL Test Fixture Setup Script
#
# Starts MySQL via docker-compose and verifies connection and seed data.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "${SCRIPT_DIR}"

echo "============================================================"
echo " FastGrid M5 MySQL Test Fixture"
echo "============================================================"

if ! command -v docker >/dev/null 2>&1; then
    echo "Error: docker is required to spin up the MySQL fixture." >&2
    exit 1
fi

echo "-> Starting MySQL container..."
docker compose up -d

echo "-> Waiting for MySQL to be ready..."
until docker compose exec mysql mysqladmin ping -h localhost -u root -prootpassword >/dev/null 2>&1; do
    sleep 2
done

echo "============================================================"
echo "MySQL is ready!"
echo "Host:     127.0.0.1"
echo "Port:     3306"
echo "User:     fastgrid"
echo "Database: fastgrid_test"
echo "Password: testpassword"
echo "============================================================"
