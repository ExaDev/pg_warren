"""The guide's blocks and where each runs, the source rows and their
changes, and the comparisons of the beside-a-managed-primary check."""

import re

import pytest

from harness import guide
from harness import logical_replication as lr
from logical_replication import GUIDE, NOT_RUN

HOOKED = re.compile(r'(?:block\.key|key) == "(-- On [^"]+)"')


def test_every_block_of_the_guide_says_where_it_runs_and_runs_or_is_known_not_to():
    blocks = guide.blocks(GUIDE.read_text())
    keys = [block.key for block in blocks]
    assert len(blocks) >= 10
    assert set(NOT_RUN) <= set(keys), "a block CI does not run is no longer in the guide"
    assert all(block.condition for block in blocks if block.key in NOT_RUN)
    assert not [block.key for block in blocks if block.condition and block.key not in NOT_RUN]


def test_the_check_names_only_blocks_the_guide_has():
    source = (GUIDE.parents[1] / "test" / "logical_replication.py").read_text()
    keys = {block.key for block in guide.blocks(GUIDE.read_text())}
    named = set(HOOKED.findall(source)) | set(re.findall(r'outputs\["(-- On [^"]+)"\]', source))
    assert named and named <= keys, sorted(named - keys)


def test_the_guide_uses_only_variables_the_check_sets():
    used = set(re.findall(r":'(\w+)'", "".join(b.sql for b in guide.blocks(GUIDE.read_text()))))
    assert used == set(lr.variables(Server("/p"), Server("/g")))


def test_a_block_says_on_which_servers_and_as_whom_it_runs():
    markdown = ("```sql\n-- On both servers: tables\nCREATE TABLE t ();\n```\n"
                "```sql\n-- On the managed primary, as app: a read\nSELECT 1;\n```\n"
                "```\nnot sql\n```\n"
                "```sql\n-- On the graph server, with pg_cron installed: a job\nSELECT 2;\n```\n")
    both, read, job = guide.blocks(markdown)
    assert both.servers == ("primary", "graph") and both.sql == "-- On both servers: tables\nCREATE TABLE t ();\n"
    assert read.servers == ("primary",) and read.user == "app" and read.condition is None
    assert job.servers == ("graph",) and job.condition == "with pg_cron installed"


@pytest.mark.parametrize("markdown", ["```sql\nSELECT 1;\n```\n",
                                      "```sql\n-- On the standby: x\nSELECT 1;\n```\n",
                                      "```sql\n-- On both servers: x\n```\n```sql\n-- On both servers: x\n```\n"])
def test_a_block_that_does_not_say_where_or_repeats_a_key_is_refused(markdown):
    with pytest.raises(ValueError):
        guide.blocks(markdown)


class Server:
    def __init__(self, directory):
        self.directory = directory


def test_the_replication_connection_is_to_the_primarys_socket_as_the_replication_role():
    found = lr.variables(Server("/p"), Server("/g"))
    assert found["primary"] == "host=/p port=5432 dbname=app user=warren_replica sslmode=verify-full"
    assert found["graph_host"] == "/g" and found["primary_host"] == "/p"


def test_the_first_rows_are_fixed_and_have_no_loop_or_parallel_edge():
    nodes, edges = lr.initial_rows()
    assert (nodes, edges) == lr.initial_rows()
    assert len(nodes) == lr.PEOPLE and len(edges) == len(set(edges)) == lr.FOLLOWS
    assert all(src != dst for src, dst in edges)
    ids = {i for i, _ in nodes}
    assert all(src in ids and dst in ids for src, dst in edges)


def test_the_changes_add_delete_and_move_rows_in_transactions_and_spare_the_starts():
    statements = lr.changes(lr.initial_rows()[1])
    assert len(statements) == 4
    assert statements[0].startswith("BEGIN; INSERT INTO people") and "INSERT INTO follows" in statements[0]
    assert statements[1].startswith("DELETE FROM follows")
    assert statements[2].count("UPDATE follows") == 20
    removed = re.search(r"DELETE FROM people WHERE id IN \(([^)]*)\)", statements[3]).group(1)
    assert not {int(i) for i in removed.split(", ")} & set(lr.STARTS)


def test_ranks_must_match_within_the_tolerance_on_the_same_nodes():
    expected = {1: 0.5, 2: 0.25, 3: 0.25}
    assert lr.rank_differences(expected, {1: 0.5 + 1e-15, 2: 0.25, 3: 0.25}, "g") == []
    assert lr.rank_differences(expected, {1: 0.5, 2: 0.25}, "g") == ["g: nodes [3] are not in both"]
    assert lr.rank_differences(expected, {1: 0.4, 2: 0.25, 3: 0.25}, "g") == \
        ["g: 1 ranks differ, node 1: 0.4 against 0.5"]


def test_the_ten_highest_ranks_are_checked_by_score_and_node():
    ranks = {i: 1 / i for i in range(1, 20)}
    top = [[str(i), repr(1 / i)] for i in range(1, 11)]
    assert lr.top_ranks_differences(top, ranks) == []
    assert lr.top_ranks_differences(top[:9], ranks)
    assert lr.top_ranks_differences([["2", repr(1.0)]] + top[1:], ranks)


def test_traversals_differ_by_key():
    expected = {"bfs from 1": [["1", "0"]], "counts": [["3", "2"]]}
    assert lr.traversal_differences(expected, dict(expected), "g") == []
    assert lr.traversal_differences(expected, {**expected, "counts": [["3", "1"]]}, "g") == \
        ["g: counts is [['3', '1']], the reference's [['3', '2']]"]


def test_the_remote_sql_of_a_plan_must_carry_the_clauses_the_guide_promises():
    plan = [["Foreign Scan on graph_results.rank  (cost=100.00..101.00 rows=10 width=16)"],
            ["  Output: id, score"],
            ["  Remote SQL: SELECT id, score FROM graph_results.rank ORDER BY score DESC NULLS FIRST LIMIT 10::bigint"]]
    assert lr.remote_sql(plan) == ["SELECT id, score FROM graph_results.rank ORDER BY score DESC NULLS FIRST "
                                   "LIMIT 10::bigint"]
    assert lr.pushdown_differences("q", plan, ("ORDER BY score DESC", "LIMIT 10")) == []
    assert lr.pushdown_differences("q", plan, ("WHERE ((id = 42))",)) == \
        [f"q: the remote SQL {lr.remote_sql(plan)} lacks 'WHERE ((id = 42))'"]


def test_any_error_or_warning_in_a_server_log_is_a_problem():
    line = "2026-10-29 10:00:00.001 UTC [7] 42501 ERROR:  permission denied"
    assert lr.log_problems(line) == [f"server log: {line}"]
    assert lr.log_problems("2026-10-29 10:00:00.001 UTC [7] 00000 LOG:  started") == []


class Primary:
    """A primary that has inserted the WAL of commits made with
    `synchronous_commit = off` but not yet written it, as the harness's
    clusters do: the write position is behind the insert position."""

    POSITIONS = {"pg_current_wal_insert_lsn()": "0/1BFED98", "pg_current_wal_lsn()": "0/1BE6E58"}

    def query(self, sql):
        return [[next(lsn for function, lsn in self.POSITIONS.items() if function in sql)]]


def test_the_wait_for_the_graph_server_targets_the_end_of_the_last_commit_not_the_written_wal():
    assert lr.committed(Primary()) == "0/1BFED98"


def test_the_guides_lag_counts_from_the_inserted_wal_not_the_written():
    lag = next(block.sql for block in guide.blocks(GUIDE.read_text())
               if block.key == "-- On the managed primary: how far behind the graph server is")
    assert "pg_current_wal_insert_lsn()" in lag and "pg_current_wal_lsn()" not in lag
