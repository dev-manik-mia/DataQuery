-- Sample schema used by integration tests and for trying the app locally.
CREATE USER dataquery IDENTIFIED BY 'dataquery123';
CREATE SCHEMA dataquery_test;
GRANT ALL ON SCHEMA dataquery_test TO dataquery WITH GRANT OPTION;
GRANT CREATE ON DATABASE vdb TO dataquery;
GRANT SYSMONITOR TO dataquery;
ALTER USER dataquery DEFAULT ROLE SYSMONITOR;

CREATE TABLE dataquery_test.customers (
  id INT NOT NULL PRIMARY KEY,
  email VARCHAR(255) NOT NULL,
  created_at TIMESTAMPTZ DEFAULT now()
) ORDER BY id SEGMENTED BY HASH(id) ALL NODES;
COMMENT ON TABLE dataquery_test.customers IS 'People who order';

CREATE TABLE dataquery_test.orders (
  id IDENTITY(1, 1) PRIMARY KEY,
  customer_id INT NOT NULL REFERENCES dataquery_test.customers (id),
  status VARCHAR(20) DEFAULT 'pending',
  total NUMERIC(12, 2) NOT NULL,
  notes VARCHAR(200),
  UNIQUE (customer_id, id)
);

CREATE TABLE dataquery_test.type_samples (
  id INT PRIMARY KEY,
  big INT,
  exact NUMERIC(38, 10),
  ratio FLOAT,
  flag BOOLEAN,
  day DATE,
  at TIME,
  at_tz TIMETZ,
  ts TIMESTAMP,
  ts_tz TIMESTAMPTZ,
  span INTERVAL DAY TO SECOND,
  label CHAR(4),
  doc LONG VARCHAR,
  raw VARBINARY(16),
  uid UUID,
  tags ARRAY[VARCHAR(10)],
  shape GEOMETRY
);

CREATE TABLE dataquery_test.no_key (a INT, b VARCHAR(10));

CREATE VIEW dataquery_test.paid_orders AS SELECT * FROM dataquery_test.orders WHERE status = 'paid';

CREATE SEQUENCE dataquery_test.invoice_numbers START 1000;

INSERT INTO dataquery_test.customers (id, email)
SELECT n, 'user' || n || '@example.com' FROM (
  SELECT ROW_NUMBER() OVER () AS n FROM v_catalog.system_columns LIMIT 50
) numbers;

INSERT INTO dataquery_test.orders (customer_id, status, total, notes)
SELECT (n % 50) + 1, CASE n % 3 WHEN 0 THEN 'pending' WHEN 1 THEN 'paid' ELSE 'refunded' END,
       ((n * 37) % 400 + 12) + (n % 100) / 100, CASE n % 4 WHEN 0 THEN NULL WHEN 1 THEN '' ELSE 'Gift wrap' END
FROM (SELECT ROW_NUMBER() OVER () AS n FROM v_catalog.system_columns LIMIT 1200) numbers;

INSERT INTO dataquery_test.type_samples VALUES (
  1, 9223372036854775807, 1234567890123456789012345678.0123456789, 1.5, true, '2026-10-14', '09:59:00.123',
  '09:59:00+02', '2026-10-14 09:59:00.123456', '2026-10-14 09:59:00.123+00', INTERVAL '1 02:03:04', 'ab',
  'Zoë ✓', HEX_TO_BINARY('89504E47'), 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', ARRAY['a', 'b c'],
  ST_GeomFromText('POINT(1 2)')
);
INSERT INTO dataquery_test.type_samples (id) VALUES (2);

INSERT INTO dataquery_test.no_key VALUES (1, 'x');
INSERT INTO dataquery_test.no_key VALUES (1, 'x');
GRANT ALL ON ALL TABLES IN SCHEMA dataquery_test TO dataquery;
GRANT ALL ON ALL SEQUENCES IN SCHEMA dataquery_test TO dataquery;
COMMIT;
