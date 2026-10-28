# Build the example dataset

This setup creates 10,000 people and 49,985 directed follows. A ring connects
everyone; deterministic hash shortcuts give people varied incoming links.
It needs no external dataset, Python packages, or application code.

## Start PostgreSQL

If pg_warren is already installed, create a fresh database and skip to the
SQL below. Otherwise, start the published PostgreSQL 17 container:

```sh
docker run -d --name warren-examples --shm-size=1g \
  -e POSTGRES_PASSWORD=warren-local \
  -e POSTGRES_DB=warren_examples \
  -p 127.0.0.1:55432:5432 \
  ghcr.io/exadev/pg_warren:0.7.0-pg17
```

Wait until `docker exec warren-examples pg_isready -U postgres` reports
that PostgreSQL is accepting connections. Then open a SQL session:

```sh
docker exec -it warren-examples psql -U postgres -d warren_examples
```

A desktop database client can use host `localhost`, port `55432`, database
`warren_examples`, user `postgres`, and password `warren-local`. The image
runs PostgreSQL; it does not include a browser GUI. If that host port is
already occupied, choose another in the Docker command.

## Create the graph

Run this setup **once per database**, then run recipes in the same database.
The `warren_examples` schema and `example_social` graph keep the example
objects separate from application tables. Schema-qualified source queries
make the graph independent of the session's search path.

<!-- sql:setup.sql -->

The final result is `nodes = 10000`, `edges = 49985`. The first refresh
builds graph version 1. The graph's external id type is `bigint`, matching
`people.id`; id-returning calls therefore use `1::bigint` or `NULL::bigint`.

## Run a recipe

Choose an example from [the library](index.md). For a downloaded SQL script,
use a fresh database because the download already includes this setup:

```sh
docker exec -i warren-examples psql -v ON_ERROR_STOP=1 \
  -U postgres -d warren_examples < bfs.sql
```

Alternatively, paste the entire downloaded file into pgAdmin's Query Tool.
Run whole scripts so transient scratch vectors remain in the same session.
In pgAdmin, select the result set you want in Data Output; loop progress
appears under Messages.

Stop the example container with `docker stop warren-examples`; start it
again with `docker start warren-examples`. The stopped container keeps its
database. For installation and shared-memory sizing, see
[installation](../../README.md#install) and [limits](../limits.md).
