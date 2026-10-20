#!/usr/bin/env python3
"""
FastGrid M1 SQLite Test Database Generator

Generates a realistic SQLite database designed for M1 acceptance testing:
- Schema browser verification (Tables & Views)
- TablePlus-style layout and high-performance Data Grid testing
- Multi-datatype formatting and edge cases (NULLs, JSON pills, BLOB pills, Unicode, multiline)
- Foreign key relations and ER jumping
- 1,000,000 rows table for virtual scrolling, streaming, and background COUNT(*) testing

Usage:
    python3 generate.py                     # Full 1,000,000 rows (default)
    python3 generate.py --quick             # Quick mode (10,000 rows)
    python3 generate.py --rows 500000       # Custom row count
    python3 generate.py --output /path/db   # Custom output path
"""

import argparse
import os
import sqlite3
import sys
import time
from pathlib import Path

DEFAULT_OUTPUT = Path(__file__).resolve().parent / "m1_test.db"
SCHEMA_SQL = Path(__file__).resolve().parent / "schema.sql"
SEED_SQL = Path(__file__).resolve().parent / "seed_sample.sql"


def format_bytes(size: int) -> str:
    """Format bytes into human-readable string."""
    for unit in ["B", "KB", "MB", "GB"]:
        if size < 1024.0:
            return f"{size:.2f} {unit}"
        size /= 1024.0
    return f"{size:.2f} TB"


def generate_database(output_path: Path, rows: int, skip_large: bool, quiet: bool) -> None:
    t_start = time.time()
    output_path = output_path.resolve()
    output_path.parent.mkdir(parents=True, exist_ok=True)

    if not quiet:
        print("============================================================")
        print(" FastGrid M1 SQLite Test Database Generator")
        print("============================================================")
        print(f"Target file:        {output_path}")
        print(f"Events table rows:  {rows if not skip_large else 0:,}")
        print("------------------------------------------------------------")

    # Remove existing database and WAL files
    for ext in ["", "-wal", "-shm", "-journal"]:
        p = Path(str(output_path) + ext)
        if p.exists():
            p.unlink()

    conn = sqlite3.connect(str(output_path))
    cursor = conn.cursor()

    # Fast generation settings
    cursor.execute("PRAGMA synchronous = OFF;")
    cursor.execute("PRAGMA journal_mode = MEMORY;")
    cursor.execute("PRAGMA foreign_keys = ON;")

    # 1. Execute schema DDL
    if not quiet:
        print("-> Applying schema DDL (schema.sql)...", end="", flush=True)
    t0 = time.time()
    with open(SCHEMA_SQL, "r", encoding="utf-8") as f:
        cursor.executescript(f.read())
    if not quiet:
        print(f" Done ({time.time() - t0:.2f}s)")

    # 2. Execute sample relational seed data
    if not quiet:
        print("-> Seeding sample relational data (seed_sample.sql)...", end="", flush=True)
    t0 = time.time()
    with open(SEED_SQL, "r", encoding="utf-8") as f:
        cursor.executescript(f.read())
    if not quiet:
        print(f" Done ({time.time() - t0:.2f}s)")

    # 3. Generate large_events rows
    if not skip_large and rows > 0:
        if not quiet:
            print(f"-> Generating {rows:,} rows for large_events table...", end="", flush=True)
        t0 = time.time()

        # Generate in chunks of up to 100,000 for memory safety and fast CTE execution
        chunk_size = 100000
        inserted = 0
        while inserted < rows:
            batch = min(chunk_size, rows - inserted)
            offset = inserted
            cursor.execute(f"""
            WITH RECURSIVE cnt(x) AS (
                SELECT 1 UNION ALL SELECT x + 1 FROM cnt LIMIT {batch}
            )
            INSERT INTO large_events (id, event_uuid, event_type, user_id, severity, ip_address, user_agent, payload, duration_ms, created_at)
            SELECT 
                {offset} + x,
                printf('e%07d-0000-4000-8000-%012d', {offset} + x, {offset} + x),
                CASE (({offset} + x) % 6)
                    WHEN 0 THEN 'page_view'
                    WHEN 1 THEN 'click_button'
                    WHEN 2 THEN 'search_query'
                    WHEN 3 THEN 'add_to_cart'
                    WHEN 4 THEN 'checkout_complete'
                    ELSE 'api_heartbeat'
                END,
                (({offset} + x) % 10) + 1,
                CASE (({offset} + x) % 20)
                    WHEN 0 THEN 'ERROR'
                    WHEN 1 THEN 'WARN'
                    WHEN 2 THEN 'WARN'
                    WHEN 3 THEN 'DEBUG'
                    ELSE 'INFO'
                END,
                printf('192.168.%d.%d', (({offset} + x) / 256) % 255 + 1, ({offset} + x) % 254 + 1),
                'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 FastGrid/0.1.0',
                printf('{{"event_id":%d,"session_id":"sess_%d","latency":%.2f}}', {offset} + x, ({offset} + x) % 1000, (({offset} + x) % 500) * 0.1),
                ROUND(((({offset} + x) % 1500) * 0.35 + 5.0), 2),
                datetime('2026-01-01 00:00:00', '+' || (({offset} + x) % 25000000) || ' seconds')
            FROM cnt;
            """)
            inserted += batch

        conn.commit()
        if not quiet:
            print(f" Done ({time.time() - t0:.2f}s)")

    # 4. Finalize database mode to WAL for concurrent reader performance
    cursor.execute("PRAGMA journal_mode = WAL;")
    cursor.execute("PRAGMA synchronous = NORMAL;")
    conn.commit()

    # 5. Query and display row counts
    cursor.execute("SELECT name, type FROM sqlite_master WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' ORDER BY type, name;")
    objects = cursor.fetchall()

    if not quiet:
        print("------------------------------------------------------------")
        print(f"{'Object Name':<25} {'Type':<8} {'Row Count':>12}")
        print("------------------------------------------------------------")
        for name, obj_type in objects:
            try:
                cursor.execute(f"SELECT COUNT(*) FROM \"{name}\";")
                count = cursor.fetchone()[0]
                count_str = f"{count:,}"
            except Exception:
                count_str = "N/A"
            print(f"{name:<25} {obj_type:<8} {count_str:>12}")

        total_time = time.time() - t_start
        file_size = output_path.stat().st_size
        print("============================================================")
        print(f"Generated successfully in {total_time:.2f} seconds!")
        print(f"File size:  {format_bytes(file_size)} ({file_size:,} bytes)")
        print(f"Path:       {output_path}")
        print("============================================================")

    conn.close()


def main() -> None:
    parser = argparse.ArgumentParser(description="Generate FastGrid M1 SQLite Test Database")
    parser.add_argument(
        "--output", "-o",
        type=Path,
        default=DEFAULT_OUTPUT,
        help=f"Target SQLite file path (default: {DEFAULT_OUTPUT})"
    )
    group = parser.add_mutually_exclusive_group()
    group.add_argument(
        "--quick", "-q",
        action="store_true",
        help="Quick mode: generates 10,000 rows in large_events (~0.1s)"
    )
    group.add_argument(
        "--full", "-f",
        action="store_true",
        help="Full mode: generates 1,000,000 rows in large_events (default)"
    )
    group.add_argument(
        "--rows", "-r",
        type=int,
        default=1000000,
        help="Custom number of rows in large_events (default: 1,000,000)"
    )
    parser.add_argument(
        "--no-large",
        action="store_true",
        help="Skip generating the large_events table (relational tables only)"
    )
    parser.add_argument(
        "--quiet",
        action="store_true",
        help="Quiet mode: minimal output"
    )

    args = parser.parse_args()

    if args.quick:
        rows = 10000
    elif args.full:
        rows = 1000000
    else:
        rows = args.rows

    generate_database(args.output, rows, args.no_large, args.quiet)


if __name__ == "__main__":
    main()
