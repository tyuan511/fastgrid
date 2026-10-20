-- FastGrid M5 MySQL / MariaDB Test Database Schema
-- Targeted for M5 validation:
-- - MySQL dialects and quoting (backticks)
-- - utf8mb4 collation and multi-byte unicode
-- - Unsigned integers, ENUM, SET, JSON, and DATETIME types
-- - SHOW / INFORMATION_SCHEMA metadata queries

CREATE DATABASE IF NOT EXISTS fastgrid_test CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;
USE fastgrid_test;

-- -----------------------------------------------------------------------------
-- Tables
-- -----------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS categories (
    id INT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
    slug VARCHAR(64) NOT NULL UNIQUE,
    name VARCHAR(100) NOT NULL,
    parent_id INT UNSIGNED DEFAULT NULL,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT fk_mysql_cat_parent FOREIGN KEY (parent_id) REFERENCES categories(id) ON DELETE SET NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS users (
    id INT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
    uuid CHAR(36) NOT NULL UNIQUE,
    username VARCHAR(50) NOT NULL UNIQUE,
    email VARCHAR(255) NOT NULL UNIQUE,
    role ENUM('customer', 'admin', 'moderator', 'vip') NOT NULL DEFAULT 'customer',
    is_active TINYINT(1) NOT NULL DEFAULT 1,
    preferences JSON,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS products (
    id INT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
    sku VARCHAR(64) NOT NULL UNIQUE,
    name VARCHAR(255) NOT NULL,
    category_id INT UNSIGNED NOT NULL,
    price DECIMAL(10, 2) NOT NULL,
    stock_quantity INT UNSIGNED NOT NULL DEFAULT 0,
    rating FLOAT NOT NULL DEFAULT 0.0,
    tags JSON,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT fk_mysql_prod_cat FOREIGN KEY (category_id) REFERENCES categories(id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS orders (
    id INT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
    order_no VARCHAR(64) NOT NULL UNIQUE,
    customer_id INT UNSIGNED NOT NULL,
    status ENUM('pending', 'processing', 'paid', 'shipped', 'delivered', 'cancelled') NOT NULL DEFAULT 'pending',
    total_amount DECIMAL(10, 2) NOT NULL,
    ordered_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT fk_mysql_ord_cust FOREIGN KEY (customer_id) REFERENCES users(id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

-- -----------------------------------------------------------------------------
-- Seed Data
-- -----------------------------------------------------------------------------
INSERT INTO categories (id, slug, name) VALUES
(1, 'computers', 'Computers & Laptops'),
(2, 'accessories', 'Accessories');

INSERT INTO users (id, uuid, username, email, role, preferences) VALUES
(1, '3d8a7c20-6d4b-4f9e-a0e2-8b7a9c3d4e5f', 'mysql_admin', 'admin@mysql.fastgrid.dev', 'admin', '{"theme": "dark"}'),
(2, '4e9b8d31-7e5c-50af-b1f3-9c8b0d4e5f60', 'mysql_user', 'user@mysql.fastgrid.dev', 'customer', '{"theme": "light"}');

INSERT INTO products (id, sku, name, category_id, price, stock_quantity, rating, tags) VALUES
(1, 'MYSQL-LAPTOP', 'MySQL Test Laptop', 1, 1999.00, 25, 4.8, '["mysql", "dev"]'),
(2, 'MYSQL-MOUSE', 'MySQL Wireless Mouse', 2, 49.99, 150, 4.6, '["mouse", "wireless"]');

INSERT INTO orders (id, order_no, customer_id, status, total_amount) VALUES
(1, 'MY-ORD-0001', 2, 'delivered', 1999.00);
