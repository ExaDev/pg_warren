#!/usr/bin/env python3
"""Computes a NetworkX reference result for a graph in a database and writes
it as a table (see harness/oracle.py for computations and tables). Connects
with libpq's PG* environment variables, like psql.

    PGHOST=/tmp/... PGDATABASE=test_x test/oracle.py components --edges-sql 'SELECT a, b FROM links'
"""

import argparse
import os
import sys

from harness import oracle
from harness.database import Database


def main(argv):
    options = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    options.add_argument("computation", choices=sorted(oracle.TABLES))
    options.add_argument("--psql", default=os.environ.get("PSQL", "psql"))
    args, rest = options.parse_known_args(argv)
    oracle.run(Database(args.psql), args.computation, rest)


if __name__ == "__main__":
    main(sys.argv[1:])
