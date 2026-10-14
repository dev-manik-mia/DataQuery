-- Sample schema used by integration tests and for trying the app locally.
CREATE TABLE customers (
  id         BIGINT AUTO_INCREMENT PRIMARY KEY,
  email      VARCHAR(255) NOT NULL UNIQUE,
  created_at DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3)
);

CREATE TABLE orders (
  id          BIGINT AUTO_INCREMENT PRIMARY KEY,
  customer_id BIGINT NOT NULL,
  status      ENUM('pending','paid','refunded') NOT NULL DEFAULT 'pending',
  total       DECIMAL(12,2) NOT NULL,
  discount    DECIMAL(12,2) NULL,
  notes       TEXT NULL,
  metadata    JSON NULL,
  created_at  DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
  CONSTRAINT orders_customer_fk FOREIGN KEY (customer_id) REFERENCES customers (id),
  INDEX orders_created_at_idx (created_at)
);

CREATE TABLE type_samples (
  id    INT PRIMARY KEY,
  big   BIGINT UNSIGNED,
  exact DECIMAL(38,10),
  ts    TIMESTAMP(3) NULL,
  doc   JSON,
  raw   VARBINARY(64),
  label VARCHAR(50)
);

CREATE TABLE no_pk (a INT, b VARCHAR(10));

CREATE VIEW paid_orders AS SELECT * FROM orders WHERE status = 'paid';

INSERT INTO customers (email)
WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 50)
SELECT CONCAT('user', n, '@example.com') FROM seq;

INSERT INTO orders (customer_id, status, total, discount, notes)
WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 1200)
SELECT (n % 50) + 1, ELT((n % 3) + 1, 'pending', 'paid', 'refunded'),
       ROUND((n * 37 % 400 + 12) + (n % 100) / 100.0, 2),
       IF(n % 3 = 2, NULL, 5.00),
       CASE n % 4 WHEN 0 THEN NULL WHEN 1 THEN '' ELSE 'Gift wrap' END
FROM seq;

INSERT INTO type_samples VALUES
  (1, 18446744073709551615, 1234567890123456789012345678.0123456789, '2026-10-14 09:59:00.123',
   '{"name": "Zoë ✓", "n": 1}', X'89504E470D0A1A0A', ''),
  (2, 0, -0.0000000001, NULL, NULL, NULL, NULL);

INSERT INTO no_pk VALUES (1, 'x'), (1, 'x');
