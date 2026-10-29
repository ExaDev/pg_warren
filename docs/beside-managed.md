# Beside a managed database

Managed PostgreSQL services (Amazon RDS and Aurora, Cloud SQL, AlloyDB,
Azure, Supabase, Neon and the like) install only the extensions the
provider ships. pg_warren is not one of them yet
([#103](https://github.com/ExaDev/pg_warren/issues/103)), so you cannot
`CREATE EXTENSION pg_warren` there.

You can still use it. Run a PostgreSQL server of your own with pg_warren
installed, the **graph server** (a sidecar), next to the managed database.
Copy the node and edge tables to it, and build graphs there:

```
application ──writes──────▶ managed primary (no pg_warren)
     │                           │  logical replication of
     │                           ▼  the node and edge tables
     └──graph queries──────▶ graph server (pg_warren)
```

The managed primary can also read results from the graph server, through
`postgres_fdw` (below).

The graph server can be anything you run yourself: an EC2 instance or an
EKS pod beside RDS, from the apt packages or the container images
([installation](../README.md#install)).

## Which way to feed it

| | Logical replication (recommended) | `postgres_fdw` at each refresh |
|---|---|---|
| How | the primary streams every change of the node and edge tables to copies on the graph server | the graph's queries read the primary's tables over the network |
| Each `refresh` reads | local tables | every node and edge row, from the primary |
| Load on the primary | streaming the changes, once | a full scan of both tables per refresh |
| Freshness | usually seconds behind the primary, plus the time since the last refresh | the primary as it is at the refresh |
| Setup on the primary | logical replication turned on (a restart on RDS), a role and a publication | a role that can read the tables |
| Use when | graphs of any size, refreshed often | small graphs, refreshed rarely, or while setting replication up |

Results come back to the application in one of two ways:

- **Query the graph server directly.** The replicated tables are there too,
  so graph results join with your rows (`JOIN people p ON p.id =
  b.node_id`), as on any server with pg_warren. Use this for traversals per
  request (`bfs`, `shortest_path`, ...).
- **Read precomputed results from the primary**, through `postgres_fdw`
  foreign tables over result tables on the graph server. Use this for
  whole-graph results, such as a rank per node, that the application
  joins with its own tables on the primary.

The rest of this page sets both up on two tables, `people` and `follows`.
Each SQL block says where it runs. The blocks run as written, in this
order, in CI (`test/logical_replication.py`), on two local servers: a
primary without pg_warren and a graph server with it. The one exception
is the pg_cron block, since CI has no pg_cron.

## Pattern 1: logical replication

### 1. Turn on logical replication on the primary

Logical replication needs `wal_level = logical` on the primary.

- **Amazon RDS:** set `rds.logical_replication` to `1` in the instance's
  DB parameter group, and reboot the instance (the parameter is static).
  **Aurora:** the same parameter, in the DB *cluster* parameter group.
- **Cloud SQL:** the `cloudsql.logical_decoding` flag. **Azure Database for
  PostgreSQL:** the `wal_level` server parameter.
- **Your own server:** `wal_level = logical` in `postgresql.conf`, and a
  restart.

### 2. Create the tables on both servers

Logical replication copies rows, not the schema. Create each table on the
graph server with the same name and columns. Every replicated table needs
a primary key (or another replica identity): without one, the primary
refuses `UPDATE` and `DELETE` on a published table.

```sql
-- On both servers: the node and edge tables
CREATE TABLE people  (id bigint PRIMARY KEY, name text NOT NULL);
CREATE TABLE follows (src bigint NOT NULL, dst bigint NOT NULL, PRIMARY KEY (src, dst));
```

Schema changes (`ALTER TABLE`) are not replicated either. Make them on both
servers: add a column on the graph server first, drop one on the primary
first.

### 3. Publish the tables on the primary

A role for the graph server to connect as, which can read only these
tables, and a publication of them:

```sql
-- On the managed primary: a role for the graph server, and the publication
CREATE ROLE warren_replica LOGIN PASSWORD :'replica_password';
GRANT rds_replication TO warren_replica;
GRANT SELECT ON people, follows TO warren_replica;
CREATE PUBLICATION graph_source FOR TABLE people, follows;
```

`rds_replication` is the RDS and Aurora role that allows replication
connections. On a server you run yourself, use `ALTER ROLE warren_replica
REPLICATION` instead. The publication must be created by the tables'
owner or `rds_superuser`. `FOR ALL TABLES` needs a true superuser, which
RDS does not give you, so name the tables.

`:'replica_password'` and the other `:'name'` values below are psql
variables. Set them first, keeping passwords out of your shell history:

```
\prompt 'Password for warren_replica: ' replica_password
```

### 4. Subscribe on the graph server

On the graph server, set the connection string to the primary. Use
`sslmode=verify-full` and the provider's certificate authority: for RDS,
download the [global
bundle](https://truststore.pki.rds.amazonaws.com/global/global-bundle.pem)
and name it in `sslrootcert`:

```
\set primary 'host=app-db.abc123.eu-west-1.rds.amazonaws.com port=5432 dbname=app user=warren_replica sslmode=verify-full sslrootcert=/etc/postgresql/rds-global-bundle.pem'
```

Keep the password out of the connection string: the subscription stores
the string in the catalog. Put it in the `.pgpass` file of the operating
system user the graph server runs as (`~postgres/.pgpass`, mode `0600`):
`app-db.abc123.eu-west-1.rds.amazonaws.com:5432:app:warren_replica:<password>`.

Then install pg_warren and subscribe, as a superuser (on PostgreSQL 16 and
later, a member of `pg_create_subscription` may too):

```sql
-- On the graph server: pg_warren and the subscription
CREATE EXTENSION pg_warren;
CREATE SUBSCRIPTION graph_source CONNECTION :'primary' PUBLICATION graph_source;
```

The subscription first copies each table whole, then streams every change
the primary commits. Wait for the first copy to finish before you build a
graph. Until then a table may be empty or half copied:

```sql
-- On the graph server: has the first copy finished?
SELECT count(*) = 0 AS copied FROM pg_subscription_rel WHERE srsubstate <> 'r';
```

Do not write to the replicated tables on the graph server. A row that
conflicts with a change from the primary stops the subscription until you
remove it.

### 5. Build the graph, and refresh it on a schedule

The graph is defined over the local copies, as over any tables:

```sql
-- On the graph server: the graph
SELECT warren.create_graph('social', 'SELECT id FROM people', 'SELECT src, dst FROM follows');
SELECT warren.refresh('social');
SELECT count(*) FROM warren.bfs('social', ARRAY[1::bigint], 3);
```

A graph does not follow its tables by itself: each `warren.refresh` builds
a new version from them ([semantics.md](semantics.md#staleness-the-version-versus-the-tables)).
Refresh on a schedule. On a server you run, you can install
[pg_cron](https://github.com/citusdata/pg_cron) and schedule it in the
database:

```sql
-- On the graph server, with pg_cron installed: refresh every five minutes
SELECT cron.schedule('refresh social', '*/5 * * * *', $$SELECT warren.refresh('social')$$);
```

Or use any scheduler you already have (cron, a systemd timer, a Kubernetes
CronJob):

```sh
psql "host=graph.internal.example.com dbname=graph" -c "SELECT warren.refresh('social')"
```

Each refresh rebuilds the whole graph (pg_warren has no incremental
maintenance yet, [limits.md](limits.md)), so choose the interval by how long
a refresh takes and how stale an answer may be. Older versions are deleted
as `retain_versions` says ([semantics.md](semantics.md#retention)).

### How stale, and how consistent

A refresh reads the node and the edge query under one snapshot of the
graph server's tables. The subscription applies the primary's transactions
whole and in the order they committed. So each version is a consistent
picture of the primary at one moment: an edge and its two nodes, inserted
in one transaction on the primary, appear in a version together or not at
all. That holds once the first copy has finished, and only for tables in
the same subscription; keep the node and edge tables of a graph in one.

That moment is in the past. A version is behind the primary by the
replication lag at its refresh, plus the time since. `info() ->>
'staleness'` gives the time since the refresh. Its `built_lsn` is a
position in the graph server's WAL, not the primary's, so do not compare
it with the primary's. The lag is measured on the primary, as the amount of
WAL the primary has inserted that the graph server has not yet confirmed.
The graph server confirms a position only once it has applied and
committed every transaction before it:

```sql
-- On the managed primary: how far behind the graph server is
SELECT slot_name, pg_size_pretty(pg_wal_lsn_diff(pg_current_wal_insert_lsn(), confirmed_flush_lsn)) AS behind
FROM pg_replication_slots WHERE slot_name = 'graph_source';
```

On RDS, CloudWatch shows much the same as `OldestReplicationSlotLag`.

To know that a change has reached the graph server, for example before a
refresh that must include it: once it has committed, take
`pg_current_wal_insert_lsn()` on the primary, and wait until the slot's
`confirmed_flush_lsn` is at or past that position. A refresh that starts
after that includes the change. Take the insert position, not
`pg_current_wal_lsn()`: that is how far the WAL has been written, and a
transaction committed with `synchronous_commit = off` returns before its
WAL is written, so the write position can be behind its commit for a
while (up to three times `wal_writer_delay`).

## Reading results on the primary

`postgres_fdw` is available on RDS, Aurora and most managed services. On
the primary, a foreign table reads a table on the graph server as if it
were local. A foreign table cannot call a function with arguments, so put
whole-graph results in ordinary tables on the graph server, and read those.

On the graph server, a table of results, a function that refreshes the
graph and rewrites the table in one transaction, and a role that may read
only the results:

```sql
-- On the graph server: results for the primary to read
CREATE SCHEMA graph_results;
CREATE TABLE graph_results.rank (id bigint PRIMARY KEY, score float8 NOT NULL);
CREATE FUNCTION graph_results.rebuild() RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    PERFORM warren.refresh('social');
    PERFORM warren.pagerank('social', 'pr');
    DELETE FROM graph_results.rank;
    INSERT INTO graph_results.rank SELECT node_id, value FROM warren.vector_values('social', 'pr', NULL::bigint);
    PERFORM warren.drop_vector('social', 'pr');
END $$;
SELECT graph_results.rebuild();
CREATE ROLE graph_reader LOGIN PASSWORD :'graph_reader_password';
GRANT USAGE ON SCHEMA graph_results TO graph_reader;
GRANT SELECT ON graph_results.rank TO graph_reader;
```

Schedule `SELECT graph_results.rebuild()` in place of the bare refresh
above. Readers see the old ranks or the new, never a mix. The function
drops the vector once it is copied: a version with a persisted vector is
kept past `retain_versions`, so a vector left on each refreshed version
would keep every version.

On the primary, the foreign server, a user mapping for the application's
role (`app` here), and the foreign table:

```sql
-- On the managed primary: the results, through postgres_fdw
CREATE EXTENSION postgres_fdw;
CREATE SERVER graph_server FOREIGN DATA WRAPPER postgres_fdw
    OPTIONS (host :'graph_host', port '5432', dbname 'graph', sslmode 'verify-full');
CREATE USER MAPPING FOR app SERVER graph_server
    OPTIONS (user 'graph_reader', password :'graph_reader_password');
CREATE SCHEMA graph_results;
CREATE FOREIGN TABLE graph_results.rank (id bigint, score float8)
    SERVER graph_server OPTIONS (schema_name 'graph_results', table_name 'rank');
GRANT USAGE ON SCHEMA graph_results TO app;
GRANT SELECT ON graph_results.rank TO app;
```

The application then reads the results on the primary:

```sql
-- On the managed primary, as app: the ten highest ranks
SELECT id, score FROM graph_results.rank ORDER BY score DESC LIMIT 10;
```

What it costs:

- **Every query is a round trip** to the graph server: a remote
  connection's first query also connects (`postgres_fdw` keeps the
  connection for the session).
- **Simple filters, `ORDER BY` and `LIMIT` run on the graph server**: the
  query above fetches ten rows, and `WHERE id = 42` one. `EXPLAIN VERBOSE`
  shows what was sent, as `Remote SQL`.
- **A join with a local table does not run remotely.** `postgres_fdw` then
  fetches the remote rows it needs, which can be the whole table, 100 rows
  per round trip by default (raise `fetch_size` on the server or the
  table). For a large result table joined often, copy it instead (a
  scheduled `INSERT ... SELECT` into a local table on the primary).

## Pattern 2: the sources through postgres_fdw

`postgres_fdw` also works the other way, for the graph's sources: the
graph server reads the primary's tables at each refresh, with no
replication. On the primary, a role that may only read the two tables:

```sql
-- On the managed primary: a role that may read the source tables
CREATE ROLE warren_source LOGIN PASSWORD :'source_password';
GRANT SELECT ON people, follows TO warren_source;
```

On the graph server, the foreign tables and a graph over them:

```sql
-- On the graph server: sources read from the primary at each refresh
CREATE EXTENSION postgres_fdw;
CREATE SERVER app_primary FOREIGN DATA WRAPPER postgres_fdw
    OPTIONS (host :'primary_host', port '5432', dbname 'app', sslmode 'verify-full', fetch_size '10000');
CREATE USER MAPPING FOR CURRENT_USER SERVER app_primary
    OPTIONS (user 'warren_source', password :'source_password');
CREATE SCHEMA app_remote;
IMPORT FOREIGN SCHEMA public LIMIT TO (people, follows) FROM SERVER app_primary INTO app_remote;
SELECT warren.create_graph('social_remote', 'SELECT id FROM app_remote.people',
                           'SELECT src, dst FROM app_remote.follows');
SELECT warren.refresh('social_remote');
```

Each refresh then reads both tables whole over the network, as the graph's
owner (the user mapping is that role's). Only the columns the queries name
are sent, and a `WHERE` of built-in operators runs on the primary. Both
queries see one snapshot of the primary: `postgres_fdw` reads through one
remote `REPEATABLE READ` transaction per local transaction. That
transaction stays open on the primary for the whole refresh, which holds
back vacuum there while it runs.

## Security

- **Least privilege on the primary.** The replication role reads only the
  published tables (`GRANT SELECT` on those, and `rds_replication`); the
  `postgres_fdw` source role only `SELECT`s them. Neither owns anything.
- **Least privilege on the graph server.** The role the primary reads
  results as has `SELECT` on the result tables alone, and no access to
  any graph. Create graphs as a role that is not a superuser
  ([semantics.md](semantics.md#access)).
- **Encrypt and verify.** Use `sslmode=verify-full` in both directions,
  with the CA that signed the other server's certificate (`sslrootcert`;
  on PostgreSQL 16 and later, `sslrootcert=system` trusts the operating
  system's CAs). On RDS, set `rds.force_ssl` to `1` to refuse unencrypted
  connections. The graph server's `pg_hba.conf` should accept the
  primary's connections with `hostssl` and `scram-sha-256` only.
- **Passwords.** A subscription's connection string is stored in
  `pg_subscription`, readable by superusers; prefer the `.pgpass` file. A
  user mapping's password is stored on the primary and shown only to its
  user and the server's owner.
- **Network.** Logical replication connects from the graph server to the
  primary, and `postgres_fdw` from the primary to the graph server: the
  security groups (or firewall) must allow each direction you use, on the
  database port, and nothing else.

## Limitations and operations

- **The replication slot holds WAL on the primary.** The subscription
  keeps a slot on the primary, and while the graph server is down or
  behind, the primary keeps all WAL the graph server has not confirmed,
  which can fill its storage. Watch the lag (above), and set
  `max_slot_wal_keep_size` on the primary to cap it: past the cap the slot
  is invalidated, and the subscription must be recreated (it copies the
  tables again). Drop the subscription before you retire the graph server
  (`DROP SUBSCRIPTION` drops the slot too).
- **Failover of the primary.** Whether a logical slot survives a failover
  depends on the service and the PostgreSQL major (failover slots arrived
  in PostgreSQL 17). If the slot is lost, recreate the subscription.
- **Sequences, DDL and large objects are not replicated**, nor tables
  missing from the publication. Add a table with `ALTER PUBLICATION ... ADD
  TABLE` on the primary and `ALTER SUBSCRIPTION ... REFRESH PUBLICATION` on
  the graph server. The new table is copied first, on its own, so wait
  for that copy (step 4) before a refresh that reads it.
- **Losing the graph server loses nothing you cannot rebuild.** Its
  tables come from the primary and its graphs from them. Back it up if a
  rebuild would take too long, or to keep old versions and vectors.
  Otherwise, set up a new one, subscribe, and refresh.
- **The graph server needs its own sizing**: memory for the graphs it
  loads ([limits.md](limits.md)), and disk for the copied tables plus the
  versions retained.
