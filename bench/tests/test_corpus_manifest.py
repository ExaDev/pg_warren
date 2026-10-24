"""bench/corpus/manifest.py: which datasets the suite has, and in which tier."""

import json

import pytest

from corpus import manifest

DATASET = {"tier": "tiny", "domain": "social", "ids": "integer", "directed": False, "weighted": False,
           "relations": False, "nodes": 34, "edges": 78, "licence": "free for scientific use", "why": "a classic"}


def write(tmp_path, datasets, pins=None):
    path = tmp_path / "manifest.json"
    path.write_text(json.dumps({"keepset": {"version": "0.57.0", "integrity": "sha512-x"}, "datasets": datasets}))
    (tmp_path / "pins.json").write_text(json.dumps(pins or {}))
    return path


def test_a_dataset_is_named_by_its_keepset_id_and_carries_its_pin(tmp_path):
    pin = {"sha256": "ab" * 32, "bytes": 970, "url": "https://example.org/karate.zip"}
    read = manifest.read(write(tmp_path, {"karate": DATASET}, {"karate": pin}))
    karate = read.dataset("karate")
    assert (karate.name, karate.tier, karate.ids, karate.directed) == ("karate", "tiny", "integer", False)
    assert karate.pin == pin
    assert read.keepset == {"version": "0.57.0", "integrity": "sha512-x"}


def test_a_dataset_fetched_for_the_first_time_has_no_pin(tmp_path):
    assert manifest.read(write(tmp_path, {"karate": DATASET})).dataset("karate").pin is None


def test_selection_by_tier_keeps_the_manifest_order(tmp_path):
    datasets = {"a": DATASET, "big": {**DATASET, "tier": "large"}, "b": {**DATASET, "tier": "medium"},
                "c": DATASET}
    read = manifest.read(write(tmp_path, datasets))
    assert [d.name for d in read.select(tiers=["tiny"])] == ["a", "c"]
    assert [d.name for d in read.select(tiers=["tiny", "medium"])] == ["a", "b", "c"]
    assert [d.name for d in read.select(names=["c", "big"])] == ["c", "big"]


def test_selecting_an_unknown_dataset_names_it(tmp_path):
    with pytest.raises(manifest.ManifestError, match="unknown dataset: nope"):
        manifest.read(write(tmp_path, {"a": DATASET})).select(names=["nope"])


@pytest.mark.parametrize("field, value", [("tier", "huge"), ("ids", "uuid")])
def test_an_unknown_tier_or_id_kind_is_refused(tmp_path, field, value):
    with pytest.raises(manifest.ManifestError, match=f"a: {field}"):
        manifest.read(write(tmp_path, {"a": {**DATASET, field: value}}))


def test_a_missing_field_is_refused(tmp_path):
    incomplete = {k: v for k, v in DATASET.items() if k != "directed"}
    with pytest.raises(manifest.ManifestError, match="a: missing directed"):
        manifest.read(write(tmp_path, {"a": incomplete}))


def test_a_pin_is_recorded_beside_the_manifest(tmp_path):
    path = write(tmp_path, {"a": DATASET})
    pin = {"sha256": "cd" * 32, "bytes": 5, "url": "https://example.org/a"}
    manifest.record_pin(path, "a", pin)
    assert manifest.read(path).dataset("a").pin == pin


def test_the_committed_manifest_spans_the_suites_axes():
    committed = manifest.read(manifest.MANIFEST)
    datasets = committed.select()
    assert {d.tier for d in datasets} == {"tiny", "medium", "large"}
    assert {d.ids for d in datasets} == {"integer", "text"}
    assert {d.directed for d in datasets} == {True, False}
    assert {d.weighted for d in datasets} == {True, False}
    assert {"social", "road", "citation", "knowledge graph"} <= {d.domain for d in datasets}
    assert all(d.pin for d in datasets if d.tier != "large"), "every fetched dataset is pinned"
