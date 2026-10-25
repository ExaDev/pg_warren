"""SQL-surface fuzzing (DESIGN.md §13, M11 "Robustness"): every function
in schema `warren`, read from `pg_proc`, called with generated arguments
that mix valid values (the fixture's graphs, versions, vectors, relations
and node ids) with hostile ones, as roles of different privilege, inside
transactions and savepoints that roll back at random. Every outcome is
judged (`verdict`): no crash, no internal error, only documented SQLSTATE
classes, no call that succeeds beyond its caller's privileges or runs the
caller's SQL elevated. A failure is shrunk (`shrink`) to the fewest
statements that reproduce it from the fixture, and every run ends by
checking what is left (`settled`)."""
