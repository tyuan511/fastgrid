-- FastGrid M1 SQLite Seed Data for Relational & Showcase Tables

BEGIN TRANSACTION;

-- -----------------------------------------------------------------------------
-- 1. Categories
-- -----------------------------------------------------------------------------
INSERT INTO categories (id, slug, name, parent_id, description, icon, display_order, is_active) VALUES
(1, 'electronics', 'Electronics', NULL, 'Consumer electronic devices and gadgets', 'cpu', 1, 1),
(2, 'computers', 'Computers & Laptops', 1, 'Desktops, laptops, and workstations', 'monitor', 2, 1),
(3, 'smartphones', 'Smartphones & Accessories', 1, 'Mobile devices, cases, and chargers', 'smartphone', 3, 1),
(4, 'audio', 'Audio & Headphones', 1, 'Headphones, speakers, and amplifiers', 'headphones', 4, 1),
(5, 'home-appliances', 'Home & Kitchen', NULL, 'Appliances and tools for modern living', 'home', 5, 1),
(6, 'coffee-makers', 'Coffee & Espresso', 5, 'Espresso machines, grinders, and beans', 'coffee', 6, 1),
(7, 'books-stationery', 'Books & Stationery', NULL, 'Technical books, notebooks, and pens', 'book-open', 7, 1),
(8, 'programming-books', 'Software Engineering Books', 7, 'Rust, Systems Programming, Distributed Systems', 'terminal', 8, 1),
(9, 'office-furniture', 'Office Furniture', 5, 'Standing desks, ergonomic chairs', 'briefcase', 9, 1),
(10, 'gaming', 'Gaming & Consoles', 1, 'Consoles, controllers, and VR headsets', 'gamepad-2', 10, 1);

-- -----------------------------------------------------------------------------
-- 2. Users
-- -----------------------------------------------------------------------------
INSERT INTO users (id, uuid, username, email, full_name, role, avatar_url, is_active, metadata_json, last_login_at, created_at) VALUES
(1, 'a1000000-0000-0000-0000-000000000001', 'admin', 'admin@fastgrid.dev', 'System Administrator', 'admin', 'https://avatar.fastgrid.dev/admin.png', 1, '{"theme":"dark","notifications":{"email":true,"push":false},"locale":"en_US"}', '2026-10-20 18:30:00', '2026-01-01 00:00:00'),
(2, 'a1000000-0000-0000-0000-000000000002', 'yuantang', 'yuantang@fastgrid.dev', 'Yuan Tang', 'admin', 'https://avatar.fastgrid.dev/yuantang.png', 1, '{"theme":"system","editor":{"font_size":14,"ligatures":true},"keybindings":"default"}', '2026-10-20 20:15:00', '2026-01-02 10:00:00'),
(3, 'a1000000-0000-0000-0000-000000000003', 'alice', 'alice.chen@example.com', 'Alice Chen', 'vip', 'https://avatar.fastgrid.dev/alice.png', 1, '{"theme":"light","tier":"platinum","points":14250}', '2026-10-19 14:22:10', '2026-02-15 08:30:00'),
(4, 'a1000000-0000-0000-0000-000000000004', 'bob_builder', 'bob.miller@example.org', 'Robert Miller', 'customer', NULL, 1, '{"preferred_shipping":"express","newsletter":true}', '2026-10-18 11:05:00', '2026-03-01 12:00:00'),
(5, 'a1000000-0000-0000-0000-000000000005', 'charlie_k', 'charlie.kim@example.com', 'Charlie Kim', 'customer', 'https://avatar.fastgrid.dev/charlie.png', 1, '{"preferred_shipping":"standard"}', '2026-10-15 09:40:00', '2026-03-20 16:45:00'),
(6, 'a1000000-0000-0000-0000-000000000006', 'diana_prince', 'diana@example.com', 'Diana Prince', 'vip', NULL, 1, '{"theme":"dark","tier":"gold","points":8900}', '2026-10-20 16:00:00', '2026-04-10 09:15:00'),
(7, 'a1000000-0000-0000-0000-000000000007', 'eva_green', 'eva.green@sample.io', 'Eva Green', 'moderator', 'https://avatar.fastgrid.dev/eva.png', 1, '{"department":"support","shifts":["morning","afternoon"]}', '2026-10-20 08:00:00', '2026-05-01 11:20:00'),
(8, 'a1000000-0000-0000-0000-000000000008', 'frank_w', 'frank.wright@sample.io', 'Frank Wright', 'customer', NULL, 0, '{"deactivation_reason":"user_request"}', '2026-06-12 10:00:00', '2026-05-15 14:00:00'),
(9, 'a1000000-0000-0000-0000-000000000009', 'grace_hopper', 'grace.hopper@compiler.org', 'Grace Hopper', 'vip', 'https://avatar.fastgrid.dev/grace.png', 1, '{"badge":"pioneer","compiler":"A-0"}', '2026-10-19 22:15:00', '2026-06-01 07:00:00'),
(10, 'a1000000-0000-0000-0000-000000000010', 'henry_t', 'henry.t@example.com', 'Henry Thoreau', 'customer', NULL, 1, '{"locale":"en_CA"}', '2026-10-10 15:30:00', '2026-07-04 10:10:00');

-- -----------------------------------------------------------------------------
-- 3. Products
-- -----------------------------------------------------------------------------
INSERT INTO products (id, sku, name, category_id, description, price, cost_price, stock_quantity, rating, tags, thumbnail, is_published, published_at, created_at) VALUES
(1, 'LAPTOP-M3-MAX', 'MacBook Pro 16-inch M3 Max', 2, 'Apple M3 Max chip with 16-core CPU and 40-core GPU, 64GB Unified Memory, 1TB SSD storage.', 3499.00, 2800.00, 45, 4.95, '["apple", "laptop", "m3", "workstation"]', X'89504E470D0A1A0A0000000D49484452000000010000000108060000001F15C489', 1, '2026-01-10 09:00:00', '2026-01-05 10:00:00'),
(2, 'PHONE-IPHONE-16', 'iPhone 16 Pro 256GB Desert Titanium', 3, 'Grade 5 titanium design with Camera Control button and A18 Pro chip.', 1099.00, 750.00, 120, 4.80, '["apple", "smartphone", "ios", "5g"]', X'89504E470D0A1A0A0000000D49484452000000010000000108060000001F15C489', 1, '2026-02-01 08:00:00', '2026-01-20 12:00:00'),
(3, 'HEADPHONE-WH1000XM5', 'Sony WH-1000XM5 Wireless Headphones', 4, 'Industry-leading noise cancelling with two processors and 8 microphones.', 398.00, 240.00, 85, 4.75, '["sony", "audio", "anc", "bluetooth"]', NULL, 1, '2026-02-15 10:00:00', '2026-02-10 14:00:00'),
(4, 'DESK-STAND-ERGO', 'ErgoPro Electric Dual-Motor Standing Desk', 9, 'Solid walnut top 60x30 inch, 4 memory presets, weight capacity 300 lbs.', 649.50, 390.00, 30, 4.65, '["furniture", "ergonomic", "desk", "home-office"]', NULL, 1, '2026-03-01 11:00:00', '2026-02-25 09:00:00'),
(5, 'COFFEE-ESPRESSO-PRO', 'Breville Barista Touch Impress', 6, 'Automated touchscreen espresso machine with assisted tamping and microfoam milk.', 1499.95, 950.00, 18, 4.88, '["kitchen", "coffee", "espresso", "appliances"]', NULL, 1, '2026-03-15 13:00:00', '2026-03-10 15:30:00'),
(6, 'BOOK-RUST-SYSTEMS', 'Programming Rust: Fast, Safe Systems Development', 8, 'Comprehensive guide to Rust covering memory safety, concurrency, and async IO.', 59.99, 28.00, 250, 4.92, '["books", "rust", "programming", "oreilly"]', NULL, 1, '2026-01-15 10:00:00', '2026-01-10 11:00:00'),
(7, 'BOOK-DESIGN-DATA', 'Designing Data-Intensive Applications', 8, 'The definitive guide to distributed databases, consensus, and storage engines by Martin Kleppmann.', 49.50, 22.00, 310, 4.98, '["books", "distributed-systems", "databases", "architecture"]', NULL, 1, '2026-01-15 10:00:00', '2026-01-10 11:00:00'),
(8, 'MONITOR-DELL-32-4K', 'Dell UltraSharp 32 4K USB-C Hub Monitor (U3223QE)', 2, 'IPS Black panel, 2000:1 contrast ratio, 90W power delivery, daisy chaining.', 829.99, 580.00, 42, 4.70, '["dell", "monitor", "4k", "usb-c"]', NULL, 1, '2026-04-01 09:00:00', '2026-03-25 10:00:00'),
(9, 'KEYBOARD-CUSTOM-65', 'Keychron Q2 Pro QMK Wireless Custom Keyboard', 2, 'CNC aluminum body, double-gasket mount, hot-swappable red switches, south-facing RGB.', 210.00, 130.00, 65, 4.82, '["keyboard", "mechanical", "wireless", "custom"]', NULL, 1, '2026-04-10 14:00:00', '2026-04-05 16:00:00'),
(10, 'CONSOLE-PS5-PRO', 'PlayStation 5 Pro Digital Edition 2TB', 10, 'Advanced ray tracing, PlayStation Spectral Super Resolution (PSSR), 60fps fidelity.', 699.99, 580.00, 0, 4.55, '["sony", "gaming", "console", "ps5"]', NULL, 1, '2026-05-01 08:00:00', '2026-04-20 18:00:00');

-- -----------------------------------------------------------------------------
-- 4. Orders
-- -----------------------------------------------------------------------------
INSERT INTO orders (id, order_no, customer_id, status, subtotal, discount, tax, total_amount, shipping_address, payment_method, notes, ordered_at, shipped_at, delivered_at) VALUES
(1, 'ORD-2026-0001', 3, 'delivered', 3499.00, 100.00, 271.92, 3670.92, '{"name":"Alice Chen","street":"100 Market St Suite 300","city":"San Francisco","state":"CA","zip":"94105","country":"USA"}', 'credit_card', 'Leave at front desk with receptionist.', '2026-09-01 10:15:00', '2026-09-01 16:30:00', '2026-09-03 14:00:00'),
(2, 'ORD-2026-0002', 4, 'delivered', 398.00, 0.00, 31.84, 429.84, '{"name":"Robert Miller","street":"456 Elmwood Ave","city":"Seattle","state":"WA","zip":"98101","country":"USA"}', 'apple_pay', NULL, '2026-09-05 11:20:00', '2026-09-06 09:00:00', '2026-09-08 16:45:00'),
(3, 'ORD-2026-0003', 6, 'delivered', 1709.95, 50.00, 132.80, 1792.75, '{"name":"Diana Prince","street":"789 Broadway","city":"New York","state":"NY","zip":"10003","country":"USA"}', 'credit_card', 'Ring doorbell twice. Do not drop over the gate.', '2026-09-10 14:00:00', '2026-09-11 10:00:00', '2026-09-13 11:30:00'),
(4, 'ORD-2026-0004', 9, 'delivered', 109.49, 0.00, 8.76, 118.25, '{"name":"Grace Hopper","street":"1 Arlington St","city":"Boston","state":"MA","zip":"02116","country":"USA"}', 'paypal', 'Gift wrapping requested for programming book.', '2026-09-15 09:30:00', '2026-09-15 15:00:00', '2026-09-17 10:15:00'),
(5, 'ORD-2026-0005', 2, 'delivered', 859.99, 0.00, 68.80, 928.79, '{"name":"Yuan Tang","street":"1 Hacker Way","city":"Menlo Park","state":"CA","zip":"94025","country":"USA"}', 'credit_card', 'FastGrid dev hardware test order.', '2026-09-20 16:45:00', '2026-09-21 11:00:00', '2026-09-22 13:20:00'),
(6, 'ORD-2026-0006', 5, 'shipped', 649.50, 0.00, 51.96, 701.46, '{"name":"Charlie Kim","street":"222 Lakeview Terrace","city":"Austin","state":"TX","zip":"78701","country":"USA"}', 'apple_pay', NULL, '2026-10-18 10:00:00', '2026-10-19 08:30:00', NULL),
(7, 'ORD-2026-0007', 3, 'processing', 1099.00, 50.00, 83.92, 1132.92, '{"name":"Alice Chen","street":"100 Market St Suite 300","city":"San Francisco","state":"CA","zip":"94105","country":"USA"}', 'credit_card', 'VIP priority shipment.', '2026-10-19 14:30:00', NULL, NULL),
(8, 'ORD-2026-0008', 10, 'pending', 210.00, 0.00, 16.80, 226.80, '{"name":"Henry Thoreau","street":"Walden Pond Cabin","city":"Concord","state":"MA","zip":"01742","country":"USA"}', 'credit_card', 'Awaiting payment confirmation.', '2026-10-20 18:00:00', NULL, NULL);

-- -----------------------------------------------------------------------------
-- 5. Order Line Items
-- -----------------------------------------------------------------------------
INSERT INTO order_items (id, order_id, product_id, quantity, unit_price, total_price) VALUES
(1, 1, 1, 1, 3499.00, 3499.00),
(2, 2, 3, 1, 398.00, 398.00),
(3, 3, 5, 1, 1499.95, 1499.95),
(4, 3, 9, 1, 210.00, 210.00),
(5, 4, 6, 1, 59.99, 59.99),
(6, 4, 7, 1, 49.50, 49.50),
(7, 5, 8, 1, 829.99, 829.99),
(8, 5, 6, 1, 30.00, 30.00),
(9, 6, 4, 1, 649.50, 649.50),
(10, 7, 2, 1, 1099.00, 1099.00),
(11, 8, 9, 1, 210.00, 210.00);

-- -----------------------------------------------------------------------------
-- 6. Product Reviews
-- -----------------------------------------------------------------------------
INSERT INTO product_reviews (id, product_id, user_id, rating, title, content, is_verified_purchase, helpful_votes, created_at) VALUES
(1, 1, 3, 5, 'Unbelievable performance for Rust compilation', 'Compiling the entire fastgrid workspace takes seconds. Battery life easily lasts 14 hours under heavy load. The Liquid Retina XDR screen is gorgeous.', 1, 48, '2026-09-05 10:00:00'),
(2, 3, 4, 5, 'Best ANC for open plan offices', 'Blocks out all chatter and AC hum completely. Very lightweight and comfortable for long calls.', 1, 23, '2026-09-10 15:30:00'),
(3, 5, 6, 5, 'Boutique espresso right on my kitchen counter', 'The assisted tamping works like magic. Milk microfoam is silky enough for latte art.', 1, 17, '2026-09-18 11:20:00'),
(4, 6, 9, 5, 'Required reading for modern systems programmers', 'Jim Blandy and Jason Orendorff explain Rust concurrency and memory ownership with unparalleled clarity.', 1, 89, '2026-09-20 18:40:00'),
(5, 7, 2, 5, 'The distributed systems bible', 'Chapters on transaction isolation levels and distributed consensus algorithms are gold standard references.', 1, 112, '2026-09-25 09:15:00'),
(6, 4, 5, 4, 'Very sturdy desk, minor assembly hurdle', 'Motors are whisper quiet and rock solid at standing height. Instructions could be slightly clearer on cable management routing.', 1, 12, '2026-10-20 12:00:00');

-- -----------------------------------------------------------------------------
-- 7. All Data Types & Edge Case Showcase Table
-- -----------------------------------------------------------------------------
INSERT INTO all_types_showcase (
    id,
    col_bool_true,
    col_bool_false,
    col_bool_null,
    col_int_small,
    col_int_large,
    col_int_negative,
    col_float_standard,
    col_float_pi,
    col_decimal,
    col_text_simple,
    col_text_unicode,
    col_text_multiline,
    col_text_empty,
    col_text_literal_null,
    col_json_small,
    col_json_large,
    col_blob_small,
    col_blob_large,
    col_uuid,
    col_date,
    col_time,
    col_datetime,
    col_timestamp,
    col_always_null
) VALUES
(
    1,
    1,
    0,
    NULL,
    42,
    9223372036854775807,
    -1234567,
    123.456,
    3.1415926535,
    99999.9999,
    'FastGrid database client',
    'Unicode test: 中文测试 🚀 Rust 🦀 日本語 🌸 한국어 🇰🇷 العربية ⚡️',
    'Line 1 of text' || X'0A' || 'Line 2 of text with tab: ' || X'09' || 'after tab' || X'0A' || 'Line 3 concluding.',
    '',
    'NULL',
    '{"status":"ok","code":200}',
    '{"spec":{"version":"1.0","engine":"sqlite","options":{"wal":true,"cache_size":-2000,"busy_timeout":5000},"tags":["database","client","native","rust","fastgrid"]},"metrics":{"qps":15420,"p99_latency_ms":0.82}}',
    X'0102030405060708090A0B0C0D0E0F10',
    zeroblob(65536),
    '550e8400-e29b-41d4-a716-446655440000',
    '2026-10-20',
    '20:25:00',
    '2026-10-20 20:25:00',
    '2026-10-20T20:25:00Z',
    NULL
),
(
    2,
    1,
    0,
    NULL,
    0,
    1000000000,
    -1,
    0.0,
    2.7182818284,
    0.0000,
    'Second showcase row',
    'Emoji mix: 💻⚡️🔥🎉📦🚀🎨🛠️',
    'Single line here',
    '',
    'NULL',
    '{"valid":true}',
    '{"deep":{"nested":{"object":{"with":{"many":{"levels":{"value":42,"description":"Deeply nested JSON structure for pill test"}}}}}}}',
    X'DEADBEEFCAFEBABE',
    zeroblob(32768),
    '6ba7b810-9dad-11d1-80b4-00c04fd430c8',
    '2026-01-01',
    '00:00:00',
    '2026-01-01 00:00:00',
    '2026-01-01T00:00:00.000Z',
    NULL
);

COMMIT;
