#!/usr/bin/env python3
"""pg_warren beside a primary it is not installed on (DESIGN.md §13, 0.9.0
"Running beside RDS today"; docs/beside-managed.md): installs pg_warren
(unless --no-install) and starts two scratch clusters, a primary with
`wal_level = logical` where pg_warren is never created, and a graph
server. Runs the guide's SQL blocks as written, in order, on the server
each names: the tables on both, a publication on the primary and a
subscription on the graph server, a graph over the subscribed tables,
results read back on the primary through `postgres_fdw`, and a second
graph whose sources are the primary's tables through `postgres_fdw`.
Then checks, before and after the primary's rows change and the graph
server catches up and refreshes, that the tables match, and that both
graphs answer traversals and PageRank as a graph built directly from the
primary's rows, read through the foreign table too; that `postgres_fdw`
sends the guide's filters, order and limit to the graph server; and the
server logs (harness/logical_replication.py). Holds /tmp/pgrx-test.lock
for the whole run."""

import argparse
import os
import sys

from harness import extension, guide, server_log
from harness import logical_replication as lr
from harness.cluster import Cluster
from harness.session import Session

GUIDE = extension.REPOSITORY / "docs" / "beside-managed.md"
# The blocks CI does not run, and why.
NOT_RUN = {
    "-- On the graph server, with pg_cron installed: refresh every five minutes":
        "CI has no pg_cron; the check runs the refresh it schedules",
}


def main(argv):
    options = argparse.ArgumentParser(description=__doc__)
    options.add_argument("--pg-config", default=os.environ.get("PG_CONFIG", "pg_config"))
    options.add_argument("--no-install", action="store_true", help="use the pg_warren already installed")
    options.add_argument("--runas", default="postgres", help="cluster user when run as root (default postgres)")
    options.add_argument("--keep", action="store_true", help="keep the cluster directories for inspection")
    args = options.parse_args(argv)
    with extension.shared_lock():
        if not args.no_install:
            extension.install(args.pg_config)
        failures = run(args)
    for failure in failures:
        print(f"  {failure}")
    print(f"beside a primary without pg_warren: {'FAILED' if failures else 'ok'}", flush=True)
    return 1 if failures else 0


def run(args):
    failures = []
    with Cluster(args.pg_config, args.runas, args.keep, settings=(*server_log.SETTINGS, *lr.PRIMARY_SETTINGS)) \
            as primary, Cluster(args.pg_config, args.runas, args.keep, settings=server_log.SETTINGS) as graph:
        try:
            failures += Sidecar(primary, graph).check()
        finally:
            # Read before teardown deletes the logs, even when a check raised.
            for name, server in (("primary", primary), ("graph server", graph)):
                failures += [f"{name} {problem}" for problem in lr.log_problems(server.log.read_text())]
            if sys.exc_info()[0] is not None:
                print("\n".join(failures), file=sys.stderr, flush=True)
    return failures


class Sidecar:
    def __init__(self, primary, graph):
        self.primary, self.graph = primary, graph
        self.variables = lr.variables(primary, graph)
        self.databases = {"primary": lr.PRIMARY_DATABASE, "graph": lr.GRAPH_DATABASE}
        self.servers = {"primary": primary, "graph": graph}
        self.primary_db = primary.database(lr.PRIMARY_DATABASE)
        self.graph_db = graph.database(lr.GRAPH_DATABASE)
        self.reference_db = graph.database(lr.REFERENCE_DATABASE)
        self.outputs = {}

    def check(self):
        self.primary.database().execute(f"CREATE DATABASE {lr.PRIMARY_DATABASE}")
        for name in (lr.GRAPH_DATABASE, lr.REFERENCE_DATABASE):
            self.graph.database().execute(f"CREATE DATABASE {name}")
        self.reference_db.execute(lr.REFERENCE_SETUP)
        self.run_guide()
        failures = self.guide_outputs()
        failures += self.compare("after the first copy")
        for statement in lr.changes(lr.initial_rows()[1]):
            self.primary_db.execute(statement)
        self.catch_up()
        self.graph_db.execute("SELECT graph_results.rebuild(); SELECT warren.refresh('social_remote')")
        failures += self.compare("after the primary's rows changed")
        failures += self.pushdown()
        failures += self.primary_lacks_pg_warren()
        return failures

    def run_guide(self):
        """Every block of the guide in order, on the servers it names, with
        what the scratch servers need around them."""
        for block in guide.blocks(GUIDE.read_text()):
            if block.key in NOT_RUN:
                print(f"skip {block.key} ({NOT_RUN[block.key]})", flush=True)
                continue
            self.before(block.key)
            for server in block.servers:
                self.outputs[block.key] = guide.run(self.servers[server], self.databases[server], block,
                                                   self.variables)
            self.after(block)
            print(f"ok   {block.key}", flush=True)

    def before(self, key):
        if key == "-- On the managed primary: a role for the graph server, and the publication":
            self.primary_db.execute(lr.STAND_INS)

    def after(self, block):
        if block.key == "-- On the managed primary: a role for the graph server, and the publication":
            self.primary_db.execute(lr.REPLICATION_ATTRIBUTE)
            nodes, edges = lr.initial_rows()
            self.primary_db.copy_in("people", nodes)
            self.primary_db.copy_in("follows", edges)
        elif block.key == "-- On the graph server: has the first copy finished?":
            lr.wait_until(lambda: guide.run(self.graph, lr.GRAPH_DATABASE, block, self.variables).strip() == "t",
                          "the subscription's first copy")
            self.outputs[block.key] = "t\n"
        elif block.key == "-- On the graph server: results for the primary to read":
            self.ask_reader_for_password()

    def ask_reader_for_password(self):
        hba = self.graph.directory / "data" / "pg_hba.conf"
        hba.write_text(lr.READER_HBA + hba.read_text())
        self.graph_db.execute("SELECT pg_reload_conf()")

    def guide_outputs(self):
        """What the guide's queries answered: the graph server behind by a
        known amount, and the ten highest ranks read on the primary."""
        failures = []
        behind = self.outputs["-- On the managed primary: how far behind the graph server is"].split("\n")[0]
        if not behind.startswith(f"{lr.SLOT}|"):
            failures.append(f"the guide's lag query answered {behind!r}")
        ranks = lr.ranks_by_node(self.graph_db.query("SELECT id, score FROM graph_results.rank"))
        top = [line.split("|") for line in
               self.outputs["-- On the managed primary, as app: the ten highest ranks"].splitlines()]
        return failures + lr.top_ranks_differences(top, ranks)

    def catch_up(self):
        lsn = lr.committed(self.primary_db)
        lr.wait_until(lambda: lr.caught_up(self.primary_db, lsn), f"the graph server confirming {lsn}")

    def compare(self, when):
        """The tables, and both graphs' answers and ranks, against the
        reference built from the primary's rows."""
        failures = lr.table_differences(self.primary_db, self.graph_db)
        lr.load_reference(self.primary_db, self.reference_db)
        expected, ranks = lr.traversals(self.reference_db, "social"), lr.pagerank(self.reference_db, "social")
        for graph in ("social", "social_remote"):
            failures += lr.traversal_differences(expected, lr.traversals(self.graph_db, graph), graph)
            failures += lr.rank_differences(ranks, lr.pagerank(self.graph_db, graph), graph)
        failures += lr.rank_differences(ranks, lr.ranks_by_node(self.graph_db.query(
            "SELECT id, score FROM graph_results.rank")), "graph_results.rank")
        through_fdw = self.primary.database(lr.PRIMARY_DATABASE, user="app").query(
            "SELECT id, score FROM graph_results.rank")
        failures += lr.rank_differences(ranks, lr.ranks_by_node(through_fdw), "the foreign table, as app")
        print(f"{'ok  ' if not failures else 'FAIL'} {when}: tables, traversals and ranks as the reference's",
              flush=True)
        return [f"{when}: {failure}" for failure in failures]

    def pushdown(self):
        failures = []
        with Session(self.primary, lr.PRIMARY_DATABASE, user="app") as app:
            for sql, clauses in lr.PUSHED_DOWN.items():
                plan = app.run(f"EXPLAIN VERBOSE {sql}")
                failures += lr.pushdown_differences(sql, plan.rows, clauses) if plan.ok \
                    else [f"EXPLAIN VERBOSE {sql}: {plan.sqlstate} {plan.message}"]
        print(f"{'ok  ' if not failures else 'FAIL'} postgres_fdw sends the filter, order and limit", flush=True)
        return failures

    def primary_lacks_pg_warren(self):
        found = self.primary_db.query("SELECT count(*) FROM pg_extension WHERE extname = 'pg_warren'")
        return [] if found == [["0"]] else ["pg_warren was created on the primary"]


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
