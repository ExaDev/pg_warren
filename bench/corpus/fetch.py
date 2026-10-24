"""Fetch and pin: a dataset is fetched once through keepset into the cache
(`<root>/datasets/<name>/`), checked against the checksum it was first
fetched with (recorded in `pins.json` the first time), and converted to
its tables (`tables`). The graph/v1 document is deleted once converted, so
the cache holds the tables, `load.sql` and `dataset.json` (the tables'
summary and sha256, and keepset's provenance)."""

import hashlib
import json
import shutil

from . import graph_v1, keepset, tables


class PinMismatch(RuntimeError):
    pass


class NotEnoughMemory(RuntimeError):
    pass


def _sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as f:
        while block := f.read(1 << 20):
            digest.update(block)
    return digest.hexdigest()


def pin_of(provenance):
    return {"sha256": provenance["sha256"], "bytes": provenance["byteLength"], "url": provenance["fetchedUrl"]}


def directory_of(root, dataset):
    return root / "datasets" / dataset.name


def fetch(dataset, keepset_, root, record, available_mb=None):
    """The directory of `dataset`'s tables, fetched unless the cache already
    holds them at its pin. `record(name, pin)` records a first fetch's pin.
    A fetch whose heap (`keepset.heap_mb`) is more than `available_mb` is
    refused before it starts, rather than exhausting a shared machine."""
    directory = directory_of(root, dataset)
    described = directory / "dataset.json"
    if described.exists() and dataset.pin and pin_of(json.loads(described.read_text())["provenance"]) == dataset.pin:
        return directory
    heap = keepset.heap_mb(dataset.edges)
    if available_mb is not None and heap > available_mb:
        raise NotEnoughMemory(f"{dataset.name}: keepset needs about {heap} MB of heap, {available_mb} MB are "
                              "available (keepset holds the whole graph and copies its edges)")
    shutil.rmtree(directory, ignore_errors=True)
    raw = directory / "raw" / f"{dataset.name}.json"
    raw.parent.mkdir(parents=True)
    try:
        provenance = keepset_.fetch(dataset.name, raw, heap)
        pin = pin_of(provenance)
        if dataset.pin is None:
            record(dataset.name, pin)
        elif pin["sha256"] != dataset.pin["sha256"]:
            raise PinMismatch(f"{dataset.name}: keepset fetched {pin['sha256']} ({pin['bytes']} bytes from "
                              f"{pin['url']}), the pin is {dataset.pin['sha256']}")
        with graph_v1.open_graph(raw) as graph:
            summary = tables.write(graph, dataset, directory)
    except BaseException:
        shutil.rmtree(directory, ignore_errors=True)
        raise
    shutil.rmtree(raw.parent)
    checksums = {name: _sha256(directory / name) for name in ("nodes.csv", "edges.csv")}
    described.write_text(json.dumps({"tables": summary, "sha256": checksums, "provenance": provenance}, indent=1) + "\n")
    return directory
