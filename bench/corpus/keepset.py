"""The port to keepset: its CLI at the version `manifest.json` pins,
installed with `npm ci` from the committed lock (`keepset_cli/`), so every
package it runs is pinned by its integrity hash.

`keepset fetch` downloads a dataset's source (from its pinned Wayback
snapshot, the closest capture, or the live URL), applies the recipe and
the category's crosswalk, and writes the graph/v1 document with the
provenance of the bytes it read beside it. `--no-archive` keeps it from
asking the Internet Archive to capture a live URL, a side effect a
benchmark run should not have."""

import json
import math
import os
import shutil
import subprocess
from pathlib import Path

LOCK = Path(__file__).resolve().parent / "keepset_cli"
DEFAULT_HEAP_MB = 2048
HEAP_MB_PER_MILLION_EDGES = 2100  # 7.8 GB resident for roadnet-tx's 3.84M edges at 0.57.0
HEAP_HEADROOM_MB = 1024


def heap_mb(edges):
    """Node's heap for a fetch of `edges` edges: keepset holds the whole
    document and its crosswalk copies every edge (ExaDev/crossmap#57), so a
    graph of a few million edges fails at Node's default heap."""
    return max(DEFAULT_HEAP_MB, HEAP_HEADROOM_MB + math.ceil(edges * HEAP_MB_PER_MILLION_EDGES / 1_000_000))


class Keepset:
    def __init__(self, root, version, run=subprocess.run):
        self.directory = Path(root) / f"keepset-{version}"
        self._run = run

    def _install(self):
        if (self.directory / "node_modules" / ".bin").exists():
            return
        self.directory.mkdir(parents=True, exist_ok=True)
        for name in ("package.json", "package-lock.json"):
            shutil.copyfile(LOCK / name, self.directory / name)
        self._run(["npm", "ci", "--no-audit", "--no-fund", "--ignore-scripts"], cwd=self.directory, check=True,
                  capture_output=True, text=True)

    def fetch(self, name, out, heap_mb):
        """Fetches dataset `name` into the graph/v1 document `out`; returns
        keepset's provenance (`out` with `.provenance.json`)."""
        self._install()
        command = [str(self.directory / "node_modules" / ".bin" / "keepset"), "fetch", name, "--out", str(out),
                   "--no-archive"]
        env = {**os.environ, "NODE_OPTIONS": f"--max-old-space-size={heap_mb}"}
        self._run(command, env=env, check=True)
        return json.loads(Path(out).with_suffix(".provenance.json").read_text())
