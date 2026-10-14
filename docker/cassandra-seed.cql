-- Seed for the Cassandra / ScyllaDB integration tests (the tests apply it with cqlsh or themselves).
CREATE KEYSPACE IF NOT EXISTS telemetry WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1};
CREATE TYPE IF NOT EXISTS telemetry.location (lat double, lon double, label text);
CREATE TABLE IF NOT EXISTS telemetry.devices (
  device_id text PRIMARY KEY,
  name text,
  location frozen<location>,
  tags set<text>,
  attrs map<text, text>,
  installed timestamp
);
CREATE TABLE IF NOT EXISTS telemetry.readings_by_device (
  device_id text,
  bucket date,
  ts timestamp,
  metric text,
  value double,
  raw blob,
  big varint,
  exact decimal,
  PRIMARY KEY ((device_id, bucket), ts)
) WITH CLUSTERING ORDER BY (ts DESC);
INSERT INTO telemetry.devices (device_id, name, location, tags, attrs, installed)
  VALUES ('dev-88f2', 'Boiler', {lat: 48.85, lon: 2.35, label: 'Paris'}, {'heat', 'eu'}, {'fw': '1.2'}, '2026-01-02 03:04:05+0000');
INSERT INTO telemetry.readings_by_device (device_id, bucket, ts, metric, value, raw, big, exact)
  VALUES ('dev-88f2', '2026-10-14', '2026-10-14 09:59:12+0000', 'cpu.temp', 61.25, 0xcafe, 9007199254740993, 120.50);
INSERT INTO telemetry.readings_by_device (device_id, bucket, ts, metric, value)
  VALUES ('dev-88f2', '2026-10-14', '2026-10-14 09:58:01+0000', 'battery.v', 3.7);
