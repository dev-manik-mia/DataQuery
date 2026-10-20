-- Test account for the Greenplum integration tests (crates/dq-postgres/tests/greenplum.rs),
-- run by the compose service once the cluster is up. The tests create their own tables.
SELECT 'CREATE ROLE dataquery LOGIN SUPERUSER PASSWORD ''dataquery'''
WHERE NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'dataquery') \gexec
SELECT 'CREATE DATABASE dataquery_test OWNER dataquery'
WHERE NOT EXISTS (SELECT 1 FROM pg_database WHERE datname = 'dataquery_test') \gexec
