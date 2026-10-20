-- FastGrid M1 SQLite Test Database Schema
-- Designed for comprehensive testing of FastGrid M1:
-- 1. Schema Browser (Tables & Views grouping, count estimates)
-- 2. Data Grid (Type badges, monospaced numbers/dates, JSON & BLOB pills, NULL rendering)
-- 3. Foreign Key relations (list_relations, forward FK navigation)
-- 4. Sorting, Multi-column sorting, and Filtering
-- 5. 1,000,000 rows large dataset for virtual scroll and pagination benchmarking

PRAGMA foreign_keys = ON;

-- -----------------------------------------------------------------------------
-- 1. Lookup & Hierarchy: Categories
-- -----------------------------------------------------------------------------
DROP TABLE IF EXISTS product_reviews;
DROP TABLE IF EXISTS order_items;
DROP TABLE IF EXISTS orders;
DROP TABLE IF EXISTS products;
DROP TABLE IF EXISTS categories;
DROP TABLE IF EXISTS users;
DROP TABLE IF EXISTS all_types_showcase;
DROP TABLE IF EXISTS large_events;

DROP VIEW IF EXISTS v_order_summary;
DROP VIEW IF EXISTS v_product_stats;
DROP VIEW IF EXISTS v_active_users;

CREATE TABLE categories (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    slug VARCHAR(64) NOT NULL UNIQUE,
    name VARCHAR(100) NOT NULL,
    parent_id INTEGER REFERENCES categories(id) ON DELETE SET NULL,
    description TEXT,
    icon VARCHAR(32),
    display_order INTEGER NOT NULL DEFAULT 0,
    is_active BOOLEAN NOT NULL DEFAULT 1,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX idx_categories_parent ON categories(parent_id);
CREATE INDEX idx_categories_display ON categories(display_order);

-- -----------------------------------------------------------------------------
-- 2. User & Customer accounts
-- -----------------------------------------------------------------------------
CREATE TABLE users (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    uuid TEXT NOT NULL UNIQUE,
    username VARCHAR(50) NOT NULL UNIQUE,
    email VARCHAR(255) NOT NULL UNIQUE,
    full_name VARCHAR(100) NOT NULL,
    role VARCHAR(20) NOT NULL DEFAULT 'customer' CHECK(role IN ('customer', 'admin', 'moderator', 'vip')),
    avatar_url TEXT,
    is_active BOOLEAN NOT NULL DEFAULT 1,
    metadata_json JSON,
    last_login_at TIMESTAMP,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX idx_users_role ON users(role);
CREATE INDEX idx_users_active_created ON users(is_active, created_at);

-- -----------------------------------------------------------------------------
-- 3. Products Catalog
-- -----------------------------------------------------------------------------
CREATE TABLE products (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    sku VARCHAR(64) NOT NULL UNIQUE,
    name VARCHAR(255) NOT NULL,
    category_id INTEGER NOT NULL REFERENCES categories(id) ON DELETE RESTRICT,
    description TEXT,
    price DECIMAL(10, 2) NOT NULL,
    cost_price DECIMAL(10, 2),
    stock_quantity INTEGER NOT NULL DEFAULT 0,
    rating REAL NOT NULL DEFAULT 0.0,
    tags JSON,
    thumbnail BLOB,
    is_published BOOLEAN NOT NULL DEFAULT 1,
    published_at TIMESTAMP,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX idx_products_category_price ON products(category_id, price);
CREATE INDEX idx_products_rating ON products(rating);
CREATE INDEX idx_products_published ON products(is_published, published_at);

-- -----------------------------------------------------------------------------
-- 4. Customer Orders
-- -----------------------------------------------------------------------------
CREATE TABLE orders (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    order_no VARCHAR(64) NOT NULL UNIQUE,
    customer_id INTEGER NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    status VARCHAR(32) NOT NULL DEFAULT 'pending' CHECK(status IN ('pending', 'processing', 'paid', 'shipped', 'delivered', 'cancelled', 'refunded')),
    subtotal DECIMAL(10, 2) NOT NULL,
    discount DECIMAL(10, 2) NOT NULL DEFAULT 0.00,
    tax DECIMAL(10, 2) NOT NULL DEFAULT 0.00,
    total_amount DECIMAL(10, 2) NOT NULL,
    shipping_address JSON NOT NULL,
    payment_method VARCHAR(32) NOT NULL,
    notes TEXT,
    ordered_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    shipped_at TIMESTAMP,
    delivered_at TIMESTAMP
);

CREATE INDEX idx_orders_customer_date ON orders(customer_id, ordered_at);
CREATE INDEX idx_orders_status ON orders(status);

-- -----------------------------------------------------------------------------
-- 5. Order Line Items
-- -----------------------------------------------------------------------------
CREATE TABLE order_items (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    order_id INTEGER NOT NULL REFERENCES orders(id) ON DELETE CASCADE,
    product_id INTEGER NOT NULL REFERENCES products(id) ON DELETE RESTRICT,
    quantity INTEGER NOT NULL DEFAULT 1 CHECK(quantity > 0),
    unit_price DECIMAL(10, 2) NOT NULL,
    total_price DECIMAL(10, 2) NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE UNIQUE INDEX idx_order_items_unique_item ON order_items(order_id, product_id);
CREATE INDEX idx_order_items_product ON order_items(product_id);

-- -----------------------------------------------------------------------------
-- 6. Product Reviews & Ratings
-- -----------------------------------------------------------------------------
CREATE TABLE product_reviews (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    product_id INTEGER NOT NULL REFERENCES products(id) ON DELETE CASCADE,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    rating INTEGER NOT NULL CHECK(rating BETWEEN 1 AND 5),
    title VARCHAR(255),
    content TEXT,
    is_verified_purchase BOOLEAN NOT NULL DEFAULT 1,
    helpful_votes INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX idx_reviews_product_rating ON product_reviews(product_id, rating);
CREATE INDEX idx_reviews_user ON product_reviews(user_id);

-- -----------------------------------------------------------------------------
-- 7. All Data Types & Edge Case Showcase Table
-- Tests FastGrid formatters, alignment, pills, NULL differentiation, and Unicode
-- -----------------------------------------------------------------------------
CREATE TABLE all_types_showcase (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    col_bool_true BOOLEAN,
    col_bool_false BOOLEAN,
    col_bool_null BOOLEAN,
    col_int_small INTEGER,
    col_int_large INTEGER,
    col_int_negative INTEGER,
    col_float_standard REAL,
    col_float_pi REAL,
    col_decimal DECIMAL(12, 4),
    col_text_simple TEXT,
    col_text_unicode TEXT,
    col_text_multiline TEXT,
    col_text_empty TEXT,
    col_text_literal_null TEXT,
    col_json_small JSON,
    col_json_large JSON,
    col_blob_small BLOB,
    col_blob_large BLOB,
    col_uuid UUID,
    col_date DATE,
    col_time TIME,
    col_datetime DATETIME,
    col_timestamp TIMESTAMP,
    col_always_null TEXT
);

-- -----------------------------------------------------------------------------
-- 8. High-Volume Events Table (1,000,000 rows for M1 performance benchmark)
-- -----------------------------------------------------------------------------
CREATE TABLE large_events (
    id INTEGER PRIMARY KEY,
    event_uuid TEXT NOT NULL,
    event_type VARCHAR(64) NOT NULL,
    user_id INTEGER,
    severity VARCHAR(16) NOT NULL,
    ip_address VARCHAR(45) NOT NULL,
    user_agent TEXT,
    payload JSON,
    duration_ms REAL NOT NULL,
    created_at TIMESTAMP NOT NULL
);

CREATE INDEX idx_events_type_created ON large_events(event_type, created_at);
CREATE INDEX idx_events_user_id ON large_events(user_id);

-- -----------------------------------------------------------------------------
-- 9. Views for testing Schema Browser Object Tree grouping
-- -----------------------------------------------------------------------------
CREATE VIEW v_active_users AS
SELECT 
    id,
    uuid,
    username,
    email,
    full_name,
    role,
    created_at
FROM users
WHERE is_active = 1;

CREATE VIEW v_order_summary AS
SELECT 
    o.id AS order_id,
    o.order_no,
    u.username AS customer_name,
    u.email AS customer_email,
    o.status,
    COUNT(oi.id) AS item_count,
    COALESCE(SUM(oi.quantity), 0) AS total_quantity,
    o.total_amount,
    o.ordered_at
FROM orders o
JOIN users u ON o.customer_id = u.id
LEFT JOIN order_items oi ON o.id = oi.order_id
GROUP BY o.id, o.order_no, u.username, u.email, o.status, o.total_amount, o.ordered_at;

CREATE VIEW v_product_stats AS
SELECT 
    p.id AS product_id,
    p.sku,
    p.name AS product_name,
    c.name AS category_name,
    p.price,
    p.stock_quantity,
    COUNT(pr.id) AS review_count,
    COALESCE(ROUND(AVG(pr.rating), 2), 0.0) AS avg_rating
FROM products p
JOIN categories c ON p.category_id = c.id
LEFT JOIN product_reviews pr ON p.id = pr.product_id
GROUP BY p.id, p.sku, p.name, c.name, p.price, p.stock_quantity;

-- -----------------------------------------------------------------------------
-- 10. Triggers
-- -----------------------------------------------------------------------------
CREATE TRIGGER trg_users_updated_at 
AFTER UPDATE ON users
FOR EACH ROW
BEGIN
    UPDATE users SET updated_at = CURRENT_TIMESTAMP WHERE id = OLD.id;
END;
