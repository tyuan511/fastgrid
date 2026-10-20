# FastGrid M4 PostgreSQL Test Fixture

Planned for **M4 (PostgreSQL Engine Integration)**.

## Overview
- Multi-schema support (`commerce`, `telemetry`, `public`)
- PostgreSQL-specific types (`UUID`, `JSONB`, `NUMERIC`, `ARRAY`, `ENUM`)
- High-volume events table with timestamp intervals

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
- **Port**: `5432`
- **Database**: `fastgrid_test`
- **User**: `fastgrid`
- **Password**: `testpassword`

*Note: Persistent database storage in `./data/` is strictly ignored by Git.*
