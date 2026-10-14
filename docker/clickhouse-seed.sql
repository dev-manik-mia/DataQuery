-- Sample schema used by integration tests and for trying the app locally.
CREATE TABLE dataquery_test.customers (
  id UInt64,
  email String,
  created_at DateTime64(3) DEFAULT now64(3)
) ENGINE = MergeTree ORDER BY id COMMENT 'People who order';

CREATE TABLE dataquery_test.orders (
  id UInt64,
  customer_id UInt64,
  status LowCardinality(String) DEFAULT 'pending',
  total Decimal(12, 2),
  discount Nullable(Decimal(12, 2)),
  notes Nullable(String),
  created_at DateTime64(3, 'UTC') DEFAULT now64(3),
  INDEX orders_status_idx status TYPE set(0) GRANULARITY 1
) ENGINE = MergeTree ORDER BY id;

CREATE TABLE dataquery_test.type_samples (
  id UInt32,
  big UInt64,
  huge Int128,
  exact Decimal(38, 10),
  ts DateTime64(3, 'Europe/Paris'),
  day Date,
  doc String,
  raw String,
  label Nullable(String),
  tags Array(String),
  attrs Map(String, UInt32),
  flag Bool,
  ratio Float64,
  uid UUID,
  ip IPv4
) ENGINE = MergeTree ORDER BY id;

CREATE TABLE dataquery_test.no_key (a UInt8, b String) ENGINE = Log;

CREATE VIEW dataquery_test.paid_orders AS SELECT * FROM dataquery_test.orders WHERE status = 'paid';

CREATE TABLE dataquery_test.daily_revenue (day Date, revenue Decimal(18, 2)) ENGINE = SummingMergeTree ORDER BY day;
CREATE MATERIALIZED VIEW dataquery_test.daily_revenue_mv TO dataquery_test.daily_revenue AS
  SELECT toDate(created_at) AS day, sum(total) AS revenue FROM dataquery_test.orders GROUP BY day;

INSERT INTO dataquery_test.customers (id, email) SELECT number + 1, concat('user', toString(number + 1), '@example.com') FROM numbers(50);

INSERT INTO dataquery_test.orders (id, customer_id, status, total, discount, notes)
SELECT number + 1, ((number + 1) % 50) + 1, ['pending', 'paid', 'refunded'][((number + 1) % 3) + 1],
       round(((number + 1) * 37 % 400 + 12) + ((number + 1) % 100) / 100, 2),
       if((number + 1) % 3 = 2, NULL, 5.00),
       multiIf((number + 1) % 4 = 0, NULL, (number + 1) % 4 = 1, '', 'Gift wrap')
FROM numbers(1200);

INSERT INTO dataquery_test.type_samples VALUES
  (1, 18446744073709551615, -170141183460469231731687303715884105728, 1234567890123456789012345678.0123456789,
   '2026-10-14 09:59:00.123', '2026-10-14', '{"name": "Zoë ✓", "n": 1}', unhex('89504E47'), '',
   ['a', 'b c'], {'x': 1}, true, 1.5, 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', '192.168.0.1'),
  (2, 0, 0, -0.0000000001, '1970-01-01 00:00:00', '1970-01-01', '', '', NULL, [], {}, false, 0, '00000000-0000-0000-0000-000000000000', '0.0.0.0');

INSERT INTO dataquery_test.no_key VALUES (1, 'x'), (1, 'x');

CREATE DATABASE billing;
CREATE TABLE billing.invoices (id UInt32, amount Decimal(12, 2)) ENGINE = MergeTree ORDER BY id;
INSERT INTO billing.invoices VALUES (1, 10.00), (2, 20.50);
