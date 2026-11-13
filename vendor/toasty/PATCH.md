# Toasty 0.11.0 connection recovery and native streaming patches

Source: the published `toasty` 0.11.0 crate from
https://github.com/tokio-rs/toasty, MIT license. `upstream.json` records the
published crate checksum, Git revision and SHA256 of every unmodified source
file. Core and macros remain the published 0.11.0 versions. The PostgreSQL
driver streaming patch is documented in `../toasty-driver-postgresql/PATCH.md`.

The local manifest includes only the PostgreSQL backend. Other backend
dependencies and the SQLite development driver are removed, so dormant MySQL
dependencies cannot retain SQLx in Cargo.lock. Their upstream source files and
feature names are retained for provenance; those backends are unsupported in
this private package. PostgreSQL runtime features and implementation are unchanged.

Runtime changes:

- `src/db/pool.rs` waits for a worker-channel barrier when recycling. A dropped
  transaction queues rollback without awaiting it. Without the barrier, a new
  checkout can enqueue a query behind rollback; if rollback discovers a lost
  connection, the worker exits and drops that query's response. The barrier
  waits for rollback and discards a dead slot without an extra SQL round trip.
- `src/db/connection_task.rs` handles the barrier and checks driver validity.
- `src/db/connection.rs` returns typed connection-lost errors when sending or
  receiving fails instead of panicking on channel closure.
- `src/sql.rs` exposes native query streams on dedicated connections, without
  collecting them. `Connection::discard` removes a lease from the pool and aborts
  its worker when a stream is abandoned, including cancellation during prepare.
  The owning PostgreSQL driver then closes the network connection. Normal
  completed streams release their lease for reuse.

Regression coverage lives in `crates/database/tests/migrations.rs`, especially
`lost_connection_rolls_back_and_pool_recovers` and cancellation/savepoint
rollback. The lost-connection test fails with a `RecvError` panic on unpatched
0.11.0. Do not remove the patch until the upstream version passes these tests.
Native stream page, lease, error and cancellation contracts are covered by
`crates/database/tests/stream.rs`; snapshot and memory limits are covered by the
Telemetry store tests. Query logging records submission errors; streamed row
counts and errors discovered during consumption are not reported by that log.
