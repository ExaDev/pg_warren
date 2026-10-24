"""bench/corpus/keepset.py and fetch.py: fetching a dataset through the
pinned keepset CLI, checking it against its pin, and keeping only its tables."""

import json
import subprocess

import pytest

from corpus import fetch, keepset
from corpus.manifest import Dataset

SHA = "ab" * 32


def dataset(pin=None, **fields):
    base = dict(name="karate", tier="tiny", domain="social", ids="integer", directed=False, weighted=False,
                relations=False, nodes=2, edges=1, licence="", why="", pin=pin)
    return Dataset(**{**base, **fields})


class FakeKeepset:
    """Writes what `keepset fetch` writes: the document and its provenance."""

    def __init__(self, sha=SHA):
        self.sha, self.fetched = sha, []

    def fetch(self, name, out, heap_mb):
        self.fetched.append((name, heap_mb))
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(json.dumps({"meta": {"directed": False, "weighted": False},
                                   "nodes": [{"id": "1"}, {"id": "2"}], "edges": [{"source": "1", "target": "2"}]}))
        provenance = {"entryId": name, "fetchedUrl": "https://example.org/k.zip", "sha256": self.sha,
                      "byteLength": 970, "catalogue": {"id": "keepset", "version": "1.33.0"}}
        out.with_suffix(".provenance.json").write_text(json.dumps(provenance))
        return provenance


def test_a_first_fetch_records_its_pin_and_keeps_only_the_tables(tmp_path):
    recorded = {}
    directory = fetch.fetch(dataset(), FakeKeepset(), tmp_path, record=recorded.__setitem__)
    assert recorded == {"karate": {"sha256": SHA, "bytes": 970, "url": "https://example.org/k.zip"}}
    assert sorted(p.name for p in directory.iterdir()) == ["dataset.json", "edges.csv", "load.sql", "nodes.csv"]
    described = json.loads((directory / "dataset.json").read_text())
    assert described["tables"] == {"nodes": 2, "edges": 1, "ids": "integer", "relations": 0, "self_loops": 0}
    assert described["provenance"]["sha256"] == SHA


def test_a_pinned_fetch_whose_bytes_differ_is_refused_and_leaves_nothing(tmp_path):
    pin = {"sha256": "cd" * 32, "bytes": 970, "url": "https://example.org/k.zip"}
    with pytest.raises(fetch.PinMismatch, match="karate: keepset fetched ab"):
        fetch.fetch(dataset(pin), FakeKeepset(), tmp_path, record=None)
    assert not (tmp_path / "datasets" / "karate" / "dataset.json").exists()


def test_a_dataset_already_fetched_at_its_pin_is_not_fetched_again(tmp_path):
    pin = {"sha256": SHA, "bytes": 970, "url": "https://example.org/k.zip"}
    keepset_ = FakeKeepset()
    fetch.fetch(dataset(pin), keepset_, tmp_path, record=None)
    fetch.fetch(dataset(pin), keepset_, tmp_path, record=None)
    assert len(keepset_.fetched) == 1


def test_the_heap_keepset_is_given_grows_with_the_edges():
    # keepset holds the whole document and copies its edges (2.0 kB an edge
    # measured on roadnet-tx); Node's default heap fails at about a million.
    assert keepset.heap_mb(78) == 2048
    assert keepset.heap_mb(3_843_320) == 9_095


def test_the_cli_is_installed_from_the_committed_lock_and_run_without_archive_requests(tmp_path):
    calls = []

    def run(command, **options):
        calls.append((command, options))
        if command[:2] == ["npm", "ci"]:
            (tmp_path / "keepset-0.57.0" / "node_modules" / ".bin").mkdir(parents=True)
        return subprocess.CompletedProcess(command, 0, "", "")

    cli = keepset.Keepset(tmp_path, "0.57.0", run=run)
    out = tmp_path / "raw" / "karate.json"
    out.parent.mkdir()
    out.with_suffix(".provenance.json").write_text('{"sha256": "x"}')
    assert cli.fetch("karate", out, heap_mb=4096) == {"sha256": "x"}
    install, fetched = calls
    assert install[0] == ["npm", "ci", "--no-audit", "--no-fund", "--ignore-scripts"]
    assert (tmp_path / "keepset-0.57.0" / "package-lock.json").exists()
    assert fetched[0][1:] == ["fetch", "karate", "--out", str(out), "--no-archive"]
    assert fetched[1]["env"]["NODE_OPTIONS"] == "--max-old-space-size=4096"
