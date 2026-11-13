# Toasty PostgreSQL 0.11.0 bounded native results

Source: the published `toasty-driver-postgresql` 0.11.0 crate from
https://github.com/tokio-rs/toasty, MIT license. `upstream.json` records the
published package checksum, VCS metadata and original file hashes. The license,
original manifest and VCS record are retained alongside the modified sources.

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
