"""What each pool mode keeps of a client's session state, and what the soak requires."""

from harness.pooler_soak.probes import PROBES, Probe, failures


def test_every_kind_of_session_state_is_probed():
    assert set(PROBES) == {"setting", "transient vector", "prepared statement", "last_stats", "left by a client"}


def test_session_pooling_must_keep_a_clients_state_and_show_it_to_no_other():
    kept = [Probe("session", name, carried=True, leaked=False, detail="") for name in PROBES]
    assert failures(kept) == []
    leaked = Probe("session", "left by a client", carried=None, leaked=True, detail="on backend 7")
    assert failures([leaked]) == ["session pooling: left by a client leaked to another client (on backend 7)"]
    lost = Probe("session", "setting", carried=False, leaked=False, detail="")
    assert failures([lost]) == ["session pooling: setting was lost between a client's transactions ()"]


def test_transaction_pooling_breaks_session_state_and_that_is_reported_not_failed():
    broken = [Probe("transaction", name, carried=False, leaked=True, detail="") for name in PROBES]
    assert failures(broken) == []


def test_a_probe_that_could_not_tell_is_not_a_failure():
    assert failures([Probe("session", "left by a client", carried=None, leaked=None, detail="")]) == []
