"""How the pooler soak warms each pool's server backends before its clock starts."""

import itertools

import pytest

from harness.pooler_soak.warm import PASSES, warm
from harness.session import Outcome


class Backend:
    """A pooled session pinned to one server backend by its open transaction."""

    pids = itertools.count(100)

    def __init__(self, distinct=True):
        self.pid = str(next(self.pids)) if distinct else "99"
        self.ran, self.closed = [], False

    def run(self, sql):
        self.ran.append(sql)
        value = self.pid if sql == "SELECT pg_backend_pid()" else "7"
        return Outcome([[value]], "00000", "")

    def close(self):
        self.closed = True


def test_every_server_backend_runs_every_kind_of_call_in_its_own_transaction():
    backends = []

    def connect():
        backends.append(Backend())
        return backends[-1]

    warmed = warm(connect, 3, nodes=100, seed=1)
    assert warmed == {b.pid for b in backends} and len(backends) == 3
    for backend in backends:
        assert backend.ran[:2] == ["BEGIN", "SELECT pg_backend_pid()"] and backend.ran[-1] == "COMMIT"
        calls = " ".join(backend.ran)
        for call in ("warren.refresh('g')", "warren.refresh('t')", "promote := false", "warren.drop_version",
                     "warren.compact('g')", "warren.pagerank", "warren.spmv", "warren.progress()"):
            assert calls.count(call) == PASSES, call
        assert calls.count("warren.bfs('g'") == calls.count("warren.bfs('t'") == PASSES
        assert backend.closed


def test_a_pool_that_does_not_give_each_session_its_own_backend_is_refused():
    with pytest.raises(RuntimeError, match="2 server backends for 3 sessions"):
        warm(iter([Backend(), Backend(distinct=False), Backend(distinct=False)]).__next__, 3, nodes=100, seed=1)
