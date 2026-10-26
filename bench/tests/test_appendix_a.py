"""bench/appendix_a.py: Appendix A's use cases at scale, the runner that
times their stages and evaluates their checks, and each use case's plan."""

import pytest

import appendix_a
from use_cases import CASES
from use_cases.plan import Step, UseCase, at_least, at_most, between, equals, resolve


class FakeSession:
    """Answers statements from a script: a statement's first row, or an error."""

    def __init__(self, answers=None, errors=None):
        self.answers, self.errors, self.ran = answers or {}, errors or {}, []

    def _result(self, sql):
        self.ran.append(sql)
        for fragment, error in self.errors.items():
            if fragment in sql:
                return {"rows": [], "seconds": 0.5, "errors": [f"ERROR:  {error}"], "notices": [], "warnings": []}
        rows = next((v if isinstance(v, list) else [v] for f, v in self.answers.items() if f in sql), [])
        return {"rows": rows, "seconds": 0.5, "errors": [], "notices": [], "warnings": []}

    def run(self, sql):
        return self._result(sql)

    def measure(self, sql, repeat=1):
        runs = [self._result(sql) for _ in range(repeat)]
        stage = {"seconds": [r["seconds"] for r in runs], "median_seconds": 0.5, "first_rows": [
            r["rows"][0] if r["rows"] else None for r in runs], "peak_rss_anon": 100, "vm_hwm": 200}
        errors = [e for r in runs for e in r["errors"]]
        if errors:
            stage["errors"] = errors
        return stage


def run(steps, session=None, free=10 << 30, floor=1 << 30):
    case = UseCase("x", "A.0", {"nodes": 1}, tuple(steps))
    return appendix_a.run_case(case, session or FakeSession(), lambda: free, floor)


def test_resolve_substitutes_recorded_values_and_refuses_unknown_ones():
    assert resolve("SELECT @@base@@ < @@staged@@", {"base": "3", "staged": "7"}) == "SELECT 3 < 7"
    with pytest.raises(KeyError, match="staged"):
        resolve("SELECT @@staged@@", {})


def test_expectations_read_psql_text():
    assert at_least(0.9)("0.95") and not at_least(0.9)("0.85")
    assert at_most(500)("500") and not at_most(500)("501")
    assert between(30, 45)("40") and not between(30, 45)("46")
    assert equals("0")("0") and not equals("0")("1")
    assert not at_least(1)("") and not at_least(1)(None)


def test_a_stage_is_timed_with_its_peak_and_its_target():
    result = run([Step("refresh", "SELECT warren.refresh('g')", target=1.0),
                  Step("slow", "SELECT 2", target=0.1)])
    stages = {s["name"]: s for s in result["steps"]}
    assert stages["refresh"]["median_seconds"] == 0.5 and stages["refresh"]["peak_rss_anon"] == 100
    assert stages["refresh"]["met"] is True and stages["slow"]["met"] is False
    assert [v["stage"] for v in result["verdicts"]] == ["refresh", "slow"]


def test_a_measured_value_feeds_later_statements():
    session = FakeSession({"refresh": "12"})
    run([Step("base", "SELECT warren.refresh('g')", kind="measure", key="base"),
         Step("use", "SELECT @@base@@")], session)
    assert session.ran[-1] == "SELECT 12"


def test_checks_record_value_and_verdict_and_the_case_goes_on():
    session = FakeSession({"max(accounts)": "40", "precision": "0.5"})
    result = run([Step("small rings", "SELECT max(accounts)", kind="check", expect=at_most(500), claim="≤ 500"),
                  Step("precise", "SELECT precision", kind="check", expect=at_least(0.98), claim="≥ 0.98"),
                  Step("after", "SELECT 1")], session)
    checks = {c["name"]: c for c in result["checks"]}
    assert checks["small rings"] == {"name": "small rings", "claim": "≤ 500", "value": "40", "passed": True}
    assert checks["precise"]["passed"] is False
    assert result["passed"] == 1 and result["failed"] == 1
    assert "aborted" not in result and session.ran[-1] == "SELECT 1"


def test_a_failing_stage_rolls_back_and_ends_the_case():
    session = FakeSession(errors={"boom": "53400: out of memory"})
    result = run([Step("first", "SELECT boom"), Step("never", "SELECT 2")], session)
    assert "53400" in result["aborted"] and session.ran == ["SELECT boom", "ROLLBACK"]
    assert [s["name"] for s in result["steps"]] == ["first"]


def test_checks_and_measures_read_the_last_statement_s_row():
    session = FakeSession({"two statements": ["", "53400"]})
    result = run([Step("m", "SELECT two statements", kind="measure", key="m"),
                  Step("c", "SELECT two statements", kind="check", expect=equals("53400"), claim="x")], session)
    assert result["values"]["m"] == "53400" and result["checks"][0]["passed"] is True


def test_a_check_that_errors_fails_without_ending_the_case():
    session = FakeSession(errors={"broken": "42883: no such function"})
    result = run([Step("c", "SELECT broken", kind="check", expect=equals("0"), claim="x"),
                  Step("next", "SELECT 2")], session)
    assert result["checks"][0]["passed"] is False and "42883" in result["checks"][0]["error"]
    assert "aborted" not in result and session.ran == ["SELECT broken", "ROLLBACK", "SELECT 2"]


def test_an_allowed_error_is_recorded_and_the_case_goes_on():
    session = FakeSession(errors={"cte": "canceling statement due to statement timeout"})
    result = run([Step("cte", "SELECT cte", allowed_error="statement timeout"), Step("next", "SELECT 2")], session)
    assert result["steps"][0]["allowed_error"] and "aborted" not in result
    assert session.ran[-1] == "SELECT 2"


def test_the_case_stops_before_a_step_when_the_disk_is_low():
    session = FakeSession()
    result = run([Step("first", "SELECT 1")], session, free=500 << 20, floor=1 << 30)
    assert "disk" in result["aborted"] and session.ran == []


def test_repeated_stages_keep_every_timing():
    result = run([Step("walk warm", "SELECT 1", repeat=3)])
    assert result["steps"][0]["seconds"] == [0.5, 0.5, 0.5]


@pytest.mark.parametrize("name", sorted(CASES))
def test_every_use_case_is_about_10m_nodes_at_full_scale(name):
    case = CASES[name](1)
    assert 9_500_000 <= case.sizes["nodes"] <= 10_500_000, case.sizes


@pytest.mark.parametrize("name", sorted(CASES))
def test_every_use_case_scales_down_for_a_quick_check(name):
    full, small = CASES[name](1), CASES[name](1000)
    assert small.sizes["nodes"] < full.sizes["nodes"] / 500


@pytest.mark.parametrize("name", sorted(CASES))
def test_every_step_is_named_once_and_every_check_states_its_claim(name):
    case = CASES[name](1)
    names = [s.name for s in case.steps]
    assert len(names) == len(set(names))
    checks = [s for s in case.steps if s.kind == "check"]
    assert checks and all(s.claim and s.expect for s in checks)
    keys = {s.key for s in case.steps if s.key}
    for step in case.steps:
        for used in step.uses():
            assert used in keys, f"{step.name} uses @@{used}@@, which no step records"


@pytest.mark.parametrize("name", sorted(CASES))
def test_every_value_is_recorded_before_it_is_used(name):
    recorded = set()
    for step in CASES[name](1).steps:
        assert step.uses() <= recorded, f"{step.name} uses {step.uses() - recorded} before it is recorded"
        if step.key:
            recorded.add(step.key)


def test_the_use_cases_are_appendix_a1_to_a5():
    assert {CASES[n](1).section for n in CASES} == {"A.1", "A.2", "A.3", "A.4", "A.5"}
