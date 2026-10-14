-- Sample schema used by integration tests and for trying the app locally.
CREATE DATABASE dataquery_test;
GO
CREATE DATABASE billing;
GO
CREATE LOGIN dataquery WITH PASSWORD = 'dataquery', CHECK_POLICY = OFF, DEFAULT_DATABASE = dataquery_test;
-- Monitoring (screen 07) reads every session and may end one.
GRANT VIEW SERVER STATE TO dataquery;
GRANT ALTER ANY CONNECTION TO dataquery;
-- Scratch databases for schema-diff tests.
GRANT CREATE ANY DATABASE TO dataquery;
GO
USE dataquery_test;
GO
CREATE USER dataquery FOR LOGIN dataquery;
ALTER ROLE db_owner ADD MEMBER dataquery;
GO
CREATE SCHEMA sales;
GO
CREATE TABLE dbo.customers (
  id         BIGINT IDENTITY(1,1) PRIMARY KEY,
  email      NVARCHAR(255) NOT NULL UNIQUE,
  created_at DATETIME2(3) NOT NULL DEFAULT SYSUTCDATETIME()
);
CREATE TABLE dbo.orders (
  id          BIGINT IDENTITY(1,1) CONSTRAINT orders_pkey PRIMARY KEY,
  customer_id BIGINT NOT NULL CONSTRAINT orders_customer_fk REFERENCES dbo.customers (id),
  status      NVARCHAR(20) NOT NULL DEFAULT 'pending'
              CONSTRAINT orders_status_check CHECK (status IN ('pending', 'paid', 'refunded')),
  total       DECIMAL(12,2) NOT NULL,
  discount    DECIMAL(12,2) NULL,
  notes       NVARCHAR(MAX) NULL,
  metadata    NVARCHAR(MAX) NULL,
  created_at  DATETIME2(3) NOT NULL DEFAULT SYSUTCDATETIME()
);
CREATE INDEX orders_created_at_idx ON dbo.orders (created_at DESC);
CREATE TABLE dbo.type_samples (
  id    INT PRIMARY KEY,
  big   BIGINT,
  exact DECIMAL(38,10),
  ts    DATETIMEOFFSET(3),
  doc   NVARCHAR(MAX),
  raw   VARBINARY(64),
  label NVARCHAR(50),
  flag  BIT,
  uid   UNIQUEIDENTIFIER,
  price MONEY,
  day   DATE,
  at    TIME(3),
  ratio FLOAT,
  geo   GEOGRAPHY,
  x     XML,
  node  HIERARCHYID
);
CREATE TABLE dbo.no_pk (a INT, b NVARCHAR(10));
GO
CREATE VIEW dbo.paid_orders AS SELECT * FROM dbo.orders WHERE status = 'paid';
GO
CREATE PROCEDURE dbo.order_count @status NVARCHAR(20) AS
  SELECT COUNT(*) AS orders FROM dbo.orders WHERE status = @status;
GO
CREATE FUNCTION dbo.order_total (@id BIGINT) RETURNS DECIMAL(12,2) AS
BEGIN
  RETURN (SELECT total FROM dbo.orders WHERE id = @id);
END;
GO
CREATE TRIGGER dbo.orders_touch ON dbo.orders AFTER UPDATE AS SET NOCOUNT ON;
GO
CREATE SEQUENCE dbo.invoice_seq START WITH 1;
GO
WITH n AS (SELECT TOP 1200 ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS g
           FROM sys.all_objects a CROSS JOIN sys.all_objects b)
INSERT INTO dbo.customers (email) SELECT CONCAT('user', g, '@example.com') FROM n WHERE g <= 50 ORDER BY g;
WITH n AS (SELECT TOP 1200 ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS g
           FROM sys.all_objects a CROSS JOIN sys.all_objects b)
INSERT INTO dbo.orders (customer_id, status, total, discount, notes)
SELECT (g % 50) + 1, CHOOSE((g % 3) + 1, 'pending', 'paid', 'refunded'),
       ROUND((g * 37 % 400 + 12) + (g % 100) / 100.0, 2),
       IIF(g % 3 = 2, NULL, 5.00),
       CASE g % 4 WHEN 0 THEN NULL WHEN 1 THEN '' ELSE 'Gift wrap' END
FROM n ORDER BY g;
INSERT INTO dbo.type_samples VALUES
  (1, 9223372036854775807, 1234567890123456789012345678.0123456789, '2026-10-14 09:59:00.123 +02:00',
   N'{"name": "Zoë ✓", "n": 1}', 0x89504E470D0A1A0A, N'', 1, 'A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11',
   12.3456, '2026-10-14', '09:59:00.123', 1.5, geography::Point(48.85, 2.35, 4326), N'<a>1</a>',
   hierarchyid::Parse('/1/2/')),
  (2, -9223372036854775808, -0.0000000001, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
   NULL, NULL, NULL);
INSERT INTO dbo.no_pk VALUES (1, 'x'), (1, 'x');
GO
CREATE TABLE sales.regions (code CHAR(2) PRIMARY KEY, name NVARCHAR(50) NOT NULL);
INSERT INTO sales.regions VALUES ('EU', N'Europe'), ('NA', N'North America');
GO
USE billing;
GO
CREATE USER dataquery FOR LOGIN dataquery;
ALTER ROLE db_owner ADD MEMBER dataquery;
CREATE TABLE dbo.invoices (id INT IDENTITY PRIMARY KEY, amount DECIMAL(12,2) NOT NULL);
GO
