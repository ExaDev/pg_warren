# Try a change and roll it back

Add a person and an edge, build a staged graph version, and query the
proposal without changing the current version. Then undo both the table
changes and the staged build. Run [the setup](setup.md) once, then execute
the whole script with no other transaction already open. In pgAdmin,
leave autocommit enabled for this script's explicit transaction commands.

## Stage and inspect the proposed change

<!-- sql:versions.sql -->

After the inserts, `current_reachable` is still **10,000** because it reads
the existing built version. The staged refresh sees your transaction's
new rows and reaches **10,001**, reported under Messages alongside the
version difference. It is queryable by its explicit version id but has
not been promoted to current.

After rollback, both `source_rows_after_rollback` and
`reachable_after_rollback` are **10,000**. The staged version is gone,
and existing persisted vectors still belong to the original version.

## Apply the pattern to a scenario

The same workflow can test an edge removal or a facility closure. Make
the proposed changes in a transaction, refresh with `promote := false`,
query that explicit version, then choose to roll back or promote and commit.
Analyses that omit `version` continue reading the current graph, so pass
the staged id consistently when evaluating a scenario. See
[graphs and versions](../semantics.md#versions).
