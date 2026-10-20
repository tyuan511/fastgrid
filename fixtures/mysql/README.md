# FastGrid M5 MySQL Test Fixture

Planned for **M5 (MySQL / MariaDB Engine Integration)**.

## Overview
- MySQL 8.0 / MariaDB compatibility
- `utf8mb4` character set and collations
- MySQL-specific features: `ENUM`, `TINYINT(1)`, `JSON`, backtick quoting, `AUTO_INCREMENT`

## Quick Start (Docker)

```bash
# Start container and apply init.sql
./generate.sh

# Or directly with Docker Compose
docker compose up -d

# Stop container
docker compose down
```

## Connection Details
- **Host**: `127.0.0.1`
- **Port**: `3306`
- **Database**: `fastgrid_test`
- **User**: `fastgrid`
- **Password**: `testpassword`

*Note: Persistent database storage in `./data/` is strictly ignored by Git.*
