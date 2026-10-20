# FastGrid Test Database Fixtures

This directory contains standardized test databases and generator scripts across all database engines supported by FastGrid:

- **SQLite** (`fixtures/sqlite/`): Primary engine for **M1** (Read-only data grid, object tree, layout, foreign keys, virtual scrolling, and 1,000,000-row streaming).
- **PostgreSQL** (`fixtures/postgres/`): Targeted for **M4** (Schemas/namespaces, OIDs, arrays, JSONB, native UUID, cancellation tokens).
- **MySQL / MariaDB** (`fixtures/mysql/`): Targeted for **M5** (Charsets, unsigned types, ENUMs, triggers, delimiter parsing).

---

## Unified Git & Storage Policy

1. **Scripts only**: All generator scripts (`generate.sh`, `generate.py`), schema definitions (`*.sql`), and container definitions (`docker-compose.yml`) are committed and tracked in Git.
2. **Databases are strictly ignored**: All generated database files (`*.db`, `*.sqlite*`, WAL/SHM files), database data volumes (`fixtures/**/data/`), dumps (`*.dump`, `*.sql.gz`), and runtime artifacts are ignored by `.gitignore`.
3. **No credentials**: Test fixtures never contain real credentials or production keys, adhering to `AGENTS.md` security rules.

---

## Directory Overview

```text
fixtures/
├── README.md                      # Unified guide and policy (this file)
├── sqlite/                        # M1 SQLite fixture
│   ├── README.md                  # Detailed M1 dataset guide & test scenarios
│   ├── generate.sh                # Main executable generator (bash wrapper)
│   ├── generate.py                # Standalone generator (cross-platform, zero dependencies)
│   ├── schema.sql                 # DDL (tables, views, indexes, foreign keys, triggers)
│   ├── seed_sample.sql            # Realistic e-commerce and edge-case showcase seed data
│   └── m1_test.db                 # [GITIGNORED] Generated SQLite database
├── postgres/                      # M4 PostgreSQL fixture
│   ├── README.md                  # PostgreSQL container & setup instructions
│   ├── generate.sh                # Container spin-up and seed script
│   ├── docker-compose.yml         # Local Docker container definition
│   ├── init.sql                   # PostgreSQL schema and seed data
│   └── data/                      # [GITIGNORED] PostgreSQL data directory
└── mysql/                         # M5 MySQL / MariaDB fixture
    ├── README.md                  # MySQL container & setup instructions
    ├── generate.sh                # Container spin-up and seed script
    ├── docker-compose.yml         # Local Docker container definition
    ├── init.sql                   # MySQL schema and seed data
    └── data/                      # [GITIGNORED] MySQL data directory
```

---

## Quick Start: Generating M1 SQLite Database

Run the generator from repository root:

```bash
# Generate full test database (1,000,000 rows, ~300MB, ~2 seconds)
./fixtures/sqlite/generate.sh

# Or generate quick test database for rapid dev (10,000 rows, ~2MB, ~0.05 seconds)
./fixtures/sqlite/generate.sh --quick

# Or specify a custom row count or output file:
./fixtures/sqlite/generate.sh --rows 50000 --output /tmp/custom.db
```

### Opening in FastGrid GUI

1. Launch FastGrid:
   ```bash
   cargo run -p fg-app
   ```
2. In the Launcher window, click **New Connection** -> select **SQLite**.
3. Choose the generated file: `fixtures/sqlite/m1_test.db`.
4. Open the connection to test data grid scrolling, filtering (`⌘F`), quick open (`⌘P`), foreign key navigation, and views.
