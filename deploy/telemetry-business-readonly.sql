-- Run with psql as the business database owner after Control initializes its baseline.
-- Supply business_database, business_schema and telemetry_business_role via psql -v.
GRANT CONNECT ON DATABASE :"business_database" TO :"telemetry_business_role";
GRANT USAGE ON SCHEMA :"business_schema" TO :"telemetry_business_role";
GRANT SELECT ON TABLE :"business_schema".nodes, :"business_schema".attempts,
    :"business_schema".__toasty_migrations
    TO :"telemetry_business_role";
