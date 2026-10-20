# FastGrid M1 SQLite Test Database

This test database is designed specifically for **M1 acceptance verification**:
- **Schema Browser**: Grouping of tables and views, count estimation, Quick Open (`⌘P`).
- **Data Grid Layout**: Proportional text vs monospace numbers/dates, type badges, foreign key badges.
- **Cell Pill Formatting**: Small inline JSON vs `[JSON n keys]` pills, `[BLOB n KB]` summary pills.
- **Null Safety**: Distinct italic `NULL` pill vs literal string `"NULL"`, empty strings `""`.
- **Foreign Key Navigation**: Forward jumping from FK cell to the target row in referenced table.
- **Performance Benchmark**: 1,000,000 rows table for testing <200ms initial screen and 60fps virtual scrolling.

---

## Generator Usage

```bash
# Full generation: 1,000,000 rows in large_events (default, takes ~2s)
./generate.sh

# Quick mode: 10,000 rows in large_events (takes ~0.05s)
./generate.sh --quick

# Custom row count
./generate.sh --rows 100000

# Skip large table entirely (relational + showcase tables only)
./generate.sh --no-large

# Custom destination path
./generate.sh --output /path/to/database.db
```

The script can also be executed directly via Python:
```bash
python3 generate.py --quick
```

---

## Schema & Tables Summary

### 1. Relational Tables

| Table | Description | Key Relationships |
|---|---|---|
| `categories` | Product category hierarchy with self-referential parent-child relationships | `parent_id -> categories(id)` |
| `users` | Customer, Admin, VIP and Moderator user accounts with JSON metadata | Referenced by `orders`, `product_reviews` |
| `products` | Product catalog with DECIMAL prices, ratings, stock counts, and JSON tags | `category_id -> categories(id)` |
| `orders` | Customer orders with statuses, multiline shipping notes, and JSON addresses | `customer_id -> users(id)` |
| `order_items` | Order line items with composite uniqueness and cascade deletion | `order_id -> orders(id)`, `product_id -> products(id)` |
| `product_reviews` | User reviews and ratings (1-5 stars) with cascade deletion | `product_id -> products(id)`, `user_id -> users(id)` |

### 2. Edge Case Showcase Table: `all_types_showcase`

This table tests every cell formatter and boundary case in FastGrid:

- `col_bool_true`, `col_bool_false`, `col_bool_null`: Boolean rendering
- `col_int_small`, `col_int_large`, `col_int_negative`: Integer bounds (including `i64::MAX`)
- `col_float_standard`, `col_float_pi`: Floating point values
- `col_decimal`: High-precision decimals
- `col_text_unicode`: Multi-language text (Chinese, Japanese, Korean, Arabic, and emojis 🚀🦀🌸⚡️)
- `col_text_multiline`: Multiline string with embedded newlines and tabs
- `col_text_empty`: Empty string `""` (should render blank, distinct from NULL)
- `col_text_literal_null`: String `"NULL"` (must not be confused with SQL NULL)
- `col_json_small`: Small inline JSON object
- `col_json_large`: Deeply nested JSON object (>120 chars, renders as `[JSON n keys]` pill)
- `col_blob_small`: 16-byte binary payload
- `col_blob_large`: 64KB binary payload (renders as `[BLOB 64 KB]` pill)
- `col_uuid`, `col_date`, `col_time`, `col_datetime`, `col_timestamp`: Monospaced date/time formats
- `col_always_null`: All NULL column to test column-wide null presentation

### 3. High-Volume Performance Table: `large_events`

Fulfills the **M1 Acceptance Criteria**:
> "打开一个 100 万行的 SQLite 表：首屏出数据 < 200ms；滚动 60fps；内存随结果集线性增长且有上限"

- **Row count**: 1,000,000 rows
- **Columns**: `id`, `event_uuid`, `event_type`, `user_id`, `severity`, `ip_address`, `user_agent`, `payload`, `duration_ms`, `created_at`
- **Indexes**: `idx_events_type_created`, `idx_events_user_id`

### 4. Views

- `v_active_users`: Filtered user view (`is_active = 1`)
- `v_order_summary`: Multi-table aggregation joining `orders`, `users`, and `order_items`
- `v_product_stats`: Product review statistics joining `products`, `categories`, and `product_reviews`

---

## Verifying M1 Features in FastGrid

1. **Schema Browser**:
   - Check left sidebar: Verify tables appear under the **Tables** group and views under the **Views** group.
   - Quick Open: Press `⌘P`, type `review` or `event`, press `Enter` to jump.
2. **Data Grid**:
   - Open `large_events`: Observe initial load latency (<200ms) and smooth 60fps scroll.
   - Change page size in status bar: 100 / 300 / 1000.
   - Open `all_types_showcase`: Verify `[JSON ...]` and `[BLOB ...]` pills, verify `NULL` vs `"NULL"`.
3. **Filtering & Sorting**:
   - Press `⌘F` to open the filter drawer. Add a filter rule: `severity = 'ERROR'`, or `price > 500`.
   - Click column headers to sort ASC, DESC, or reset.
4. **Foreign Key Navigation**:
   - Open `orders`: Click on the `customer_id` cell to navigate directly to the matching user in `users`.
