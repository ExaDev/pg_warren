"""What the pooler soak checks once its clients have stopped."""

from harness.pooler_soak.samples import OUTLIVED_PROGRESS_ROWS
from harness.pooler_soak.settled import after_clients
from harness.session import Outcome


class Answers:
    """Answers each statement with the value of the first key it contains, else 0."""

    def __init__(self, values):
        self.values = values

    def run(self, sql):
        value = next((value for key, value in self.values.items() if key in sql), "0")
        return Outcome([[value]], "00000", "")


def test_a_progress_row_left_once_the_clients_have_stopped_fails(monkeypatch):
    monkeypatch.setattr("harness.pooler_soak.settled.SETTLE_SECONDS", 0)
    failures, _ = after_clients(Answers({OUTLIVED_PROGRESS_ROWS: "2"}), {"transaction": "soak_transaction"})
    assert failures == ["2 progress rows outlived their call once the clients stopped"]
    assert after_clients(Answers({}), {"transaction": "soak_transaction"}) == ([], {"transaction": 0})
