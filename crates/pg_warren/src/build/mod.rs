//! Build pipeline adapter: reads node/edge sources through SPI and writes
//! chunked segments for a new version (DESIGN.md §5.4). Build logic talks to
//! Postgres only through narrow ports (a source reader, a segment store) so it
//! is unit-testable with in-memory fakes. Arrives in M2.
