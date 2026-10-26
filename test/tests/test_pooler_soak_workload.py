"""The pooler soak's clients: what they call, and the failures they may see."""

import random

from harness.pooler_soak.fixture import edit_sql, role, setup_sql
from harness.pooler_soak.workload import READS, Tally, Workload, allowed, traversal_sql
from harness.session import Outcome


def test_a_cancel_or_a_statement_timeout_is_allowed_to_every_operation():
    for operation in ("traverse", "stats", "transient", "persist", "prepared", "introspect", "timeout", "write"):
        assert allowed(operation, "57014", "canceling statement due to user request")
        assert allowed(operation, "57014", "canceling statement due to statement timeout")


def test_a_retired_version_is_allowed_only_where_a_version_is_named():
    retired = 'graph "g" has no version 12'
    assert allowed("transient", "42704", retired)
    assert allowed("persist", "42704", retired)
    assert not allowed("traverse", "42704", retired)
    assert not allowed("introspect", "42704", retired)


def test_a_lock_timeout_is_allowed_only_to_writers():
    assert allowed("write", "55P03", "canceling statement due to lock timeout")
    assert not allowed("persist", "55P03", "canceling statement due to lock timeout")


def test_the_transient_cap_is_never_allowed():
    assert not allowed("transient", "53400", "transient vectors would take 10 MB past warren.max_cache_memory")


def test_traversals_read_both_graphs_by_their_own_id_types():
    for kind in READS:
        assert "ARRAY[17::bigint]" in traversal_sql("g", kind, 17, 3) or "17::bigint" in traversal_sql("g", kind, 17, 3)
        assert "'n17'" in traversal_sql("t", kind, 17, 3)
        assert "::bigint" not in traversal_sql("t", kind, 17, 3)


def test_a_tally_keeps_failures_that_are_not_allowed_by_pool_mode():
    tally = Tally()
    tally.count("session", "traverse", Outcome([["1"]], "00000", ""))
    tally.count("session", "traverse", Outcome([], "57014", "canceling statement due to user request"))
    tally.count("transaction", "transient", Outcome([], "53400", "too much"))
    assert tally.outcomes[("session", "traverse", "00000")] == 1
    assert tally.unexpected == [("transaction", "transient", "53400", "too much")]


def test_the_fixture_makes_a_login_role_per_pool_mode_in_the_owner_role():
    statements = setup_sql(100, ["transaction", "session"])
    assert "CREATE ROLE soak_transaction LOGIN IN ROLE soak_owner;" in statements[0]
    assert role("session") == "soak_session"
    assert any("create_graph('t'" in s for s in statements)


def test_an_edit_replaces_one_nodes_edges_and_toggles_a_node_above_half():
    sql = edit_sql(random.Random(3), 100)
    assert sql.startswith("DELETE FROM public.edges WHERE src = ")
    toggled = int(sql.split("DELETE FROM public.nodes WHERE id = ")[1].split()[0])
    assert 51 <= toggled <= 100


class ScriptedSession:
    """Answers each statement with the next outcome whose key it contains."""

    def __init__(self, answers):
        self.answers, self.ran = answers, []

    def run(self, sql):
        self.ran.append(sql)
        for key, outcomes in self.answers.items():
            if key in sql and outcomes:
                return outcomes.pop(0)
        return Outcome([["7"]], "00000", "")


def test_a_client_first_drops_its_vector_wherever_one_was_left_or_moved():
    """A cancel can keep a persist from its drop, and a compaction moves a
    vector to the compacted version: either keeps a version from retention."""
    workload = Workload(None, "transaction", "soak_transaction", 35, 0, 100, 1, Tally(), None, 0, 0)
    session = ScriptedSession({"(v ->> 'vectors')": [Outcome([["620"], ["623"]], "00000", "")],
                               "current_version": [Outcome([["625"]], "00000", "")]})
    workload._persist(session, random.Random(1), 34)
    assert session.ran[1:3] == ["SELECT warren.drop_vector('g', 'pt34', 620, if_exists := true)",
                                "SELECT warren.drop_vector('g', 'pt34', 623, if_exists := true)"]
    assert session.ran[-1] == "SELECT warren.drop_vector('g', 'pt34', 625, if_exists := true)"


def test_a_timeout_call_that_finishes_in_time_is_rolled_back_so_its_vector_keeps_no_version():
    workload = Workload(None, "session", "soak_session", 1, 0, 100, 1, Tally(), None, 0, 0)
    session = ScriptedSession({})
    workload._timeout(session, random.Random(1), 0)
    assert session.ran[-1] == "ROLLBACK"
    assert "COMMIT" not in session.ran
