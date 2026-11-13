# Toasty PostgreSQL 0.11.0 local patches

Source: the published `toasty-driver-postgresql` 0.11.0 crate from
https://github.com/tokio-rs/toasty, MIT license. `upstream.json` records the
published package checksum, VCS metadata and original file hashes. The license,
original manifest and VCS record are retained alongside the modified sources.

TLS URL parsing also accepts SQLx 0.8.6's `ssl-mode`, `ssl-root-cert` / `ssl-ca`,
`ssl-cert`, and `ssl-key` aliases. They follow the same validation and query-order
precedence as their canonical names, so existing verification and client identity
settings cannot silently fall back to `prefer`. Without the TLS feature, a secure
`ssl-mode` is rejected just like `sslmode`. Workspace regressions in
`crates/database/tests/tls.rs` validate these options without opening a connection.

The driver uses `Client::query_raw` instead of `Client::query` for returning
statements. Rows are decoded fallibly as Toasty consumes the stream, retaining
tokio-postgres's bounded response-channel backpressure. Count statements,
parameter types, result type hints, SQLSTATE classification and field decoding
are unchanged. The `tokio-stream` dependency supplies the stream mapping adapter.
Query logging no longer reports an eager result-row count; stream errors still
propagate to the caller when consumed.

Connections created by `Connection::connect` retain their network task and abort
it on drop. They also send a PostgreSQL cancellation request using the same TLS
configuration, bounded to five seconds: closing a socket alone does not interrupt
a backend waiting on a lock. This makes Toasty's explicit discard stop abandoned
queries rather than silently draining them. Connections created from an externally
managed `Client` leave that client's network-task lifecycle to its owner. The local
`connect` TLS parameter therefore also requires `Clone + Send` and a `Send` TLS
connector; both supported built-in connectors satisfy these bounds.

Regressions live in `crates/database/tests/stream.rs`, the existing connection-loss
and transaction tests, and Telemetry's snapshot/truncation/allocation tests. The
application owns a dedicated Toasty lease for the entire stream and discards it
on limits, errors or cancellation. Model execution and collected SQL still await
all result rows before returning to their callers.

Control replication also uses `PostgreSQL::listen` and its owned `Listener`.
This dedicated connection shares the driver's URL parsing, startup options and
TLS connector with pooled connections, without retaining a pool lease. Its
network task continuously drives `poll_message`, with at most 64 buffered wakeups;
notifications beyond that bound may coalesce and the Control's durable database
queue plus periodic reconciliation remain authoritative. Connection loss is
returned to the caller for reconnect, and dropping the listener aborts its task.
Channel identifiers are validated and quoted before LISTEN. Regressions in
`crates/database/tests/listen.rs`, `tests/tls.rs`, and the Control replication
suite cover transaction rollback, delivery, disconnect, cleanup and TLS options.

Typed SQL rejects a returned column count that differs from the declared types
with a driver error rather than indexing beyond the declarations. The HA schema
adds a node column; OTA reads now use explicit projections so these declarations
remain stable. `crates/database/tests/values.rs` covers both mismatch directions
and reuse of the same pool afterward.
