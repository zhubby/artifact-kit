# Artifactory store extraction and Toasty migration

Approved scope: consolidate data access first, then remove all SQLx production/test/migration dependencies in one final release. Use Toasty 0.11 models for ordinary CRUD and parameterized raw SQL for locks, aggregate projections and TimescaleDB. Preserve external HTTP/gRPC behavior, authorization rechecks, lock order and transaction boundaries. Rebuild fresh databases; retain the old environment for rollback. No history import is required.

## Execution
1. Establish isolated TimescaleDB/S3 test fixtures and baseline checks/performance.
2. Extract Control, Runtime, Source, Telemetry and read-only business authorization stores; entrypoints receive domain DTOs, not pools or database rows. Verify the behavior-preserving SQLx checkpoint.
3. Introduce Toasty PostgreSQL infrastructure, checked conversions, SQLSTATE errors and private models. Migrate store operations and all test fixtures. Keep multi-table actions on one transaction.
4. Create reviewed embedded migration baselines for each enabled database, including triggers, partial indexes, hypertables, continuous aggregates, columnstore and retention. Serialize migrations; refuse unknown existing schemas.
5. Bound telemetry reads to 128-row pages in a stable snapshot and retain 6000-row/2MiB response limits and batch writes.
6. Enforce boundaries and dependency removal in CI. Run formatting, clippy, tests including ignored database regressions, protocol and deployment/recovery acceptance.
7. Compare p95 of scheduling, heartbeats, event submission and telemetry against the same-environment baseline (maximum 10% regression). Verify bounded result memory.
8. Prepare fresh databases/object namespace and bootstrap; stop intake, drain running work, preserve old state, switch all services and re-enroll agents. Archive old agent spool/identity. Validate before reopening; roll back the complete environment if pre-opening checks fail. Never reset an unrecognized existing schema automatically.

## Architecture
Shared database infrastructure owns connection settings, extension checks, migrations, error/type conversion. Each service owns concrete store modules, private models and DTOs. SQL, model calls, row decoding and transaction management stay inside those modules. Pure business rules, permissions and external IO stay in services, except transaction-scoped authorization revalidation remains inside stores.

## Validation
Reuse registration/limit, scheduler race, lease/session fencing, OTA, source lock/idempotency/cache, publication, runtime budgets/outbox, telemetry replay/retention/size, migration failure/concurrency and backup restoration regressions. Test HTTP/gRPC/browser/Compose/Operator profiles. Validate no SQLx in the dependency graph and no SQL or Toasty references in entrypoints.
