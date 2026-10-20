#!/usr/bin/env bash
# FastGrid M1 SQLite Test Database Generator Script
#
# Usage:
#   ./generate.sh              # Full 1,000,000 rows (default)
#   ./generate.sh --quick      # Quick mode: 10,000 rows
#   ./generate.sh --rows 50000 # Custom row count
#   ./generate.sh -o /tmp/x.db # Custom output path

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Run python generator using python3
if command -v python3 >/dev/null 2>&1; then
    exec python3 "${SCRIPT_DIR}/generate.py" "$@"
elif command -v python >/dev/null 2>&1; then
    exec python "${SCRIPT_DIR}/generate.py" "$@"
else
    echo "Error: python3 is required to run the generator." >&2
    exit 1
fi
