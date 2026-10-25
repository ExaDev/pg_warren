"""The fuzzer's oracle: which SQLSTATEs a call may fail with, and which
successes are breaches of the caller's privileges."""

import re

from harness.session import Outcome
from harness.sql_fuzz.fixture import ESCALATED, OWNER, READER, STRANGER
from harness.sql_fuzz.surface import Argument, Call, Function
from harness.sql_fuzz.verdict import LOG_EXPECTED, Finding, allowed, changes, judge, log_faults, signature
from harness.sql_fuzz.world import Graph, World

WORLD = World((Graph("g8", "bigint", OWNER, (READER,)),))
INFO = Function("info", (Argument("name", "text"),), 0, False, "jsonb")
REFRESH = Function("refresh", (Argument("name", "text"),), 0, False, "bigint")
FROM_QUERY = Function("vector_from_query", (Argument("graph", "text"), Argument("name", "text"),
                                            Argument("query", "text")), 0, False, "bigint")
OK = Outcome([["1"]], "00000", "")


def failed(sqlstate, message="m"):
    return Outcome([], sqlstate, message)


def test_documented_classes_are_allowed():
    for state in ("22023", "22004", "42704", "42501", "53400", "54000", "55000", "55P03", "0A000", "57014",
                  "25006", "40001", "39P03"):
        assert allowed(state), state


def test_internal_errors_corruption_integrity_violations_and_system_errors_are_not():
    for state in ("XX000", "XX001", "XX002", "23503", "23505", "58P01", "25P02", "57P01", ESCALATED, "P0001"):
        assert not allowed(state), state


def test_a_failure_with_an_unexpected_sqlstate_is_a_finding():
    found = judge("postgres", Call(INFO, ("'g8'",)), failed("XX000", "index out of bounds"), WORLD)
    assert found == Finding("sqlstate", "XX000: index out of bounds")


def test_an_operator_run_elevated_is_its_own_finding():
    assert judge(STRANGER, None, failed(ESCALATED, "ran as postgres"), WORLD).kind == "escalation"


def test_a_stranger_succeeding_on_a_graph_it_may_not_read_is_a_breach():
    assert judge(STRANGER, Call(INFO, ("'g8'",), graphs=("g8",)), OK, WORLD).kind == "access"
    assert judge(READER, Call(INFO, ("'g8'",), graphs=("g8",)), OK, WORLD) is None
    assert judge("postgres", Call(INFO, ("'g8'",), graphs=("g8",)), OK, WORLD) is None


def test_a_reader_succeeding_at_an_owners_operation_is_a_breach():
    assert judge(READER, Call(REFRESH, ("'g8'",), graphs=("g8",)), OK, WORLD).kind == "access"
    assert judge(OWNER, Call(REFRESH, ("'g8'",), graphs=("g8",)), OK, WORLD) is None


def test_a_non_superuser_query_of_the_secret_must_not_succeed():
    call = Call(FROM_QUERY, ("'g8'", "'v'", "'SELECT id, 1 FROM public.secret'"), graphs=("g8",),
                queries=("'SELECT id, 1 FROM public.secret'",))
    assert judge(OWNER, call, OK, WORLD).kind == "access"
    assert judge("postgres", call, OK, WORLD) is None


def test_a_signature_ignores_the_numbers_in_a_message():
    one = signature("info", Finding("sqlstate", "XX000: version 12 of graph g8 at 0x7f"))
    two = signature("info", Finding("sqlstate", "XX000: version 99 of graph g8 at 0x80"))
    assert one == two
    assert one != signature("refresh", Finding("sqlstate", "XX000: version 12 of graph g8 at 0x7f"))


def test_a_strict_function_given_a_null_ran_nothing_and_so_breached_nothing():
    strict = Function("refresh", (Argument("name", "text"), Argument("promote", "boolean")), 1, False, "bigint",
                      strict=True)
    assert judge(READER, Call(strict, ("'g8'",), (("promote", "NULL"),), graphs=("g8",)), OK, WORLD) is None
    assert judge(READER, Call(strict, ("'g8'",), graphs=("g8",)), OK, WORLD).kind == "access"


def test_anyone_may_evict():
    evict = Function("evict", (Argument("name", "text"),), 0, False, "bigint")
    assert judge(STRANGER, Call(evict, ("'g8'",), graphs=("g8",)), OK, WORLD) is None


def test_a_report_without_a_command_only_clears_the_callers_frame():
    report = Function("report_progress", (Argument("command", "text"), Argument("graph", "text"),
                                          Argument("phase", "text")), 0, False, "void")
    assert judge(STRANGER, Call(report, ("NULL", "'g8'", "'p'"), graphs=("g8",)), OK, WORLD) is None
    assert judge(STRANGER, Call(report, ("'c'", "'g8'", "'p'"), graphs=("g8",)), OK, WORLD).kind == "access"


def test_an_allowed_privilege_or_missing_object_error_about_warrens_own_objects_is_internal():
    call = Call(INFO, ("'g8'",))
    for state, message in (("42501", "permission denied for table segments"),
                           ("42501", "permission denied for schema warren"),
                           ("42501", "permission denied for sequence graphs_graph_id_seq"),
                           ("42501", "permission denied for function vector_map"),
                           ("42P01", 'relation "warren.versions" does not exist'),
                           ("42883", "function warren.vector_map(unknown, unknown) does not exist")):
        assert judge(OWNER, call, failed(state, message), WORLD) == Finding("internal", f"{state}: {message}")


def test_errors_about_the_callers_own_objects_or_call_are_not_internal():
    call = Call(INFO, ("'g8'",))
    for state, message in (("42501", "permission denied for table edges8"),
                           ("42501", 'must be a member of role "fuzz_owner" to refresh graph "g8"'),
                           ("42P01", 'relation "nosuch" does not exist'),
                           ("42883", "function warren.info(integer) does not exist")):
        assert judge(STRANGER, call, failed(state, message), WORLD) is None, message


def test_the_server_log_flags_internal_faults_and_expects_documented_warnings():
    prefix = "2026-10-25 22:00:00.000 UTC [42] "
    expected = ("WARNING:  pagerank did not converge in 20 iterations",
                "WARNING:  leiden did not stabilise in 100 iterations",
                "WARNING:  eigen did not converge in 25 steps; the largest residual is 1e-3",
                "WARNING:  the matrix has 2 connected components: eigenvalues repeat")
    for line in expected:
        assert any(re.search(pattern, prefix + "01000 " + line) for pattern in LOG_EXPECTED), line
    log = "\n".join((prefix + "42501 ERROR:  permission denied for table segments",
                     prefix + "42501 ERROR:  permission denied for table edges8",
                     prefix + '42P01 ERROR:  relation "warren.graphs" does not exist'))
    assert log_faults(log) == [log.splitlines()[0], log.splitlines()[2]]


BEFORE = World((Graph("g8", "bigint", OWNER, (READER,), current=3),),
               versions=(("g8", 2, False), ("g8", 3, False)),
               vectors=(("g8", "pr", 3, OWNER, "7:100"), ("g8", "mine", 3, READER, "8:101")))


def after(**changes):
    graph = Graph("g8", "bigint", changes.pop("owner", OWNER), changes.pop("readers", (READER,)),
                  current=changes.pop("current", 3))
    graphs = () if changes.pop("dropped", False) else (graph,)
    return World(graphs, versions=changes.pop("versions", BEFORE.versions),
                 vectors=changes.pop("vectors", BEFORE.vectors))


def test_a_non_owner_changing_a_graph_it_does_not_own_is_a_breach():
    for changed in (after(dropped=True), after(owner=STRANGER), after(readers=(READER, STRANGER)),
                    after(current=2), after(versions=(*BEFORE.versions, ("g8", 4, False))),
                    after(versions=(("g8", 3, False),))):
        for role in (READER, STRANGER):
            found = changes(role, BEFORE, changed)
            assert [f.kind for f in found] == ["access"], (role, changed, found)
            assert "g8" in found[0].detail


def test_the_owner_or_a_superuser_may_change_the_graph():
    for changed in (after(dropped=True), after(owner=READER), after(current=2), after(vectors=())):
        assert changes(OWNER, BEFORE, changed) == []
        assert changes("postgres", BEFORE, changed) == []


def test_a_reader_may_drop_or_replace_its_own_vector_but_not_anothers():
    own = after(vectors=(BEFORE.vectors[0], ("g8", "mine", 3, READER, "9:102")))
    assert changes(READER, BEFORE, own) == []
    assert changes(READER, BEFORE, after(vectors=(BEFORE.vectors[0],))) == []
    for changed in (after(vectors=(BEFORE.vectors[1],)),
                    after(vectors=(("g8", "pr", 3, READER, "9:102"), BEFORE.vectors[1])),
                    after(vectors=(("g8", "pr", 3, OWNER, "7:200"), BEFORE.vectors[1]))):
        found = changes(READER, BEFORE, changed)
        assert [f.kind for f in found] == ["access"] and "pr" in found[0].detail, found


def test_contracting_into_an_existing_graph_is_the_owners_alone():
    contract = Function("contract", (Argument("graph", "text"), Argument("labels", "text"),
                                     Argument("out_graph", "text")), 0, False, "bigint")
    world = World((Graph("g8", "bigint", OWNER, (READER,)), Graph("gq", "bigint", OWNER, (READER,))))
    into_gq = Call(contract, ("'g8'", "'lbl'", "'gq'"), graphs=("g8", "gq"))
    assert judge(READER, into_gq, OK, world).kind == "access"
    assert judge(OWNER, into_gq, OK, world) is None
    assert judge(READER, Call(contract, ("'g8'", "'lbl'", "'fz1'"), graphs=("g8",)), OK, world) is None
