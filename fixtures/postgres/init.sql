-- FastGrid M4 PostgreSQL Test Database Schema
-- Targeted for M4 validation:
-- - Schemas / Namespaces (public vs private schemas)
-- - Native types: JSONB, UUID, ENUMs, ARRAY, TIMESTAMPTZ, NUMERIC
-- - Column OIDs and system catalog metadata
-- - Foreign key relationships across schemas

CREATE EXTENSION IF NOT EXISTS "uuid-ossp";

-- -----------------------------------------------------------------------------
-- Schemas
-- -----------------------------------------------------------------------------
CREATE SCHEMA IF NOT EXISTS commerce;
CREATE SCHEMA IF NOT EXISTS telemetry;

-- -----------------------------------------------------------------------------
-- Enums
-- -----------------------------------------------------------------------------
CREATE TYPE commerce.order_status AS ENUM (
    'pending', 'processing', 'paid', 'shipped', 'delivered', 'cancelled', 'refunded'
);

CREATE TYPE telemetry.log_level AS ENUM (
    'DEBUG', 'INFO', 'WARN', 'ERROR', 'FATAL'
);

-- -----------------------------------------------------------------------------
-- Commerce Schema
-- -----------------------------------------------------------------------------
CREATE TABLE commerce.categories (
    id SERIAL PRIMARY KEY,
    slug VARCHAR(64) NOT NULL UNIQUE,
    name VARCHAR(100) NOT NULL,
    parent_id INTEGER REFERENCES commerce.categories(id) ON DELETE SET NULL,
    description TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE commerce.users (
    id SERIAL PRIMARY KEY,
    uuid UUID NOT NULL DEFAULT uuid_generate_v4() UNIQUE,
    username VARCHAR(50) NOT NULL UNIQUE,
    email VARCHAR(255) NOT NULL UNIQUE,
    full_name VARCHAR(100) NOT NULL,
    is_active BOOLEAN NOT NULL DEFAULT TRUE,
    roles TEXT[] NOT NULL DEFAULT '{"customer"}',
    preferences JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE commerce.products (
    id SERIAL PRIMARY KEY,
    sku VARCHAR(64) NOT NULL UNIQUE,
    name VARCHAR(255) NOT NULL,
    category_id INTEGER NOT NULL REFERENCES commerce.categories(id),
    price NUMERIC(10, 2) NOT NULL,
    stock_quantity INTEGER NOT NULL DEFAULT 0,
    rating REAL DEFAULT 0.0,
    tags TEXT[],
    attributes JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE commerce.orders (
    id SERIAL PRIMARY KEY,
    order_no VARCHAR(64) NOT NULL UNIQUE,
    customer_id INTEGER NOT NULL REFERENCES commerce.users(id),
    status commerce.order_status NOT NULL DEFAULT 'pending',
    total_amount NUMERIC(10, 2) NOT NULL,
    shipping_address JSONB NOT NULL,
    ordered_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE commerce.order_items (
    id SERIAL PRIMARY KEY,
    order_id INTEGER NOT NULL REFERENCES commerce.orders(id) ON DELETE CASCADE,
    product_id INTEGER NOT NULL REFERENCES commerce.products(id),
    quantity INTEGER NOT NULL DEFAULT 1 CHECK(quantity > 0),
    unit_price NUMERIC(10, 2) NOT NULL
);

-- -----------------------------------------------------------------------------
-- Telemetry Schema (High-Volume)
-- -----------------------------------------------------------------------------
CREATE TABLE telemetry.events (
    id BIGSERIAL PRIMARY KEY,
    event_uuid UUID NOT NULL DEFAULT uuid_generate_v4(),
    event_type VARCHAR(64) NOT NULL,
    user_id INTEGER,
    level telemetry.log_level NOT NULL DEFAULT 'INFO',
    payload JSONB,
    duration_ms DOUBLE PRECISION NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX idx_pg_events_type_created ON telemetry.events(event_type, created_at);

-- -----------------------------------------------------------------------------
-- Seed Data
-- -----------------------------------------------------------------------------
INSERT INTO commerce.categories (id, slug, name) VALUES
(1, 'tech', 'Technology'),
(2, 'audio', 'Audio & Sound');

INSERT INTO commerce.users (id, username, email, full_name, roles, preferences) VALUES
(1, 'alice', 'alice@fastgrid.dev', 'Alice PG', '{"admin", "customer"}', '{"theme": "dark"}'),
(2, 'bob', 'bob@fastgrid.dev', 'Bob PG', '{"customer"}', '{"theme": "light"}');

INSERT INTO commerce.products (id, sku, name, category_id, price, stock_quantity, rating, tags, attributes) VALUES
(1, 'LAPTOP-PRO', 'Pro Workstation Laptop', 1, 2499.00, 50, 4.9, '{"workstation", "portable"}', '{"ram": "64GB", "ssd": "2TB"}'),
(2, 'HEADSET-PRO', 'Studio Wireless Headset', 2, 299.99, 100, 4.8, '{"audio", "wireless"}', '{"battery_hours": 30}');

INSERT INTO commerce.orders (id, order_no, customer_id, status, total_amount, shipping_address) VALUES
(1, 'PG-ORD-0001', 1, 'delivered', 2499.00, '{"city": "San Francisco", "zip": "94105"}');

INSERT INTO commerce.order_items (order_id, product_id, quantity, unit_price) VALUES
(1, 1, 1, 2499.00);

-- Generate sample telemetry events using generate_series
INSERT INTO telemetry.events (event_type, user_id, level, payload, duration_ms, created_at)
SELECT 
    CASE (g % 3) WHEN 0 THEN 'http_request' WHEN 1 THEN 'db_query' ELSE 'cache_fetch' END,
    (g % 2) + 1,
    'INFO'::telemetry.log_level,
    jsonb_build_object('req_id', g, 'status', 200),
    (g % 100) * 0.5 + 1.0,
    CURRENT_TIMESTAMP - (g || ' minutes')::INTERVAL
FROM generate_series(1, 1000) AS g;
