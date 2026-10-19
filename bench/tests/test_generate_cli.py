"""bench/generate.py: one subcommand per generator, writing a fixture directory."""

import csv
import hashlib
import json
from pathlib import Path

import pytest

import generate

SMALL = {
    "random": ["--nodes", "50", "--edges", "80"],
    "power-law": ["--nodes", "50", "--attachments", "2"],
    "grid": ["--rows", "4", "--cols", "5"],
    "sbm": ["--communities", "3", "--community-size", "20", "--embedding-dim", "4", "--relations", "a,b"],
    "evolving": ["--communities", "3", "--community-size", "20", "--new-entities", "5", "--new-edges", "10",
                 "--variants", "volume,novel"],
    "fraud": ["--honest-accounts", "200", "--rings", "2", "--hub-sizes", "20,10,10"],
    "entity-resolution": ["--records", "300", "--entities", "100"],
    "citation": ["--papers", "300", "--people", "50", "--institutions", "10", "--root-neighbourhood", "20"],
    "embedding": ["--communities", "3", "--community-size", "20"],
}


def digest(directory):
    return {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(Path(directory).iterdir())}


@pytest.mark.parametrize("kind", sorted(SMALL))
def test_every_generator_writes_a_loadable_fixture_reproducibly(kind, tmp_path):
    generate.main([kind, *SMALL[kind], "--seed", "4", "--out", str(tmp_path / "a")])
    generate.main([kind, *SMALL[kind], "--seed", "4", "--out", str(tmp_path / "b")])
    assert (tmp_path / "a" / "load.sql").exists()
    assert digest(tmp_path / "a") == digest(tmp_path / "b")


def test_parameters_reach_the_generator(tmp_path):
    generate.main(["random", "--nodes", "30", "--edges", "12", "--directed", "--seed", "2", "--out", str(tmp_path)])
    truth = json.loads((tmp_path / "truth.json").read_text())
    assert (truth["nodes"], truth["edges"], truth["directed"], truth["seed"]) == (30, 12, True, 2)


def test_the_embedding_preset_is_a_block_model_with_embeddings_and_relations(tmp_path):
    generate.main(["embedding", *SMALL["embedding"], "--out", str(tmp_path)])
    truth = json.loads((tmp_path / "truth.json").read_text())
    assert truth["embedding_dim"] == 64
    assert truth["relations"] == ["related_to", "part_of", "mentions"]
    with open(tmp_path / "edges.csv", newline="") as f:
        assert next(csv.reader(f)) == ["src", "dst", "rel"]
