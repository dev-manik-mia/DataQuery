-- Sample schema used by integration tests and for trying the app locally.
CREATE TABLE public.customers (
  id         bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  email      text NOT NULL UNIQUE,
  created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE public.orders (
  id          bigint GENERATED ALWAYS AS IDENTITY,
  customer_id bigint NOT NULL,
  status      text NOT NULL DEFAULT 'pending',
  total       numeric(12,2) NOT NULL,
  discount    numeric(12,2),
  notes       text,
  metadata    jsonb NOT NULL DEFAULT '{}',
  created_at  timestamptz NOT NULL DEFAULT now(),
  updated_at  timestamptz,
  CONSTRAINT orders_pkey PRIMARY KEY (id),
  CONSTRAINT orders_customer_fk FOREIGN KEY (customer_id) REFERENCES public.customers (id),
  CONSTRAINT orders_status_check CHECK (status IN ('pending', 'paid', 'refunded'))
);
CREATE INDEX orders_created_at_idx ON public.orders (created_at DESC);

CREATE TABLE public.type_samples (
  id        int PRIMARY KEY,
  big       int8,
  exact     numeric(38,10),
  ts        timestamptz,
  doc       jsonb,
  raw       bytea,
  label     text,
  tags      text[],
  uid       uuid,
  addr      inet,
  span      interval
);

CREATE TABLE public.no_pk (a int, b text);

CREATE VIEW public.paid_orders AS SELECT * FROM public.orders WHERE status = 'paid';
CREATE MATERIALIZED VIEW public.daily_revenue AS
  SELECT date_trunc('day', created_at) AS day, sum(total) AS revenue FROM public.orders GROUP BY 1;

CREATE SCHEMA billing;
CREATE TABLE billing.invoices (id serial PRIMARY KEY, amount numeric(12,2) NOT NULL);

INSERT INTO public.customers (email)
SELECT 'user' || g || '@example.com' FROM generate_series(1, 50) g;

INSERT INTO public.orders (customer_id, status, total, discount, notes, created_at)
SELECT (g % 50) + 1,
       (ARRAY['pending','paid','refunded'])[(g % 3) + 1],
       round((g * 37 % 400 + 12)::numeric + (g % 100) / 100.0, 2),
       CASE WHEN g % 3 = 2 THEN NULL ELSE 5.00 END,
       CASE g % 4 WHEN 0 THEN NULL WHEN 1 THEN '' ELSE 'Gift wrap' END,
       now() - (g || ' minutes')::interval
FROM generate_series(1, 1200) g;

INSERT INTO public.type_samples VALUES
  (1, 9007199254740993, 1234567890123456789012345678.0123456789, '2026-10-14 09:59:00.123+02',
   '{"name": "Zoë ✓", "n": 1}', '\x89504e470d0a1a0a', '', '{a,"b c",NULL}',
   'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', '192.168.0.1/24', '1 year 2 mons 3 days 04:05:06'),
  (2, -9223372036854775808, -0.0000000001, NULL, 'null', NULL, NULL, '{}', NULL, NULL, '-00:00:01');

INSERT INTO public.no_pk VALUES (1, 'x'), (1, 'x');
