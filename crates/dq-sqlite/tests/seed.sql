CREATE TABLE customers (id INTEGER PRIMARY KEY, email TEXT NOT NULL UNIQUE, created_at TEXT DEFAULT CURRENT_TIMESTAMP);
CREATE TABLE orders (
  id INTEGER PRIMARY KEY,
  customer_id INTEGER NOT NULL REFERENCES customers (id) ON DELETE CASCADE,
  status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'paid', 'refunded')),
  total NUMERIC(12,2) NOT NULL,
  notes TEXT
);
CREATE INDEX orders_status_idx ON orders (status);
CREATE TABLE type_samples (id INTEGER PRIMARY KEY, big INTEGER, ratio REAL, doc JSON, raw BLOB, label TEXT,
  total REAL GENERATED ALWAYS AS (ratio * 2) VIRTUAL);
CREATE TABLE no_pk (a INTEGER, b TEXT);
CREATE TABLE audit (order_id INTEGER, status TEXT);
CREATE VIEW paid_orders AS SELECT * FROM orders WHERE status = 'paid';
CREATE TRIGGER orders_audit AFTER UPDATE OF status ON orders BEGIN
  INSERT INTO audit VALUES (new.id, CASE WHEN new.status = 'paid' THEN 'PAID' ELSE new.status END);
END;
WITH RECURSIVE g(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM g WHERE n < 50)
INSERT INTO customers (id, email) SELECT n, 'user' || n || '@example.com' FROM g;
WITH RECURSIVE g(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM g WHERE n < 1200)
INSERT INTO orders (customer_id, status, total, notes)
SELECT (n % 50) + 1, CASE n % 3 WHEN 0 THEN 'pending' WHEN 1 THEN 'paid' ELSE 'refunded' END,
       round((n * 37 % 400 + 12) + (n % 100) / 100.0, 2),
       CASE n % 4 WHEN 0 THEN NULL WHEN 1 THEN '' ELSE 'Gift wrap' END
FROM g;
INSERT INTO type_samples (id, big, ratio, doc, raw, label) VALUES
  (1, 9223372036854775807, 1.5, '{"name": "Zoë ✓"}', X'89504E47', ''),
  (2, NULL, NULL, NULL, NULL, NULL);
INSERT INTO no_pk VALUES (1, 'x'), (1, 'x');
