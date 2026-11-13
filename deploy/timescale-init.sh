#!/bin/sh
(
set -eu
# Runs only when initializing a fresh TimescaleDB data directory, as the database administrator.
psql --username "$POSTGRES_USER" --dbname postgres --set ON_ERROR_STOP=1 <<'SQL'
\getenv telemetry_password ARTIFACTORY_TELEMETRY_DATABASE_PASSWORD
SELECT format('CREATE ROLE artifactory_telemetry LOGIN PASSWORD %L', :'telemetry_password')
WHERE NOT EXISTS(SELECT 1 FROM pg_roles WHERE rolname='artifactory_telemetry') \gexec
SELECT 'CREATE DATABASE artifactory_telemetry OWNER artifactory_telemetry'
WHERE NOT EXISTS(SELECT 1 FROM pg_database WHERE datname='artifactory_telemetry') \gexec
\connect artifactory_telemetry
CREATE EXTENSION IF NOT EXISTS timescaledb;
SQL
)
