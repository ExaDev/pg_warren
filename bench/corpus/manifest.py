"""The suite's datasets (`manifest.json`) and the checksum each was first
fetched with (`pins.json`, written by `record_pin`): a dataset is named by
its keepset id and sits in one tier, tiny (runnable in CI), medium (about
a million edges) or large."""

import json
from dataclasses import dataclass
from pathlib import Path

MANIFEST = Path(__file__).resolve().parent / "manifest.json"
TIERS = ("tiny", "medium", "large")
ID_KINDS = ("integer", "text")
FIELDS = ("tier", "domain", "ids", "directed", "weighted", "relations", "nodes", "edges", "licence", "why")


class ManifestError(ValueError):
    pass


@dataclass(frozen=True)
class Dataset:
    name: str
    tier: str
    domain: str
    ids: str
    directed: bool
    weighted: bool
    relations: bool
    nodes: int
    edges: int
    licence: str
    why: str
    pin: dict | None


@dataclass(frozen=True)
class Manifest:
    keepset: dict
    datasets: tuple[Dataset, ...]

    def dataset(self, name):
        for dataset in self.datasets:
            if dataset.name == name:
                return dataset
        raise ManifestError(f"unknown dataset: {name}")

    def select(self, names=(), tiers=()):
        """The datasets named, in that order, or else those of `tiers` (every
        tier if none), in the manifest's order."""
        if names:
            return [self.dataset(name) for name in names]
        return [d for d in self.datasets if not tiers or d.tier in tiers]


def read(path):
    path = Path(path)
    document = json.loads(path.read_text())
    pins = _pins(path)
    return Manifest(document["keepset"],
                    tuple(_dataset(name, spec, pins.get(name)) for name, spec in document["datasets"].items()))


def record_pin(path, name, pin):
    """Records `pin` (the sha256, size and URL of the bytes keepset fetched)
    for dataset `name` in the pins file beside manifest `path`."""
    pins = _pins(Path(path))
    pins[name] = pin
    _pins_path(Path(path)).write_text(json.dumps(dict(sorted(pins.items())), indent=1) + "\n")


def _pins_path(path):
    return path.parent / "pins.json"


def _pins(path):
    pins = _pins_path(path)
    return json.loads(pins.read_text()) if pins.exists() else {}


def _dataset(name, spec, pin):
    for field in FIELDS:
        if field not in spec:
            raise ManifestError(f"{name}: missing {field}")
    if spec["tier"] not in TIERS:
        raise ManifestError(f"{name}: tier {spec['tier']!r} is not one of {', '.join(TIERS)}")
    if spec["ids"] not in ID_KINDS:
        raise ManifestError(f"{name}: ids {spec['ids']!r} is not one of {', '.join(ID_KINDS)}")
    return Dataset(name, pin=pin, **{field: spec[field] for field in FIELDS})
